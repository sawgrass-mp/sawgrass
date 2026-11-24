//! The DB-1 money ledger held in the server's own memory, made durable by an append-only log on disk.
//!
//! One writer thread owns every wallet. Callers (the tick, scripts) send it transfer requests over a channel and
//! wait for the reply. The writer checks and applies each transfer, appends one record per operation to the log,
//! and replies only once the record is on disk, so an acknowledged transfer survives a crash. Each record holds a
//! whole operation (both entries) behind a length and a CRC, so a crash can only leave a torn last record, never
//! half a transfer. Recovery drops a torn last record and refuses to start on a bad record anywhere else, because
//! acknowledged transfers follow it.
//!
//! A retried request with the same idempotency key and the same parameters gets the original operation back, as
//! with `sawgrass_db::transfer`; the same key with other parameters is an error.
//!
//! Commands:
//!
//! ```text
//! inproc_ledger run <log> <clients> <seconds> <uniform|hot> <each|group|none>
//! inproc_ledger recover <log>
//! ```
//!
//! `each` flushes to disk after every transfer, `group` once per batch of whatever is waiting (group commit),
//! `none` never (the OS writes it back later; a crash loses recent transfers). `recover` replays a log, checks the
//! invariants and prints how long it took.
//!
//! Crash tests: with `SAWGRASS_ACK_MARKER` set, the writer records in `<log>.acked` the number of operations it has
//! made durable, after each flush and before it sends any reply. Every acknowledgement a caller ever received is
//! therefore covered by the marker, so after killing the process, `recover` must find at least that many
//! operations.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

const CHARACTERS: u64 = 10_000;
const STARTING_BALANCE: i64 = 100_000_000;
const TREASURY: u64 = 0;
const SHOP: u64 = 1;
const MAX_BATCH: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flush {
    Each,
    Group,
    None,
}

struct Request {
    key: String,
    from: u64,
    to: u64,
    amount: i64,
    reply: Sender<Result<u64, String>>,
}

/// The in-memory state. Wallets 0 and 1 are the system wallets (treasury and shop), which may go negative.
#[derive(Default)]
struct Ledger {
    balances: Vec<i64>,
    /// Idempotency key to (operation, from, to, amount). A real ledger keeps a bounded window of keys.
    keys: HashMap<String, (u64, u64, u64, i64)>,
    operations: u64,
}

/// What a request turns into.
enum Checked {
    New(Record),
    /// The key was used before with the same parameters: this operation already did it.
    Already(u64),
}

/// One operation as stored in the log.
struct Record {
    key: String,
    from: u64,
    to: u64,
    amount: i64,
    from_after: i64,
    to_after: i64,
}

impl Ledger {
    fn wallet_count(&self) -> u64 {
        self.balances.len() as u64
    }

    /// Checks a transfer and returns the record to write, without changing anything.
    fn check(&self, key: &str, from: u64, to: u64, amount: i64) -> Result<Checked, String> {
        if amount <= 0 {
            return Err(format!("amount {amount} is not positive"));
        }
        if from == to {
            return Err("same wallet".into());
        }
        if let Some(&(op, f, t, a)) = self.keys.get(key) {
            return if (f, t, a) == (from, to, amount) {
                Ok(Checked::Already(op))
            } else {
                Err(format!(
                    "key {key} already used by operation {op} with other parameters"
                ))
            };
        }
        let (Some(&fb), Some(&tb)) = (self.balances.get(from as usize), self.balances.get(to as usize))
        else {
            return Err("unknown wallet".into());
        };
        let from_after = fb.checked_sub(amount).ok_or("overflow")?;
        if from_after < 0 && from > SHOP {
            return Err(format!("wallet {from} holds {fb}, needs {amount}"));
        }
        let to_after = tb.checked_add(amount).ok_or("overflow")?;
        Ok(Checked::New(Record {
            key: key.to_string(),
            from,
            to,
            amount,
            from_after,
            to_after,
        }))
    }

    /// Applies a checked record (or one read back from the log).
    fn apply(&mut self, r: &Record) -> Result<u64, String> {
        let need = r.from.max(r.to) as usize + 1;
        if self.balances.len() < need {
            self.balances.resize(need, 0);
        }
        if self.balances[r.from as usize] - r.amount != r.from_after
            || self.balances[r.to as usize] + r.amount != r.to_after
        {
            return Err(format!("record {} does not follow from the balances", r.key));
        }
        self.balances[r.from as usize] = r.from_after;
        self.balances[r.to as usize] = r.to_after;
        self.operations += 1;
        self.keys
            .insert(r.key.clone(), (self.operations, r.from, r.to, r.amount));
        Ok(self.operations)
    }

    fn check_invariants(&self) -> Result<(), String> {
        let total: i128 = self.balances.iter().map(|&b| i128::from(b)).sum();
        if total != 0 {
            return Err(format!("balances sum to {total}"));
        }
        if let Some(w) = self.balances.iter().skip(SHOP as usize + 1).position(|&b| b < 0) {
            return Err(format!("character wallet {} is negative", w + 2));
        }
        Ok(())
    }
}

fn encode(r: &Record, out: &mut Vec<u8>) {
    let mut p = Vec::with_capacity(64);
    p.extend_from_slice(&(r.key.len() as u16).to_le_bytes());
    p.extend_from_slice(r.key.as_bytes());
    for v in [r.from, r.to] {
        p.extend_from_slice(&v.to_le_bytes());
    }
    for v in [r.amount, r.from_after, r.to_after] {
        p.extend_from_slice(&v.to_le_bytes());
    }
    let len = (p.len() as u32).to_le_bytes();
    out.extend_from_slice(&len);
    out.extend_from_slice(&crc32(&len).to_le_bytes());
    out.extend_from_slice(&crc32(&p).to_le_bytes());
    out.extend_from_slice(&p);
}

fn decode(p: &[u8]) -> Option<Record> {
    let klen = u16::from_le_bytes(p.get(0..2)?.try_into().ok()?) as usize;
    let key = String::from_utf8(p.get(2..2 + klen)?.to_vec()).ok()?;
    let mut at = 2 + klen;
    let mut next = || {
        let v = u64::from_le_bytes(p.get(at..at + 8)?.try_into().ok()?);
        at += 8;
        Some(v)
    };
    let (from, to) = (next()?, next()?);
    let (amount, from_after, to_after) = (next()? as i64, next()? as i64, next()? as i64);
    Some(Record {
        key,
        from,
        to,
        amount,
        from_after,
        to_after,
    })
}

fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 == 1 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *e = c;
        }
        t
    });
    !data.iter().fold(!0u32, |c, &b| {
        table[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8)
    })
}

/// Replays a log into a fresh ledger and returns it with the length of the valid log.
///
/// Only the last record may be torn: one that is cut short, or fails its checksum and ends exactly at the end of
/// the file (a crash during its write). It is reported so the caller can truncate it. A record that fails its
/// checksum, decoding or the balance check anywhere else is corruption with acknowledged operations after it, and
/// replay fails so that an operator decides.
fn replay(path: &str) -> Result<(Ledger, u64, u64), String> {
    let mut ledger = Ledger::default();
    let Ok(file) = File::open(path) else {
        return Ok((ledger, 0, 0));
    };
    let total = file.metadata().map_err(|e| e.to_string())?.len();
    let mut r = BufReader::with_capacity(1 << 20, file);
    let mut good = 0u64;
    let mut head = [0u8; 12];
    let mut payload = Vec::new();
    while good < total {
        if total - good < 12 {
            break; // A header cut short: torn tail.
        }
        r.read_exact(&mut head).map_err(|e| e.to_string())?;
        let word = |i: usize| u32::from_le_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
        // The length has its own checksum, so the torn-tail decision below never rests on unchecked bytes. A bad
        // header is never taken for a torn tail: start-up stops and an operator decides.
        if crc32(&head[0..4]) != word(4) {
            return Err(format!(
                "corrupt record header at byte {good}, {} bytes of log from there; refusing to start",
                total - good
            ));
        }
        let len = u64::from(word(0));
        let crc = word(8);
        let end = good + 12 + len;
        if end > total {
            break; // A record cut short (by its checked length): torn tail.
        }
        payload.resize(len as usize, 0);
        r.read_exact(&mut payload).map_err(|e| e.to_string())?;
        let record = (crc32(&payload) == crc).then(|| decode(&payload)).flatten();
        let Some(record) = record else {
            if end == total {
                break; // The last record, written in part: torn tail.
            }
            return Err(format!(
                "corrupt record at byte {good}, {} bytes of log after it; refusing to start",
                total - end
            ));
        };
        ledger
            .apply(&record)
            .map_err(|e| format!("record at byte {good} is inconsistent: {e}; refusing to start"))?;
        good = end;
    }
    Ok((ledger, good, total - good))
}

fn writer(mut ledger: Ledger, mut log: File, flush: Flush, marker: Option<File>, rx: Receiver<Request>) {
    let mut buf = Vec::with_capacity(1 << 20);
    let mut pending: Vec<(Sender<Result<u64, String>>, u64)> = Vec::with_capacity(MAX_BATCH);
    while let Ok(first) = rx.recv() {
        let mut next = Some(first);
        while let Some(req) = next.take() {
            match ledger.check(&req.key, req.from, req.to, req.amount) {
                Ok(Checked::New(record)) => {
                    encode(&record, &mut buf);
                    let op = ledger.apply(&record).expect("checked");
                    pending.push((req.reply, op));
                }
                // Answered after this batch's flush too: the original may be in it.
                Ok(Checked::Already(op)) => pending.push((req.reply, op)),
                Err(e) => {
                    let _ = req.reply.send(Err(e));
                }
            }
            if flush != Flush::Each && pending.len() < MAX_BATCH {
                next = rx.try_recv().ok();
            }
        }
        if !buf.is_empty() {
            log.write_all(&buf).expect("write log");
            if flush != Flush::None {
                log.sync_data().expect("sync log");
            }
            buf.clear();
        }
        if let Some(marker) = &marker {
            marker
                .write_all_at(&ledger.operations.to_le_bytes(), 0)
                .expect("write marker");
        }
        for (reply, op) in pending.drain(..) {
            let _ = reply.send(Ok(op));
        }
    }
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

fn run(path: &str, clients: usize, seconds: u64, hot: bool, flush: Flush) {
    let (mut ledger, good, torn) = match replay(path) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    };
    let created = !Path::new(path).exists();
    let log = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .expect("open log");
    log.set_len(good).expect("truncate torn tail");
    log.sync_all().expect("sync log");
    drop(log);
    if created {
        // Make the new file's directory entry durable too, or a power cut can lose the whole file.
        let dir = Path::new(path).parent().filter(|d| !d.as_os_str().is_empty());
        File::open(dir.unwrap_or(Path::new(".")))
            .and_then(|d| d.sync_all())
            .expect("sync log directory");
    }
    if torn > 0 {
        eprintln!("dropped {torn} bytes of torn tail");
    }
    let mut log = OpenOptions::new().append(true).open(path).expect("open log");
    if ledger.wallet_count() == 0 {
        // Seed: system wallets 0 and 1, then one bank wallet per character funded by the treasury.
        let mut buf = Vec::new();
        ledger.balances.resize(2 + CHARACTERS as usize, 0);
        for w in 2..2 + CHARACTERS {
            let Ok(Checked::New(r)) = ledger.check(&format!("seed-{w}"), TREASURY, w, STARTING_BALANCE)
            else {
                panic!("seed");
            };
            encode(&r, &mut buf);
            ledger.apply(&r).expect("seed");
        }
        log.write_all(&buf).expect("write");
        log.sync_data().expect("sync");
    }
    let wallets = ledger.wallet_count();
    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    let (tx_unbounded, rx_unbounded) = mpsc::channel::<Request>();
    let marker = std::env::var_os("SAWGRASS_ACK_MARKER").map(|_| {
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(format!("{path}.acked"))
            .expect("open marker")
    });
    let writer_thread = std::thread::spawn(move || writer(ledger, log, flush, marker, rx_unbounded));

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let started = Instant::now();
    let handles: Vec<_> = (0..clients)
        .map(|client| {
            let tx = tx_unbounded.clone();
            std::thread::spawn(move || {
                let mut rng =
                    XorShift(0x9E37_79B9_7F4A_7C15 ^ (client as u64 + 1).wrapping_mul(0xA24B_AED4_963E_E407));
                let (reply, replies) = mpsc::channel();
                let mut latencies = Vec::new();
                let mut n = 0u64;
                while Instant::now() < deadline {
                    let from = 2 + rng.below(wallets - 2);
                    let to = if hot {
                        SHOP
                    } else {
                        loop {
                            let to = 2 + rng.below(wallets - 2);
                            if to != from {
                                break to;
                            }
                        }
                    };
                    let begun = Instant::now();
                    tx.send(Request {
                        key: format!("load-{run_id}-{client}-{n}"),
                        from,
                        to,
                        amount: 1 + rng.below(100) as i64,
                        reply: reply.clone(),
                    })
                    .expect("send");
                    n += 1;
                    replies.recv().expect("reply").expect("transfer");
                    latencies.push(begun.elapsed());
                }
                latencies
            })
        })
        .collect();
    drop(tx_unbounded);
    let mut latencies: Vec<Duration> = handles
        .into_iter()
        .flat_map(|h| h.join().expect("client"))
        .collect();
    let elapsed = started.elapsed();
    writer_thread.join().expect("writer");
    latencies.sort_unstable();
    let at = |q: f64| {
        let i = ((latencies.len() as f64 - 1.0) * q).round() as usize;
        latencies.get(i).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
    };
    let mode = match flush {
        Flush::Each => "each",
        Flush::Group => "group",
        Flush::None => "none",
    };
    println!(
        "pattern={} clients={clients} flush={mode} transfers={} tps={:.0} p50_ms={:.3} p99_ms={:.3} max_ms={:.2}",
        if hot { "Hot" } else { "Uniform" },
        latencies.len(),
        latencies.len() as f64 / elapsed.as_secs_f64(),
        at(0.5),
        at(0.99),
        at(1.0)
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("run") if args.len() == 7 => {
            let flush = match args[6].as_str() {
                "each" => Flush::Each,
                "group" => Flush::Group,
                _ => Flush::None,
            };
            run(
                &args[2],
                args[3].parse().expect("clients"),
                args[4].parse().expect("seconds"),
                args[5] == "hot",
                flush,
            );
        }
        Some("recover") if args.len() == 3 => {
            let started = Instant::now();
            let (ledger, good, torn) = match replay(&args[2]) {
                Ok(state) => state,
                Err(e) => {
                    println!("{}: {e}", args[2]);
                    std::process::exit(1);
                }
            };
            let elapsed = started.elapsed();
            let marker = std::fs::read(format!("{}.acked", args[2]))
                .ok()
                .and_then(|b| b.try_into().ok())
                .map(u64::from_le_bytes);
            let consistent = ledger.check_invariants();
            println!(
                "operations={} acknowledged_marker={} log_bytes={good} torn_bytes={torn} seconds={:.2} invariants={}",
                ledger.operations,
                marker.map_or_else(|| "none".into(), |m| m.to_string()),
                elapsed.as_secs_f64(),
                consistent.map_or_else(|e| e, |()| "ok".into())
            );
        }
        _ => eprintln!(
            "usage: inproc_ledger run <log> <clients> <seconds> <uniform|hot> <each|group|none> | recover <log>"
        ),
    }
}
