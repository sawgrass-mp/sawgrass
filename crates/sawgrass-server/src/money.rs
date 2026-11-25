//! Players' money in the ledger's single writer (task DB-8, ADR 0003).
//!
//! With a ledger, every player who connects gets a character wallet. The wallet is funded with a starting amount from
//! the server's system wallet, so money only moves between wallets and every balance sums to zero. A client pays
//! another player with [`ClientEvent::Pay`](sawgrass_protocol::game::ClientEvent::Pay). That is the load-test path
//! for money until resources move it through the scripting API, so the server takes the event only when started
//! for it (`--money-events`).
//!
//! The ledger answers only after the batch that holds an operation is on disk. Connection tasks await those answers
//! through the writer's callbacks, holding no thread while a batch commits; the tick thread only keeps the map
//! from entity ids to wallets that payments look up.
//!
//! A retried key gets the first answer even after the payee has left (task DB-9): each payer's last
//! [`RECENT_KEYS`] payments remember the payee's wallet, so the retry reaches the ledger as the same operation and
//! the ledger answers it from its key window. Only a new key to a player who is not connected is `UnknownPayee`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

use sawgrass_protocol::game::PayResult;
use sawgrass_store::{Error, Handle, Kind, Operation, Outcome, WalletId};
use tokio::sync::oneshot;

/// Payments per payer whose payee is remembered for retries. A client retries a payment it got no answer for,
/// so only its latest keys need this; older retries of a payee who has left are answered `UnknownPayee`.
pub const RECENT_KEYS: usize = 64;

/// A payment a payer made recently: its key, the entity it paid and that entity's wallet.
#[derive(Debug, Clone, Copy)]
struct Paid {
    key: u64,
    to: u32,
    payee: WalletId,
}

/// Players' wallets, and how payments between them reach the ledger.
#[derive(Debug)]
pub struct Money {
    ledger: Handle,
    treasury: WalletId,
    start_cash: i64,
    /// Distinguishes this run's keys from earlier runs': connection ids restart at 0.
    run: u64,
    payees: RwLock<HashMap<u32, WalletId>>,
    /// Each connected payer's latest payments, newest last, at most [`RECENT_KEYS`].
    recent: Mutex<HashMap<WalletId, VecDeque<Paid>>>,
}

impl Money {
    /// Opens (or, after a restart, finds) the system wallet that funds new players with `start_cash` each. `run`
    /// must differ between runs of the server within the ledger's key window; the start time in milliseconds does.
    pub fn open(ledger: Handle, start_cash: i64, run: u64) -> Result<Arc<Self>, Error> {
        let treasury = match ledger
            .submit(Operation::OpenWallet {
                key: "treasury".to_owned(),
                kind: Kind::System,
            })
            .wait()?
        {
            Outcome::Opened { wallet, .. } => wallet,
            Outcome::Transferred { .. } => {
                return Err(Error::Corrupt("treasury key names a transfer".to_owned()));
            }
        };
        Ok(Arc::new(Self {
            ledger,
            treasury,
            start_cash,
            run,
            payees: RwLock::new(HashMap::new()),
            recent: Mutex::new(HashMap::new()),
        }))
    }

    /// Opens connection `connection`'s wallet and funds it, once both are on disk.
    pub(crate) async fn open_wallet(&self, connection: u32) -> Result<WalletId, Error> {
        let opened = submit(
            &self.ledger,
            Operation::OpenWallet {
                key: format!("player/{}/{connection}", self.run),
                kind: Kind::Character,
            },
        );
        let wallet = match opened.await? {
            Outcome::Opened { wallet, .. } => wallet,
            Outcome::Transferred { .. } => {
                return Err(Error::Corrupt("wallet key names a transfer".to_owned()));
            }
        };
        if self.start_cash > 0 {
            submit(
                &self.ledger,
                Operation::Transfer {
                    key: format!("start/{}/{connection}", self.run),
                    from: self.treasury,
                    to: wallet,
                    amount: self.start_cash,
                },
            )
            .await?;
        }
        Ok(wallet)
    }

    /// Pays `amount` from `from` to the player with entity id `to`, once per client `key`, and returns the result
    /// with the payer's balance.
    pub(crate) async fn pay(&self, from: WalletId, key: u64, to: u32, amount: u32) -> (PayResult, i64) {
        let payee = self
            .payees
            .read()
            .ok()
            .and_then(|p| p.get(&to).copied())
            .or_else(|| self.paid_before(from, key, to));
        let Some(payee) = payee else {
            return (
                PayResult::UnknownPayee,
                balance(&self.ledger, from).await.unwrap_or(0),
            );
        };
        // Both requests go in together: the balance is read after the transfer, in the same batch.
        let paid = submit(
            &self.ledger,
            Operation::Transfer {
                // Keys are per payer, so one client cannot spend another's key.
                key: format!("pay/{}/{key}", from.0),
                from,
                to: payee,
                amount: i64::from(amount),
            },
        );
        let after = balance(&self.ledger, from);
        let result = match paid.await {
            Ok(_) => {
                // Only a payment the ledger applied holds its key; a refused one leaves the key free, so a retry
                // of it is a new payment and must find the payee connected.
                self.remember(from, Paid { key, to, payee });
                PayResult::Done
            }
            Err(e) => result_of(&e),
        };
        (result, after.await.unwrap_or(0))
    }

    /// The payee's wallet if `from` already paid entity `to` with `key`.
    fn paid_before(&self, from: WalletId, key: u64, to: u32) -> Option<WalletId> {
        let recent = self.recent.lock().ok()?;
        recent
            .get(&from)?
            .iter()
            .rev()
            .find(|p| p.key == key && p.to == to)
            .map(|p| p.payee)
    }

    fn remember(&self, from: WalletId, paid: Paid) {
        let Ok(mut recent) = self.recent.lock() else {
            return;
        };
        let mine = recent.entry(from).or_default();
        if mine.iter().any(|p| p.key == paid.key && p.to == paid.to) {
            return;
        }
        if mine.len() == RECENT_KEYS {
            mine.pop_front();
        }
        mine.push_back(paid);
    }

    /// Whether payments to entity `wire` are taken now: it is connected with a wallet.
    #[must_use]
    pub fn knows_payee(&self, wire: u32) -> bool {
        self.payees.read().is_ok_and(|p| p.contains_key(&wire))
    }

    /// Records that entity `wire` pays and is paid through `wallet`.
    pub(crate) fn register(&self, wire: u32, wallet: WalletId) {
        if let Ok(mut payees) = self.payees.write() {
            payees.insert(wire, wallet);
        }
    }

    /// Forgets entity `wire`: new payments to it are refused until it is registered again. Its own recent
    /// payments are forgotten too, since a wallet belongs to one connection and cannot pay again.
    pub(crate) fn unregister(&self, wire: u32) {
        let wallet = self.payees.write().ok().and_then(|mut p| p.remove(&wire));
        if let (Some(wallet), Ok(mut recent)) = (wallet, self.recent.lock()) {
            recent.remove(&wallet);
        }
    }

    /// The balance of `wallet`, after every operation submitted before this call. Blocks.
    pub fn balance(&self, wallet: WalletId) -> Result<i64, Error> {
        self.ledger.balance(wallet).wait()
    }

    /// The ledger handle, for its stats.
    #[must_use]
    pub fn ledger(&self) -> &Handle {
        &self.ledger
    }
}

fn result_of(error: &Error) -> PayResult {
    match error {
        Error::InsufficientFunds { .. } => PayResult::InsufficientFunds,
        Error::UnknownWallet(_) => PayResult::UnknownPayee,
        Error::InvalidAmount(_) | Error::SameWallet(_) | Error::Overflow(_) | Error::KeyReused(_) => {
            PayResult::Invalid
        }
        Error::Store(_) | Error::Corrupt(_) | Error::Stopped => PayResult::Failed,
    }
}

/// Submits `operation` and returns its answer without holding a thread while the batch commits.
fn submit(ledger: &Handle, operation: Operation) -> impl Future<Output = Result<Outcome, Error>> + use<> {
    let (tx, rx) = oneshot::channel();
    ledger.submit_then(operation, move |answer| {
        let _ = tx.send(answer);
    });
    async move { rx.await.unwrap_or(Err(Error::Stopped)) }
}

/// The balance of `wallet` once everything submitted before is durable, without holding a thread.
fn balance(ledger: &Handle, wallet: WalletId) -> impl Future<Output = Result<i64, Error>> + use<> {
    let (tx, rx) = oneshot::channel();
    ledger.balance_then(wallet, move |answer| {
        let _ = tx.send(answer);
    });
    async move { rx.await.unwrap_or(Err(Error::Stopped)) }
}
