//! The single writer: one thread that owns the ledger, applies operations in the order they arrive and commits them
//! in batches (group commit).
//!
//! The thread takes every request waiting when it wakes, up to [`MAX_BATCH`], applies them to its memory one after
//! the other, so each sees the ones before it, and commits all that changed state as one durable batch. Only then
//! does any caller of that batch get its answer, refusals and reads included: an answer never reveals state that a
//! crash could still take back (ADR 0003, "No reads of state that is not yet durable").
//!
//! A failed commit leaves memory ahead of the disk. The writer then answers that batch with [`Error::Store`] and
//! everything after it with [`Error::Stopped`]; the server must restart, which reloads the state the disk holds.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::ledger::{Error, Ledger, Operation, Outcome, Record, Wallet, WalletId};
use crate::store::Store;

/// Most requests in one batch.
pub const MAX_BATCH: usize = 4096;

/// How the writer runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// How long an idempotency key is remembered: a retry within it gets the original outcome. Start-up loads only
    /// the records of this window.
    pub key_window: Duration,
}

impl Default for Config {
    /// A day, far longer than any client retries.
    fn default() -> Self {
        Self {
            key_window: Duration::from_secs(24 * 3600),
        }
    }
}

/// Counts since the writer started.
#[derive(Debug, Default)]
pub struct Stats {
    /// Operations committed (retries of a known key not included).
    pub operations: AtomicU64,
    /// Batches committed.
    pub batches: AtomicU64,
}

enum Request {
    Apply(Operation, mpsc::Sender<Result<Outcome, Error>>),
    Balance(WalletId, mpsc::Sender<Result<i64, Error>>),
    Stop,
}

/// An answer that is not there yet.
#[derive(Debug)]
pub struct Pending<T>(mpsc::Receiver<Result<T, Error>>);

impl<T> Pending<T> {
    /// Waits for the answer.
    ///
    /// # Errors
    ///
    /// The operation's own refusal, or [`Error::Stopped`] if the writer stopped before answering.
    pub fn wait(self) -> Result<T, Error> {
        self.0.recv().unwrap_or(Err(Error::Stopped))
    }

    /// The answer if it has arrived, for a caller such as the tick that must not block.
    pub fn try_take(&self) -> Option<Result<T, Error>> {
        match self.0.try_recv() {
            Ok(answer) => Some(answer),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(Error::Stopped)),
        }
    }
}

/// Submits requests to the writer; cheap to clone and usable from any thread.
#[derive(Debug, Clone)]
pub struct Handle {
    requests: mpsc::Sender<Request>,
    stats: Arc<Stats>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Apply(op, _) => write!(f, "Apply({op:?})"),
            Self::Balance(w, _) => write!(f, "Balance({w:?})"),
            Self::Stop => write!(f, "Stop"),
        }
    }
}

impl Handle {
    /// Submits `operation`; the answer comes once its batch is durable.
    pub fn submit(&self, operation: Operation) -> Pending<Outcome> {
        let (tx, rx) = mpsc::channel();
        let _ = self.requests.send(Request::Apply(operation, tx));
        Pending(rx)
    }

    /// Asks for the balance of `wallet`, as of the durable state when the answer leaves.
    pub fn balance(&self, wallet: WalletId) -> Pending<i64> {
        let (tx, rx) = mpsc::channel();
        let _ = self.requests.send(Request::Balance(wallet, tx));
        Pending(rx)
    }

    /// The writer's counts.
    #[must_use]
    pub fn stats(&self) -> &Stats {
        &self.stats
    }
}

/// The writer thread and a handle to it.
#[derive(Debug)]
pub struct Writer {
    handle: Handle,
    thread: Option<JoinHandle<()>>,
}

impl Writer {
    /// Loads the state from `store` and starts the writer thread.
    ///
    /// # Errors
    ///
    /// The store cannot be loaded, or the loaded state breaks a ledger rule.
    pub fn start(mut store: impl Store + Send + 'static, config: Config) -> Result<Self, Error> {
        let since = now_ms().saturating_sub(millis(config.key_window));
        let loaded = store.load(since)?;
        let ledger = Ledger::load(loaded.wallets, loaded.last_operation, loaded.recent)?;
        let (tx, rx) = mpsc::channel();
        let stats = Arc::new(Stats::default());
        let thread_stats = stats.clone();
        let thread = std::thread::Builder::new()
            .name("writer".to_owned())
            .spawn(move || run(store, ledger, &rx, config, &thread_stats))
            .map_err(|e| Error::Store(e.to_string()))?;
        Ok(Self {
            handle: Handle { requests: tx, stats },
            thread: Some(thread),
        })
    }

    /// A handle for submitting.
    #[must_use]
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// Answers what was submitted before this call, then stops the thread and waits for it.
    pub fn stop(mut self) {
        let _ = self.handle.requests.send(Request::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, millis)
}

/// An answer held until its batch is durable.
enum Answer {
    Apply(mpsc::Sender<Result<Outcome, Error>>, Result<Outcome, Error>),
    Balance(mpsc::Sender<Result<i64, Error>>, Result<i64, Error>),
}

fn run(
    mut store: impl Store,
    mut ledger: Ledger,
    requests: &mpsc::Receiver<Request>,
    config: Config,
    stats: &Stats,
) {
    let mut batch = Vec::with_capacity(MAX_BATCH);
    let mut answers = Vec::with_capacity(MAX_BATCH);
    let mut records: Vec<Record> = Vec::new();
    let mut wallets: Vec<Wallet> = Vec::new();
    let mut stopping = false;
    let mut failed = false;
    while !stopping {
        let Ok(first) = requests.recv() else {
            return;
        };
        batch.push(first);
        while batch.len() < MAX_BATCH {
            match requests.try_recv() {
                Ok(request) => batch.push(request),
                Err(_) => break,
            }
        }
        let now = now_ms();
        ledger.forget_before(now.saturating_sub(millis(config.key_window)));
        for request in batch.drain(..) {
            match request {
                Request::Apply(_, reply) if failed => {
                    let _ = reply.send(Err(Error::Stopped));
                }
                Request::Balance(_, reply) if failed => {
                    let _ = reply.send(Err(Error::Stopped));
                }
                Request::Apply(operation, reply) => {
                    let result = ledger.apply(operation, now).map(|(outcome, change)| {
                        if let Some((record, changed)) = change {
                            records.push(record);
                            wallets.extend(changed);
                        }
                        outcome
                    });
                    answers.push(Answer::Apply(reply, result));
                }
                Request::Balance(wallet, reply) => {
                    answers.push(Answer::Balance(reply, ledger.balance(wallet)));
                }
                Request::Stop => stopping = true,
            }
        }
        let committed = if records.is_empty() {
            Ok(())
        } else {
            store.commit(&records, &dedup_last(&mut wallets))
        };
        match committed {
            Ok(()) => {
                if !records.is_empty() {
                    stats
                        .operations
                        .fetch_add(records.len() as u64, Ordering::Relaxed);
                    stats.batches.fetch_add(1, Ordering::Relaxed);
                }
                for answer in answers.drain(..) {
                    match answer {
                        Answer::Apply(reply, result) => {
                            let _ = reply.send(result);
                        }
                        Answer::Balance(reply, result) => {
                            let _ = reply.send(result);
                        }
                    }
                }
            }
            Err(e) => {
                // Memory is now ahead of the disk: nothing of this batch is known to be durable, and nothing after
                // it may be built on it.
                failed = true;
                for answer in answers.drain(..) {
                    match answer {
                        Answer::Apply(reply, _) => {
                            let _ = reply.send(Err(e.clone()));
                        }
                        Answer::Balance(reply, _) => {
                            let _ = reply.send(Err(e.clone()));
                        }
                    }
                }
            }
        }
        records.clear();
        wallets.clear();
    }
}

/// The last state of each wallet changed in a batch, once each.
fn dedup_last(wallets: &mut [Wallet]) -> Vec<Wallet> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<Wallet> = wallets
        .iter()
        .rev()
        .filter(|w| seen.insert(w.id))
        .copied()
        .collect();
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Kind;
    use crate::store::Loaded;

    /// A store in memory that can be told to fail.
    #[derive(Default)]
    struct Memory {
        commits: Arc<AtomicU64>,
        fail: bool,
    }

    impl Store for Memory {
        fn load(&mut self, _: u64) -> Result<Loaded, Error> {
            Ok(Loaded::default())
        }

        fn commit(&mut self, _: &[Record], _: &[Wallet]) -> Result<(), Error> {
            if self.fail {
                return Err(Error::Store("disk full".into()));
            }
            self.commits.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn a_wallet_changed_twice_in_a_batch_is_stored_once_with_its_last_balance() {
        let w = |id, balance| Wallet {
            id: WalletId(id),
            kind: Kind::Character,
            balance,
        };
        let mut changed = vec![w(1, 5), w(2, 3), w(1, 9)];
        assert_eq!(dedup_last(&mut changed), vec![w(2, 3), w(1, 9)]);
    }

    #[test]
    fn a_failed_commit_stops_the_writer() {
        let writer = Writer::start(
            Memory {
                fail: true,
                ..Memory::default()
            },
            Config::default(),
        )
        .expect("start");
        let h = writer.handle();
        let open = h.submit(Operation::OpenWallet {
            key: "a".into(),
            kind: Kind::System,
        });
        assert_eq!(open.wait(), Err(Error::Store("disk full".into())));
        // Wallet 0 exists in the writer's memory but not on disk: it must not be visible.
        assert_eq!(h.balance(WalletId(0)).wait(), Err(Error::Stopped));
        writer.stop();
    }

    #[test]
    fn refusals_and_reads_need_no_commit() {
        let commits = Arc::new(AtomicU64::new(0));
        let writer = Writer::start(
            Memory {
                commits: commits.clone(),
                fail: false,
            },
            Config::default(),
        )
        .expect("start");
        let h = writer.handle();
        assert_eq!(
            h.balance(WalletId(5)).wait(),
            Err(Error::UnknownWallet(WalletId(5)))
        );
        assert_eq!(commits.load(Ordering::Relaxed), 0);
        writer.stop();
    }
}
