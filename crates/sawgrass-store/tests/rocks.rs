//! The writer on RocksDB: state survives a reopen, concurrent callers share batches, a process killed under load
//! keeps every acknowledged operation, and a log damaged in the middle refuses to open.

use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sawgrass_store::{Config, Error, Handle, Kind, Operation, Outcome, RocksStore, WalletId, Writer};

/// A fresh directory under the system's temporary one, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("sawgrass-store-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn start(dir: &Path) -> Result<Writer, Error> {
    Writer::start(RocksStore::open(dir)?, Config::default())
}

fn open(h: &Handle, key: &str, kind: Kind) -> WalletId {
    match h
        .submit(Operation::OpenWallet {
            key: key.into(),
            kind,
        })
        .wait()
    {
        Ok(Outcome::Opened { wallet, .. }) => wallet,
        other => panic!("{other:?}"),
    }
}

fn pay(key: &str, from: WalletId, to: WalletId, amount: i64) -> Operation {
    Operation::Transfer {
        key: key.into(),
        from,
        to,
        amount,
    }
}

#[test]
fn state_and_keys_survive_a_reopen() {
    let dir = TempDir::new("reopen");
    let (treasury, a, first) = {
        let writer = start(&dir.0).expect("start");
        let h = writer.handle();
        let treasury = open(&h, "treasury", Kind::System);
        let a = open(&h, "a", Kind::Character);
        let first = h.submit(pay("mint", treasury, a, 500)).wait().expect("mint");
        writer.stop();
        (treasury, a, first)
    };
    let writer = start(&dir.0).expect("reopen");
    let h = writer.handle();
    assert_eq!(h.balance(a).wait(), Ok(500));
    assert_eq!(h.balance(treasury).wait(), Ok(-500));
    // The key is remembered across the restart: a retry changes nothing and gets the original answer.
    assert_eq!(h.submit(pay("mint", treasury, a, 500)).wait(), Ok(first));
    assert_eq!(h.balance(a).wait(), Ok(500));
    // New wallets and operations continue the ids.
    let b = open(&h, "b", Kind::Character);
    assert!(b.0 > a.0);
    match h.submit(pay("ab", a, b, 1)).wait() {
        Ok(Outcome::Transferred { operation }) => assert_eq!(operation.0, 5),
        other => panic!("{other:?}"),
    }
    writer.stop();
}

#[test]
fn concurrent_callers_share_batches() {
    let dir = TempDir::new("batches");
    let writer = start(&dir.0).expect("start");
    let h = writer.handle();
    let treasury = open(&h, "treasury", Kind::System);
    let wallets: Vec<_> = (0..64)
        .map(|i| open(&h, &format!("w{i}"), Kind::Character))
        .collect();
    let before = h.stats().batches.load(Ordering::Relaxed);
    std::thread::scope(|scope| {
        for (i, &w) in wallets.iter().enumerate() {
            let h = h.clone();
            scope.spawn(move || {
                for n in 0..50 {
                    h.submit(pay(&format!("{i}-{n}"), treasury, w, 1))
                        .wait()
                        .expect("pay");
                }
            });
        }
    });
    let batches = h.stats().batches.load(Ordering::Relaxed) - before;
    // 3,200 operations from 64 callers that each wait for their answer: far fewer flushes than operations.
    assert!(batches < 3200 / 4, "{batches} batches for 3,200 operations");
    assert_eq!(h.balance(treasury).wait(), Ok(-3200));
    writer.stop();
}

/// The child process of the crash test: transfers under load and prints each acknowledged one, until killed.
#[test]
#[ignore = "run by killed_under_load_keeps_every_acknowledged_operation in its own process"]
fn crash_child() {
    let Ok(dir) = std::env::var("SAWGRASS_STORE_CRASH_DIR") else {
        return;
    };
    let writer = start(Path::new(&dir)).expect("start");
    let h = writer.handle();
    let treasury = open(&h, "treasury", Kind::System);
    let wallets: Vec<_> = (0..16)
        .map(|i| open(&h, &format!("w{i}"), Kind::Character))
        .collect();
    let round = std::env::var("SAWGRASS_STORE_CRASH_ROUND").unwrap_or_default();
    std::thread::scope(|scope| {
        for t in 0..8u64 {
            let (h, wallets, round) = (h.clone(), &wallets, &round);
            scope.spawn(move || {
                for n in 0u64.. {
                    let from = if n % 4 == 0 {
                        treasury
                    } else {
                        wallets[((t + n) % 16) as usize]
                    };
                    let to = wallets[((t * 7 + n * 3 + 1) % 16) as usize];
                    let amount = 1 + ((t + n) % 9) as i64;
                    let key = format!("{round}-{t}-{n}");
                    // A refusal (not enough money, same wallet) is an answer too, and must be the same after the
                    // crash; only transfers that went through are checked.
                    if from != to && h.submit(pay(&key, from, to, amount)).wait().is_ok() {
                        let mut out = std::io::stdout().lock();
                        let _ = writeln!(out, "ack {key} {} {} {amount}", from.0, to.0);
                        let _ = out.flush();
                    }
                }
            });
        }
    });
}

#[test]
fn killed_under_load_keeps_every_acknowledged_operation() {
    let dir = TempDir::new("crash");
    for round in 0..3u64 {
        let mut child = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "crash_child",
                "--ignored",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env("SAWGRASS_STORE_CRASH_DIR", &dir.0)
            .env("SAWGRASS_STORE_CRASH_ROUND", round.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("child");
        let stdout = child.stdout.take().expect("stdout");
        let reader = std::thread::spawn(move || {
            BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
                .filter_map(|line| line.strip_prefix("ack ").map(str::to_owned))
                .collect::<Vec<_>>()
        });
        // Kill at a different moment each round, while transfers are in flight.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        std::thread::sleep(Duration::from_millis(400 + u64::from(nanos % 800)));
        child.kill().expect("SIGKILL");
        let _ = child.wait();
        let acks = reader.join().expect("reader");
        assert!(
            acks.len() > 100,
            "round {round}: only {} acknowledged",
            acks.len()
        );

        // Reopened, every acknowledged transfer is already there: submitting it again is an idempotent retry that
        // moves no money. One that was lost would move money now, or be refused.
        let writer = start(&dir.0).expect("reopen after the kill");
        let h = writer.handle();
        let balances = |h: &Handle| (0..17).map(|w| h.balance(WalletId(w)).wait()).collect::<Vec<_>>();
        let before = balances(&h);
        for ack in &acks {
            let f: Vec<&str> = ack.split(' ').collect();
            let parse = |s: &str| s.parse::<u64>().expect("number");
            let op = pay(
                f[0],
                WalletId(parse(f[1])),
                WalletId(parse(f[2])),
                parse(f[3]) as i64,
            );
            assert!(
                h.submit(op).wait().is_ok(),
                "round {round}: {ack} refused after the crash"
            );
        }
        assert_eq!(
            balances(&h),
            before,
            "round {round}: an acknowledged transfer was lost"
        );
        writer.stop();
    }
}

#[test]
fn a_log_damaged_in_the_middle_refuses_to_open() {
    let dir = TempDir::new("corrupt");
    // A killed writer leaves its operations in the write-ahead log, not yet in a table file.
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "crash_child",
            "--ignored",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env("SAWGRASS_STORE_CRASH_DIR", &dir.0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("child");
    std::thread::sleep(Duration::from_millis(800));
    child.kill().expect("SIGKILL");
    let _ = child.wait();
    let log = std::fs::read_dir(&dir.0)
        .expect("dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "log"))
        .max_by_key(|p| std::fs::metadata(p).map_or(0, |m| m.len()))
        .expect("a write-ahead log");
    let mut bytes = std::fs::read(&log).expect("read log");
    assert!(bytes.len() > 4096, "log of {} bytes", bytes.len());
    let middle = bytes.len() / 2;
    for b in &mut bytes[middle..middle + 16] {
        *b ^= 0xff;
    }
    std::fs::write(&log, &bytes).expect("write log");
    let started = Instant::now();
    match start(&dir.0) {
        Err(Error::Store(e)) => {
            eprintln!("refused: {e}");
            assert!(started.elapsed() < Duration::from_secs(30), "{e}");
        }
        Err(other) => panic!("refused, but as {other:?}"),
        Ok(_) => panic!("a damaged log was opened"),
    }
}
