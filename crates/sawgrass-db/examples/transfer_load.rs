//! Load test for the money ledger: many concurrent [`transfer`] calls against a real PostgreSQL database.
//!
//! ```sh
//! createdb sawgrass_load
//! cargo run -p sawgrass-db --release --example transfer_load -- \
//!     postgres://postgres@localhost/sawgrass_load 64 10 uniform
//! ```
//!
//! Arguments: database URL (an empty database; the migrations run first), concurrent clients, seconds, and the
//! pattern: `uniform` moves money between random character wallets, `hot` has every transfer pay one shop wallet
//! (one contended row, like a popular store). It prints transfers per second and latency percentiles. Results and
//! the setup they were taken on are in `docs/research/spacetimedb-vs-postgresql/`.

use std::time::{Duration, Instant};

use sawgrass_db::{
    Transfer, TransferOutcome, WalletId, connect, create_character, migrate, system_wallet, transfer,
};

const CHARACTERS: usize = 10_000;
const STARTING_BALANCE: i64 = 100_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pattern {
    Uniform,
    Hot,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, url, clients, seconds, pattern] = args.as_slice() else {
        return Err("usage: transfer_load <database-url> <clients> <seconds> <uniform|hot>".into());
    };
    let clients: usize = clients.parse()?;
    let seconds: u64 = seconds.parse()?;
    let pattern = match pattern.as_str() {
        "uniform" => Pattern::Uniform,
        "hot" => Pattern::Hot,
        other => return Err(format!("unknown pattern {other}").into()),
    };

    let pool = connect(url, u32::try_from(clients)?.max(4)).await?;
    migrate(&pool).await?;

    let treasury = system_wallet(&pool, "treasury").await?;
    let shop = system_wallet(&pool, "shop").await?;
    let mut wallets = Vec::with_capacity(CHARACTERS);
    for i in 0..CHARACTERS {
        let character = create_character(&pool, "Load", &format!("Bot{i}")).await?;
        let key = format!("seed-{}", character.bank.0);
        transfer(
            &pool,
            &Transfer {
                idempotency_key: &key,
                from: treasury,
                to: character.bank,
                amount: STARTING_BALANCE,
                reason: "seed",
            },
        )
        .await?;
        wallets.push(character.bank);
    }
    let wallets: &'static [WalletId] = Vec::leak(wallets);

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(clients);
    for client in 0..clients {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            let mut rng =
                XorShift(0x9E37_79B9_7F4A_7C15 ^ (client as u64 + 1).wrapping_mul(0xA24B_AED4_963E_E407));
            let mut latencies = Vec::new();
            let mut n = 0u64;
            while Instant::now() < deadline {
                let from = wallets[rng.below(wallets.len())];
                let to = match pattern {
                    Pattern::Hot => shop,
                    Pattern::Uniform => loop {
                        let to = wallets[rng.below(wallets.len())];
                        if to != from {
                            break to;
                        }
                    },
                };
                let key = format!("load-{client}-{n}");
                n += 1;
                let request = Transfer {
                    idempotency_key: &key,
                    from,
                    to,
                    amount: 1 + i64::try_from(rng.below(100)).unwrap_or(0),
                    reason: "load",
                };
                let begun = Instant::now();
                match transfer(&pool, &request).await {
                    Ok(TransferOutcome::Applied(_)) => latencies.push(begun.elapsed()),
                    Ok(TransferOutcome::AlreadyApplied(_)) => return Err(format!("key {key} reused")),
                    Err(error) => return Err(error.to_string()),
                }
            }
            Ok(latencies)
        }));
    }
    let mut latencies = Vec::new();
    for task in tasks {
        latencies.extend(task.await??);
    }
    let elapsed = started.elapsed();
    latencies.sort_unstable();

    let at = |q: f64| {
        let index = ((latencies.len() as f64 - 1.0) * q).round() as usize;
        latencies.get(index).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
    };
    println!(
        "pattern={pattern:?} clients={clients} transfers={} tps={:.0} p50_ms={:.2} p99_ms={:.2} max_ms={:.2}",
        latencies.len(),
        latencies.len() as f64 / elapsed.as_secs_f64(),
        at(0.50),
        at(0.99),
        at(1.0),
    );
    pool.close().await;
    Ok(())
}

/// A small deterministic generator, so the example needs no extra dependency.
struct XorShift(u64);

impl XorShift {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}
