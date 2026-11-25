//! Durable transfers a second through the writer on RocksDB, with `CLIENTS` callers that each wait for their
//! answer before the next transfer, as the DB-2 and DB-4 measurements did.
//!
//! ```text
//! cargo run --release -p sawgrass-store --example load -- DIR [CLIENTS] [SECONDS]
//! ```
//!
//! DIR must not exist yet; it is left behind for inspection.

use std::time::{Duration, Instant};

use sawgrass_store::{Config, Kind, Operation, Outcome, RocksStore, WalletId, Writer};

const WALLETS: u64 = 10_000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().ok_or("usage: load DIR [CLIENTS] [SECONDS]")?;
    let clients: u64 = args.next().map_or(Ok(64), |a| a.parse())?;
    let seconds: u64 = args.next().map_or(Ok(10), |a| a.parse())?;
    let writer = Writer::start(RocksStore::open(dir.as_ref())?, Config::default())?;
    let h = writer.handle();
    let open = |key: String, kind| match h.submit(Operation::OpenWallet { key, kind }).wait() {
        Ok(Outcome::Opened { wallet, .. }) => Ok(wallet),
        other => Err(format!("{other:?}")),
    };
    let treasury = open("treasury".into(), Kind::System)?;
    // Open and fund the wallets from many callers at once, so setup is grouped too.
    std::thread::scope(|s| {
        for c in 0..64 {
            let h = h.clone();
            s.spawn(move || {
                for w in (c..WALLETS).step_by(64) {
                    let Ok(Outcome::Opened { wallet, .. }) = h
                        .submit(Operation::OpenWallet {
                            key: format!("w{w}"),
                            kind: Kind::Character,
                        })
                        .wait()
                    else {
                        continue;
                    };
                    let mint = Operation::Transfer {
                        key: format!("mint{w}"),
                        from: treasury,
                        to: wallet,
                        amount: 1_000_000,
                    };
                    let _ = h.submit(mint).wait();
                }
            });
        }
    });
    let (ops0, batches0) = (
        h.stats().operations.load(std::sync::atomic::Ordering::Relaxed),
        h.stats().batches.load(std::sync::atomic::Ordering::Relaxed),
    );
    let start = Instant::now();
    let stop = start + Duration::from_secs(seconds);
    let mut latencies: Vec<Duration> = std::thread::scope(|s| {
        let tasks: Vec<_> = (0..clients)
            .map(|c| {
                let h = h.clone();
                s.spawn(move || {
                    let mut lat = Vec::new();
                    let mut x = c.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
                    for n in 0u64.. {
                        let t = Instant::now();
                        if t >= stop {
                            break;
                        }
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        let (from, to) = (1 + x % WALLETS, 1 + (x >> 32) % WALLETS);
                        if from == to {
                            continue;
                        }
                        let op = Operation::Transfer {
                            key: format!("{c}-{n}"),
                            from: WalletId(from),
                            to: WalletId(to),
                            amount: 1,
                        };
                        let _ = h.submit(op).wait();
                        lat.push(t.elapsed());
                    }
                    lat
                })
            })
            .collect();
        tasks
            .into_iter()
            .flat_map(|t| t.join().unwrap_or_default())
            .collect()
    });
    let elapsed = start.elapsed().as_secs_f64();
    latencies.sort();
    let q = |p: f64| {
        latencies
            .get(((latencies.len() as f64 * p) as usize).min(latencies.len().saturating_sub(1)))
            .copied()
            .unwrap_or_default()
    };
    let ops = h.stats().operations.load(std::sync::atomic::Ordering::Relaxed) - ops0;
    let batches = h.stats().batches.load(std::sync::atomic::Ordering::Relaxed) - batches0;
    println!(
        "{clients} callers, {seconds} s: {:.0} durable transfers/s, {:.1} per batch, latency p50 {:.2} ms p99 {:.2} ms max {:.2} ms",
        ops as f64 / elapsed,
        ops as f64 / batches.max(1) as f64,
        q(0.5).as_secs_f64() * 1e3,
        q(0.99).as_secs_f64() * 1e3,
        latencies.last().copied().unwrap_or_default().as_secs_f64() * 1e3
    );
    writer.stop();
    Ok(())
}
