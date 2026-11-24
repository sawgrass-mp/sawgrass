//! The DB-4 money ledger on RocksDB (`../../spacetimedb-vs-postgresql/embedded`), with its callers on Tokio or
//! compio and each allocator.
//!
//! As in DB-4: one writer thread keeps balances and idempotency keys in memory, groups every waiting transfer (up
//! to 4,096) into one RocksDB write batch with `sync=true`, and replies only after that write returns. Callers are
//! async tasks that send a transfer and await the reply over a runtime-neutral oneshot channel.
//!
//! - `closed`: `callers` tasks each send a transfer, wait for its reply, and send the next: the most the writer
//!   can do, as in DB-4.
//! - `open RATE`: the callers together offer `RATE` transfers a second on a fixed schedule; latency is counted from
//!   when each transfer was due, so a stall shows up in full.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_channel::oneshot;

use crate::common::*;

const CHARACTERS: u64 = 10_000;
const STARTING_BALANCE: i64 = 100_000_000;
const MAX_BATCH: usize = 4096;

struct Request {
    key: String,
    from: u64,
    to: u64,
    amount: i64,
    reply: oneshot::Sender<u64>,
}

#[derive(Default)]
struct Ledger {
    balances: Vec<i64>,
    keys: HashMap<String, u64>,
    operations: u64,
}

fn record(key: &str, from: u64, to: u64, amount: i64, from_after: i64, to_after: i64) -> Vec<u8> {
    let mut p = Vec::with_capacity(48 + key.len());
    for v in [from, to] {
        p.extend_from_slice(&v.to_le_bytes());
    }
    for v in [amount, from_after, to_after] {
        p.extend_from_slice(&v.to_le_bytes());
    }
    p.extend_from_slice(key.as_bytes());
    p
}

fn open(dir: &Path) -> rocksdb::DB {
    let mut o = rocksdb::Options::default();
    o.create_if_missing(true);
    o.set_wal_recovery_mode(rocksdb::DBRecoveryMode::TolerateCorruptedTailRecords);
    o.set_paranoid_checks(true);
    rocksdb::DB::open(&o, dir).expect("open RocksDB")
}

fn writer(db: rocksdb::DB, rx: Receiver<Request>) {
    let mut ledger = Ledger {
        balances: vec![STARTING_BALANCE; CHARACTERS as usize],
        ..Ledger::default()
    };
    let mut sync = rocksdb::WriteOptions::default();
    sync.set_sync(true);
    let mut touched: HashMap<u64, i64> = HashMap::new();
    let mut pending: Vec<(oneshot::Sender<u64>, u64)> = Vec::with_capacity(MAX_BATCH);
    while let Ok(first) = rx.recv() {
        let mut batch = rocksdb::WriteBatch::default();
        let mut next = Some(first);
        while let Some(req) = next.take() {
            if let Some(&op) = ledger.keys.get(&req.key) {
                pending.push((req.reply, op));
            } else {
                let (f, t) = (req.from as usize, req.to as usize);
                let from_after = ledger.balances[f] - req.amount;
                if from_after >= 0 {
                    let to_after = ledger.balances[t] + req.amount;
                    ledger.balances[f] = from_after;
                    ledger.balances[t] = to_after;
                    ledger.operations += 1;
                    let op = ledger.operations;
                    batch.put(
                        [&b"o"[..], &op.to_be_bytes()].concat(),
                        record(&req.key, req.from, req.to, req.amount, from_after, to_after),
                    );
                    touched.insert(req.from, from_after);
                    touched.insert(req.to, to_after);
                    ledger.keys.insert(req.key, op);
                    pending.push((req.reply, op));
                } else {
                    pending.push((req.reply, 0));
                }
            }
            if pending.len() < MAX_BATCH {
                next = rx.try_recv().ok();
            }
        }
        for (id, balance) in touched.drain() {
            batch.put([&b"w"[..], &id.to_be_bytes()].concat(), balance.to_le_bytes());
        }
        db.write_opt(batch, &sync).expect("commit");
        for (reply, op) in pending.drain(..) {
            let _ = reply.send(op);
        }
    }
}

/// One caller: sends transfers and records each one's latency until `deadline`. With `every`, a transfer is due
/// every `every` from `start`, and its latency counts from when it was due.
async fn caller<S, F>(
    id: u64,
    tx: Sender<Request>,
    deadline: Instant,
    every: Option<Duration>,
    sleep: S,
) -> Vec<u64>
where
    S: Fn(Duration) -> F,
    F: std::future::Future<Output = ()>,
{
    let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ (id + 1).wrapping_mul(0xA24B_AED4_963E_E407);
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut latencies = Vec::new();
    let start = Instant::now();
    let mut n = 0u64;
    loop {
        let due = match every {
            Some(every) => {
                // Spread the callers over the interval so they do not all fire together.
                let due = start + every * n as u32 + every.mul_f64((id % 64) as f64 / 64.0);
                sleep(due.saturating_duration_since(Instant::now())).await;
                due
            }
            None => Instant::now(),
        };
        if due >= deadline {
            break;
        }
        let from = next() % CHARACTERS;
        let to = (from + 1 + next() % (CHARACTERS - 1)) % CHARACTERS;
        let (reply, answer) = oneshot::channel();
        tx.send(Request {
            key: format!("load-{id}-{n}"),
            from,
            to,
            amount: 1 + (next() % 100) as i64,
            reply,
        })
        .expect("writer");
        n += 1;
        answer.await.expect("reply");
        latencies.push(due.elapsed().as_nanos() as u64);
    }
    latencies
}

/// Runs the ledger in a fresh directory `dir` for `secs` seconds.
pub fn ledger(runtime: &str, dir: &Path, callers: usize, rate: Option<u64>, secs: u64) {
    let _ = std::fs::remove_dir_all(dir);
    let db = open(dir);
    let (tx, rx) = mpsc::channel::<Request>();
    let writer_thread = std::thread::spawn(move || writer(db, rx));
    let every = rate.map(|r| Duration::from_secs(1).mul_f64(callers as f64 / r as f64));
    let started = Instant::now();
    let cpu0 = process_cpu();
    let deadline = started + Duration::from_secs(secs);
    let all: Arc<Mutex<Vec<u64>>> = Arc::default();
    match runtime {
        "tokio" => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("rt");
            rt.block_on(async {
                let tasks: Vec<_> = (0..callers as u64)
                    .map(|id| {
                        let tx = tx.clone();
                        tokio::spawn(caller(id, tx, deadline, every, tokio::time::sleep))
                    })
                    .collect();
                for t in tasks {
                    // Await before locking: a lock held across the await would block the other runtime threads.
                    let latencies = t.await.expect("caller");
                    all.lock().expect("all").extend(latencies);
                }
            });
        }
        "compio" => {
            // Two threads, half of the callers each, as Tokio's two workers.
            let threads: Vec<_> = (0..2u64)
                .map(|t| {
                    let (tx, all) = (tx.clone(), Arc::clone(&all));
                    std::thread::spawn(move || {
                        let rt = compio::runtime::Runtime::new().expect("compio");
                        rt.block_on(async move {
                            let tasks: Vec<_> = (0..callers as u64)
                                .filter(|id| id % 2 == t)
                                .map(|id| {
                                    compio::runtime::spawn(caller(
                                        id,
                                        tx.clone(),
                                        deadline,
                                        every,
                                        compio::time::sleep,
                                    ))
                                })
                                .collect();
                            for task in tasks {
                                // Await before locking, or this thread would hold the lock while the other one waits for it
                                // and its callers would not run until this thread's are done.
                                let latencies = task.await.expect("caller");
                                all.lock().expect("all").extend(latencies);
                            }
                        });
                    })
                })
                .collect();
            for t in threads {
                t.join().expect("thread");
            }
        }
        other => panic!("unknown runtime {other}"),
    }
    drop(tx);
    let elapsed = started.elapsed();
    writer_thread.join().expect("writer");
    let cpu = (process_cpu() - cpu0).as_secs_f64() / elapsed.as_secs_f64();
    let mut lat = std::mem::take(&mut *all.lock().expect("all"));
    lat.sort_unstable();
    let ms = |q: f64| percentile(&lat, q) as f64 / 1e6;
    println!(
        "ledger runtime={runtime} allocator={} callers={callers} mode={} transfers={} tps={:.0} p50_ms={:.3} \
         p99_ms={:.3} max_ms={:.3} cpu_cores={cpu:.2} rss_mb={:.1}",
        allocator(),
        rate.map_or("closed".to_string(), |r| format!("open-{r}")),
        lat.len(),
        lat.len() as f64 / elapsed.as_secs_f64(),
        ms(0.5),
        ms(0.99),
        ms(1.0),
        process_rss() as f64 / 1048576.0,
    );
    flush();
    let _ = std::fs::remove_dir_all(dir);
}
