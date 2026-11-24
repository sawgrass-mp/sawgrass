//! Load client for TigerBeetle on the same ledger workload: 10,000 character accounts that may not go below zero
//! (`DebitsMustNotExceedCredits`), a treasury and a shop that may, and N callers each sending one transfer and
//! waiting for the reply. One shared client, which batches concurrent requests itself.
//!
//! ```text
//! tb_ledger seed <address>
//! tb_ledger run <address> <clients> <seconds> <uniform|hot> [transfers per request, default 1]
//! tb_ledger check <address>
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use tigerbeetle as tb;

const CHARACTERS: u128 = 10_000;
const STARTING_BALANCE: u128 = 100_000_000;
const TREASURY: u128 = 1;
const SHOP: u128 = 2;
const FIRST_CHARACTER: u128 = 3;
const LEDGER: u32 = 1;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let mut future = Box::pin(future);
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(out) => return out,
            Poll::Pending => std::thread::park(),
        }
    }
}

fn transfer(id: u128, from: u128, to: u128, amount: u128) -> tb::Transfer {
    tb::Transfer {
        id,
        debit_account_id: from,
        credit_account_id: to,
        amount,
        ledger: LEDGER,
        code: 1,
        ..Default::default()
    }
}

fn seed(client: &tb::Client) -> Result<(), String> {
    let mut accounts = vec![
        tb::Account {
            id: TREASURY,
            ledger: LEDGER,
            code: 1,
            ..Default::default()
        },
        tb::Account {
            id: SHOP,
            ledger: LEDGER,
            code: 1,
            ..Default::default()
        },
    ];
    for i in 0..CHARACTERS {
        accounts.push(tb::Account {
            id: FIRST_CHARACTER + i,
            ledger: LEDGER,
            code: 2,
            flags: tb::AccountFlags::DebitsMustNotExceedCredits,
            ..Default::default()
        });
    }
    for chunk in accounts.chunks(100) {
        let results = block_on(client.create_accounts(chunk)).map_err(|e| format!("{e:?}"))?;
        if let Some(bad) = results
            .iter()
            .find(|r| r.status != tb::CreateAccountStatus::Created)
        {
            return Err(format!("create account: {:?}", bad.status));
        }
    }
    let seeds: Vec<tb::Transfer> = (0..CHARACTERS)
        .map(|i| transfer(1 << 100 | i, TREASURY, FIRST_CHARACTER + i, STARTING_BALANCE))
        .collect();
    for chunk in seeds.chunks(100) {
        let results = block_on(client.create_transfers(chunk)).map_err(|e| format!("{e:?}"))?;
        if let Some(bad) = results
            .iter()
            .find(|r| r.status != tb::CreateTransferStatus::Created)
        {
            return Err(format!("seed transfer: {:?}", bad.status));
        }
    }
    Ok(())
}

struct XorShift(u64);
impl XorShift {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn run(client: Arc<tb::Client>, clients: usize, seconds: u64, hot: bool, batch: usize) -> Result<(), String> {
    let run_id = u128::from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    );
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let started = Instant::now();
    let handles: Vec<_> = (0..clients)
        .map(|c| {
            let client = Arc::clone(&client);
            std::thread::spawn(move || {
                let mut rng =
                    XorShift(0x9E37_79B9_7F4A_7C15 ^ (c as u64 + 1).wrapping_mul(0xA24B_AED4_963E_E407));
                let mut latencies = Vec::new();
                let mut n: u128 = 0;
                let mut transfers = Vec::with_capacity(batch);
                while Instant::now() < deadline {
                    transfers.clear();
                    for _ in 0..batch {
                        let from = FIRST_CHARACTER + u128::from(rng.below(CHARACTERS as u64));
                        let to = if hot {
                            SHOP
                        } else {
                            loop {
                                let to = FIRST_CHARACTER + u128::from(rng.below(CHARACTERS as u64));
                                if to != from {
                                    break to;
                                }
                            }
                        };
                        // Unique per run, caller and request: the transfer id is TigerBeetle's idempotency key.
                        let id = run_id << 64 | (c as u128) << 40 | n;
                        n += 1;
                        transfers.push(transfer(id, from, to, 1 + u128::from(rng.below(100))));
                    }
                    let begun = Instant::now();
                    let results = block_on(client.create_transfers(&transfers)).expect("request");
                    assert!(
                        results
                            .iter()
                            .all(|r| r.status == tb::CreateTransferStatus::Created),
                        "transfer"
                    );
                    let took = begun.elapsed();
                    latencies.extend(std::iter::repeat_n(took, batch));
                }
                latencies
            })
        })
        .collect();
    let mut latencies: Vec<Duration> = handles
        .into_iter()
        .flat_map(|h| h.join().expect("caller"))
        .collect();
    let elapsed = started.elapsed();
    latencies.sort_unstable();
    let at = |q: f64| {
        let i = ((latencies.len() as f64 - 1.0) * q).round() as usize;
        latencies.get(i).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
    };
    println!(
        "engine=tigerbeetle pattern={} clients={clients} transfers={} tps={:.0} p50_ms={:.3} p99_ms={:.3}",
        if hot { "hot" } else { "uniform" },
        latencies.len(),
        latencies.len() as f64 / elapsed.as_secs_f64(),
        at(0.5),
        at(0.99)
    );
    Ok(())
}

/// Sums every account: debits and credits must balance, and no character account may be overdrawn.
fn check(client: &tb::Client) -> Result<(), String> {
    let ids: Vec<u128> = (1..FIRST_CHARACTER + CHARACTERS).collect();
    let (mut debits, mut credits) = (0u128, 0u128);
    for chunk in ids.chunks(100) {
        let accounts = block_on(client.lookup_accounts(chunk)).map_err(|e| format!("{e:?}"))?;
        for a in &accounts {
            debits += a.debits_posted;
            credits += a.credits_posted;
            if a.id >= FIRST_CHARACTER && a.debits_posted > a.credits_posted {
                return Err(format!("account {} overdrawn", a.id));
            }
        }
    }
    if debits != credits {
        return Err(format!("debits {debits} != credits {credits}"));
    }
    println!("engine=tigerbeetle transfers_posted_total={debits} invariants=ok");
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(address) = args.get(2) else {
        eprintln!("usage: tb_ledger seed|run|check <address> ...");
        std::process::exit(2);
    };
    let client = Arc::new(tb::Client::new(0, address).expect("client"));
    let result = match args[1].as_str() {
        "seed" => seed(&client),
        "run" if args.len() >= 6 => run(
            Arc::clone(&client),
            args[3].parse().unwrap_or(1),
            args[4].parse().unwrap_or(1),
            args[5] == "hot",
            args.get(6).and_then(|b| b.parse().ok()).unwrap_or(1),
        ),
        "check" => check(&client),
        _ => Err("usage: tb_ledger seed|run|check <address> ...".into()),
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
