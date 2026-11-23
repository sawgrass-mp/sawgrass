//! Load client for the SpacetimeDB ledger module: N WebSocket connections, each keeping `inflight` transfer
//! reducer calls outstanding, mirroring sawgrass-db's `transfer_load` example.
mod module_bindings;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use module_bindings::*;
use spacetimedb_sdk::{DbContext, Table};

const URI: &str = "http://127.0.0.1:3000";

fn connect(db: &str, confirmed: bool) -> DbConnection {
    let (tx, rx) = mpsc::channel();
    let conn = DbConnection::builder()
        .with_uri(URI)
        .with_database_name(db)
        .with_confirmed_reads(confirmed)
        .on_connect(move |_, _, _| {
            let _ = tx.send(());
        })
        .build()
        .expect("connect");
    conn.run_threaded();
    rx.recv_timeout(Duration::from_secs(10)).expect("connected");
    conn
}

struct XorShift(u64);
impl XorShift {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = args[1].clone();
    let clients: usize = args[2].parse().unwrap();
    let seconds: u64 = args[3].parse().unwrap();
    let hot = args[4] == "hot";
    let inflight: usize = args[5].parse().unwrap();
    let confirmed = args.get(6).map_or(true, |s| s != "unconfirmed");

    // Read the wallet ids once through a subscription.
    let (tx, rx) = mpsc::channel();
    let probe = connect(&db, confirmed);
    probe
        .subscription_builder()
        .on_applied(move |ctx| {
            let mut banks = Vec::new();
            let mut shop = 0;
            for w in ctx.db.wallet().iter() {
                match w.kind.as_str() {
                    "bank" => banks.push(w.id),
                    "shop" => shop = w.id,
                    _ => {}
                }
            }
            let _ = tx.send((banks, shop));
        })
        .subscribe("SELECT * FROM wallet");
    let (banks, shop) = rx.recv_timeout(Duration::from_secs(30)).expect("wallets");
    probe.disconnect().ok();
    assert!(!banks.is_empty() && shop != 0);
    let banks: &'static [u64] = Vec::leak(banks);
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    let conns: Vec<DbConnection> = (0..clients).map(|_| connect(&db, confirmed)).collect();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let started = Instant::now();
    let mut handles = Vec::new();
    for (client, conn) in conns.into_iter().enumerate() {
        handles.push(std::thread::spawn(move || {
            let mut rng = XorShift(
                0x9E37_79B9_7F4A_7C15 ^ (client as u64 + 1).wrapping_mul(0xA24B_AED4_963E_E407),
            );
            let (done_tx, done_rx) = mpsc::channel::<(Instant, bool)>();
            let mut n = 0u64;
            let send = |rng: &mut XorShift, n: &mut u64| {
                let from = banks[rng.below(banks.len())];
                let to = if hot {
                    shop
                } else {
                    loop {
                        let to = banks[rng.below(banks.len())];
                        if to != from {
                            break to;
                        }
                    }
                };
                let key = format!("load-{run}-{client}-{n}");
                *n += 1;
                let begun = Instant::now();
                let done = done_tx.clone();
                conn.reducers
                    .transfer_then(
                        key,
                        from,
                        to,
                        1 + rng.below(100) as i64,
                        "load".into(),
                        move |_, r| {
                            let ok = matches!(r, Ok(Ok(())));
                            let _ = done.send((begun, ok));
                        },
                    )
                    .expect("send");
            };
            for _ in 0..inflight {
                send(&mut rng, &mut n);
            }
            let mut latencies = Vec::new();
            let mut failed = 0u64;
            loop {
                let (begun, ok) = done_rx.recv().expect("reply");
                if ok {
                    latencies.push(begun.elapsed());
                } else {
                    failed += 1;
                }
                if Instant::now() < deadline {
                    send(&mut rng, &mut n);
                } else if (latencies.len() as u64 + failed) == n {
                    break;
                }
            }
            conn.disconnect().ok();
            (latencies, failed)
        }));
    }
    let mut latencies = Vec::new();
    let mut failed = 0;
    for h in handles {
        let (l, f) = h.join().unwrap();
        latencies.extend(l);
        failed += f;
    }
    let elapsed = started.elapsed();
    latencies.sort_unstable();
    let at = |q: f64| {
        let i = ((latencies.len() as f64 - 1.0) * q).round() as usize;
        latencies.get(i).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
    };
    println!(
        "pattern={} clients={clients} inflight={inflight} confirmed={confirmed} transfers={} failed={failed} tps={:.0} p50_ms={:.2} p99_ms={:.2} max_ms={:.2}",
        if hot { "Hot" } else { "Uniform" },
        latencies.len(),
        latencies.len() as f64 / elapsed.as_secs_f64(),
        at(0.5),
        at(0.99),
        at(1.0)
    );
}
