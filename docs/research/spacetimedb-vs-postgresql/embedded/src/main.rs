//! The DB-1 money ledger on embedded storage engines, behind the same single writer and group commit as
//! `../inproc`. Only the storage differs: instead of our own log, each batch of operations is one transaction in
//! SQLite, redb, RocksDB or LMDB, committed durably before any caller in the batch gets its reply.
//!
//! The writer keeps balances and idempotency keys in memory for its checks, as `../inproc` does, and writes each
//! operation and the new balances of the wallets it touched to the store. On start it loads the wallets and
//! rebuilds the keys from the stored operations, which is what a restart costs.
//!
//! ```text
//! embedded_ledger run <engine> <dir> <clients> <seconds> <uniform|hot> <each|group>
//! embedded_ledger recover <engine> <dir>
//! ```
//!
//! Engines: `sqlite-full` (WAL, `synchronous=FULL`), `sqlite-normal` (WAL, `synchronous=NORMAL`: not durable
//! against a power cut until the next checkpoint), `redb` (`Durability::Immediate`), `rocksdb` (write batch with
//! `sync=true`, WAL recovery mode `TolerateCorruptedTailRecords`), `lmdb` (default flags: synced on commit), `fjall`
//! (write batch with `PersistMode::SyncAll`). `each` commits one transaction per transfer, `group` one
//! per batch of up to 4,096 waiting requests.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

const CHARACTERS: u64 = 10_000;
const STARTING_BALANCE: i64 = 100_000_000;
const TREASURY: u64 = 0;
const SHOP: u64 = 1;
const MAX_BATCH: usize = 4096;

/// Wallet balances and stored operations, as `Store::load` returns them.
type Loaded = (Vec<(u64, i64)>, Vec<Record>);
type Res<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

struct Request {
    key: String,
    from: u64,
    to: u64,
    amount: i64,
    reply: Sender<Res<u64>>,
}

#[derive(Clone)]
struct Record {
    key: String,
    from: u64,
    to: u64,
    amount: i64,
    from_after: i64,
    to_after: i64,
}

enum Checked {
    New(Record),
    Already(u64),
}

#[derive(Default)]
struct Ledger {
    balances: Vec<i64>,
    keys: HashMap<String, (u64, u64, u64, i64)>,
    operations: u64,
}

impl Ledger {
    fn check(&self, key: &str, from: u64, to: u64, amount: i64) -> Res<Checked> {
        if amount <= 0 || from == to {
            return Err("invalid transfer".into());
        }
        if let Some(&(op, f, t, a)) = self.keys.get(key) {
            return if (f, t, a) == (from, to, amount) {
                Ok(Checked::Already(op))
            } else {
                Err(format!("key {key} reused with other parameters"))
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
            key: key.into(),
            from,
            to,
            amount,
            from_after,
            to_after,
        }))
    }

    fn apply(&mut self, r: &Record) -> u64 {
        self.balances[r.from as usize] = r.from_after;
        self.balances[r.to as usize] = r.to_after;
        self.operations += 1;
        self.keys
            .insert(r.key.clone(), (self.operations, r.from, r.to, r.amount));
        self.operations
    }

    fn check_invariants(&self) -> Res<()> {
        let total: i128 = self.balances.iter().map(|&b| i128::from(b)).sum();
        if total != 0 {
            return Err(format!("balances sum to {total}"));
        }
        if self.balances.iter().skip(2).any(|&b| b < 0) {
            return Err("a character wallet is negative".into());
        }
        Ok(())
    }
}

fn encode(r: &Record) -> Vec<u8> {
    let mut p = Vec::with_capacity(48 + r.key.len());
    for v in [r.from, r.to] {
        p.extend_from_slice(&v.to_le_bytes());
    }
    for v in [r.amount, r.from_after, r.to_after] {
        p.extend_from_slice(&v.to_le_bytes());
    }
    p.extend_from_slice(r.key.as_bytes());
    p
}

fn decode(p: &[u8]) -> Option<Record> {
    let n = |i: usize| {
        p.get(i * 8..i * 8 + 8)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_le_bytes)
    };
    Some(Record {
        from: n(0)?,
        to: n(1)?,
        amount: n(2)? as i64,
        from_after: n(3)? as i64,
        to_after: n(4)? as i64,
        key: String::from_utf8(p.get(40..)?.to_vec()).ok()?,
    })
}

/// A storage engine: commit a batch of operations and the touched wallets in one durable transaction.
trait Store {
    fn commit(&mut self, ops: &[(u64, Record)], wallets: &[(u64, i64)]) -> Res<()>;
    /// All wallets, and all operations in id order.
    fn load(&mut self) -> Res<Loaded>;
}

struct Sqlite(rusqlite::Connection);

impl Sqlite {
    fn open(dir: &Path, full: bool) -> Res<Self> {
        let c = rusqlite::Connection::open(dir.join("ledger.sqlite")).map_err(err)?;
        let sync = if full { "FULL" } else { "NORMAL" };
        c.execute_batch(&format!(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous={sync};
             CREATE TABLE IF NOT EXISTS wallets (id INTEGER PRIMARY KEY, balance INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS operations (id INTEGER PRIMARY KEY, key TEXT NOT NULL UNIQUE,
                 from_wallet INTEGER NOT NULL, to_wallet INTEGER NOT NULL, amount INTEGER NOT NULL,
                 from_after INTEGER NOT NULL, to_after INTEGER NOT NULL);"
        ))
        .map_err(err)?;
        Ok(Self(c))
    }
}

impl Store for Sqlite {
    fn commit(&mut self, ops: &[(u64, Record)], wallets: &[(u64, i64)]) -> Res<()> {
        let tx = self.0.transaction().map_err(err)?;
        {
            let mut op = tx
                .prepare_cached("INSERT INTO operations VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")
                .map_err(err)?;
            for (id, r) in ops {
                op.execute(rusqlite::params![
                    *id as i64,
                    r.key,
                    r.from as i64,
                    r.to as i64,
                    r.amount,
                    r.from_after,
                    r.to_after
                ])
                .map_err(err)?;
            }
            let mut w = tx
                .prepare_cached(
                    "INSERT INTO wallets VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET balance = ?2",
                )
                .map_err(err)?;
            for (id, balance) in wallets {
                w.execute(rusqlite::params![*id as i64, balance]).map_err(err)?;
            }
        }
        tx.commit().map_err(err)
    }

    fn load(&mut self) -> Res<Loaded> {
        let mut s = self.0.prepare("SELECT id, balance FROM wallets").map_err(err)?;
        let wallets = s
            .query_map([], |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)))
            .map_err(err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(err)?;
        let mut s = self
            .0
            .prepare("SELECT key, from_wallet, to_wallet, amount, from_after, to_after FROM operations ORDER BY id")
            .map_err(err)?;
        let ops = s
            .query_map([], |r| {
                Ok(Record {
                    key: r.get(0)?,
                    from: r.get::<_, i64>(1)? as u64,
                    to: r.get::<_, i64>(2)? as u64,
                    amount: r.get(3)?,
                    from_after: r.get(4)?,
                    to_after: r.get(5)?,
                })
            })
            .map_err(err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(err)?;
        Ok((wallets, ops))
    }
}

const REDB_WALLETS: redb::TableDefinition<u64, i64> = redb::TableDefinition::new("wallets");
const REDB_OPS: redb::TableDefinition<u64, &[u8]> = redb::TableDefinition::new("operations");

struct Redb(redb::Database);

impl Store for Redb {
    fn commit(&mut self, ops: &[(u64, Record)], wallets: &[(u64, i64)]) -> Res<()> {
        let mut tx = self.0.begin_write().map_err(err)?;
        // The default in redb 3, set explicitly so the durability claim does not rest on a library default.
        tx.set_durability(redb::Durability::Immediate).map_err(err)?;
        {
            let mut o = tx.open_table(REDB_OPS).map_err(err)?;
            for (id, r) in ops {
                o.insert(id, encode(r).as_slice()).map_err(err)?;
            }
            let mut w = tx.open_table(REDB_WALLETS).map_err(err)?;
            for (id, balance) in wallets {
                w.insert(id, balance).map_err(err)?;
            }
        }
        tx.commit().map_err(err)
    }

    fn load(&mut self) -> Res<Loaded> {
        use redb::{ReadableDatabase, ReadableTable};
        let tx = self.0.begin_read().map_err(err)?;
        let (Ok(w), Ok(o)) = (tx.open_table(REDB_WALLETS), tx.open_table(REDB_OPS)) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let mut wallets = Vec::new();
        for item in w.iter().map_err(err)? {
            let (k, v) = item.map_err(err)?;
            wallets.push((k.value(), v.value()));
        }
        let mut ops = Vec::new();
        for item in o.iter().map_err(err)? {
            let (_, v) = item.map_err(err)?;
            ops.push(decode(v.value()).ok_or("bad record")?);
        }
        Ok((wallets, ops))
    }
}

struct Fjall {
    db: fjall::Database,
    wallets: fjall::Keyspace,
    ops: fjall::Keyspace,
}

impl Fjall {
    fn open(dir: &Path) -> Res<Self> {
        let db = fjall::Database::builder(dir.join("fjall")).open().map_err(err)?;
        let wallets = db
            .keyspace("wallets", fjall::KeyspaceCreateOptions::default)
            .map_err(err)?;
        let ops = db
            .keyspace("operations", fjall::KeyspaceCreateOptions::default)
            .map_err(err)?;
        Ok(Self { db, wallets, ops })
    }
}

impl Store for Fjall {
    fn commit(&mut self, ops: &[(u64, Record)], wallets: &[(u64, i64)]) -> Res<()> {
        // `SyncAll`: the journal is synced to disk before `commit` returns.
        let mut batch = self.db.batch().durability(Some(fjall::PersistMode::SyncAll));
        for (id, r) in ops {
            batch.insert(&self.ops, id.to_be_bytes(), encode(r));
        }
        for (id, balance) in wallets {
            batch.insert(&self.wallets, id.to_be_bytes(), balance.to_le_bytes());
        }
        batch.commit().map_err(err)
    }

    fn load(&mut self) -> Res<Loaded> {
        let (mut wallets, mut ops) = (Vec::new(), Vec::new());
        for item in self.wallets.iter() {
            let (k, v) = item.into_inner().map_err(err)?;
            let id = u64::from_be_bytes(k[..].try_into().map_err(err)?);
            wallets.push((id, i64::from_le_bytes(v[..].try_into().map_err(err)?)));
        }
        for item in self.ops.iter() {
            let (_, v) = item.into_inner().map_err(err)?;
            ops.push(decode(&v).ok_or("bad record")?);
        }
        Ok((wallets, ops))
    }
}

struct Rocks(rocksdb::DB);

impl Store for Rocks {
    fn commit(&mut self, ops: &[(u64, Record)], wallets: &[(u64, i64)]) -> Res<()> {
        let mut batch = rocksdb::WriteBatch::default();
        for (id, r) in ops {
            batch.put([&b"o"[..], &id.to_be_bytes()].concat(), encode(r));
        }
        for (id, balance) in wallets {
            batch.put([&b"w"[..], &id.to_be_bytes()].concat(), balance.to_le_bytes());
        }
        let mut opts = rocksdb::WriteOptions::default();
        opts.set_sync(true);
        self.0.write_opt(batch, &opts).map_err(err)
    }

    fn load(&mut self) -> Res<Loaded> {
        let (mut wallets, mut ops) = (Vec::new(), Vec::new());
        for item in self.0.iterator(rocksdb::IteratorMode::Start) {
            let (k, v) = item.map_err(err)?;
            let id = u64::from_be_bytes(k.get(1..9).and_then(|b| b.try_into().ok()).ok_or("bad key")?);
            match k[0] {
                b'w' => wallets.push((id, i64::from_le_bytes(v[..8].try_into().map_err(err)?))),
                _ => ops.push(decode(&v).ok_or("bad record")?),
            }
        }
        Ok((wallets, ops))
    }
}

type BeU64 = heed::types::U64<heed::byteorder::BigEndian>;

struct Lmdb {
    env: heed::Env,
    wallets: heed::Database<BeU64, heed::types::I64<heed::byteorder::LittleEndian>>,
    ops: heed::Database<BeU64, heed::types::Bytes>,
}

impl Lmdb {
    fn open(dir: &Path) -> Res<Self> {
        // SAFETY: LMDB requires that one environment is not opened twice in a process and that its files are not
        // modified by anyone else while open. This benchmark opens it once, in its own directory.
        #[allow(unsafe_code)]
        let env = unsafe {
            heed::EnvOpenOptions::new()
                .map_size(64 << 30)
                .max_dbs(2)
                .open(dir)
        }
        .map_err(err)?;
        let mut tx = env.write_txn().map_err(err)?;
        let wallets = env.create_database(&mut tx, Some("wallets")).map_err(err)?;
        let ops = env.create_database(&mut tx, Some("operations")).map_err(err)?;
        tx.commit().map_err(err)?;
        Ok(Self { env, wallets, ops })
    }
}

impl Store for Lmdb {
    fn commit(&mut self, ops: &[(u64, Record)], wallets: &[(u64, i64)]) -> Res<()> {
        let mut tx = self.env.write_txn().map_err(err)?;
        for (id, r) in ops {
            self.ops.put(&mut tx, id, &encode(r)).map_err(err)?;
        }
        for (id, balance) in wallets {
            self.wallets.put(&mut tx, id, balance).map_err(err)?;
        }
        tx.commit().map_err(err)
    }

    fn load(&mut self) -> Res<Loaded> {
        let tx = self.env.read_txn().map_err(err)?;
        let wallets = self
            .wallets
            .iter(&tx)
            .map_err(err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(err)?;
        let mut ops = Vec::new();
        for item in self.ops.iter(&tx).map_err(err)? {
            let (_, v) = item.map_err(err)?;
            ops.push(decode(v).ok_or("bad record")?);
        }
        Ok((wallets, ops))
    }
}

fn open(engine: &str, dir: &Path) -> Res<Box<dyn Store + Send>> {
    std::fs::create_dir_all(dir).map_err(err)?;
    Ok(match engine {
        "sqlite-full" => Box::new(Sqlite::open(dir, true)?),
        "sqlite-normal" => Box::new(Sqlite::open(dir, false)?),
        "redb" => Box::new(Redb(
            redb::Database::create(dir.join("ledger.redb")).map_err(err)?,
        )),
        "rocksdb" => {
            let mut o = rocksdb::Options::default();
            o.create_if_missing(true);
            // Drop only an incomplete last WAL record (never acknowledged); any other corruption stops start-up.
            o.set_wal_recovery_mode(rocksdb::DBRecoveryMode::TolerateCorruptedTailRecords);
            o.set_paranoid_checks(true);
            Box::new(Rocks(rocksdb::DB::open(&o, dir.join("rocksdb")).map_err(err)?))
        }
        "lmdb" => Box::new(Lmdb::open(dir)?),
        "fjall" => Box::new(Fjall::open(dir)?),
        other => return Err(format!("unknown engine {other}")),
    })
}

/// Loads the state from the store and checks that every stored balance follows from the stored operations.
fn load(store: &mut dyn Store) -> Res<Ledger> {
    let (wallets, ops) = store.load()?;
    let mut ledger = Ledger::default();
    let n = wallets.iter().map(|(id, _)| *id as usize + 1).max().unwrap_or(0);
    ledger.balances.resize(n, 0);
    let mut replayed = vec![0i64; n];
    for r in &ops {
        replayed[r.from as usize] -= r.amount;
        replayed[r.to as usize] += r.amount;
        ledger.operations += 1;
        ledger
            .keys
            .insert(r.key.clone(), (ledger.operations, r.from, r.to, r.amount));
    }
    for (id, balance) in wallets {
        ledger.balances[id as usize] = balance;
    }
    if replayed != ledger.balances {
        return Err("stored balances do not match the stored operations".into());
    }
    Ok(ledger)
}

fn writer(mut ledger: Ledger, mut store: Box<dyn Store + Send>, group: bool, rx: Receiver<Request>) {
    // For crash tests: after each commit and before any reply, record how many operations are committed, so every
    // acknowledgement a caller received is covered by the number in this file.
    let marker = std::env::var_os("SAWGRASS_ACK_MARKER").map(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .expect("marker")
    });
    let mut ops = Vec::with_capacity(MAX_BATCH);
    let mut touched: HashMap<u64, i64> = HashMap::new();
    let mut pending: Vec<(Sender<Res<u64>>, u64)> = Vec::with_capacity(MAX_BATCH);
    while let Ok(first) = rx.recv() {
        let mut next = Some(first);
        while let Some(req) = next.take() {
            match ledger.check(&req.key, req.from, req.to, req.amount) {
                Ok(Checked::New(r)) => {
                    let op = ledger.apply(&r);
                    touched.insert(r.from, r.from_after);
                    touched.insert(r.to, r.to_after);
                    ops.push((op, r));
                    pending.push((req.reply, op));
                }
                Ok(Checked::Already(op)) => pending.push((req.reply, op)),
                Err(e) => {
                    let _ = req.reply.send(Err(e));
                }
            }
            if group && pending.len() < MAX_BATCH {
                next = rx.try_recv().ok();
            }
        }
        let wallets: Vec<(u64, i64)> = touched.drain().collect();
        store.commit(&ops, &wallets).expect("commit");
        ops.clear();
        if let Some(marker) = &marker {
            use std::os::unix::fs::FileExt;
            marker
                .write_all_at(&ledger.operations.to_le_bytes(), 0)
                .expect("marker");
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

fn run(engine: &str, dir: &Path, clients: usize, seconds: u64, hot: bool, group: bool) -> Res<()> {
    let mut store = open(engine, dir)?;
    let mut ledger = load(store.as_mut())?;
    if ledger.balances.is_empty() {
        ledger.balances.resize(2 + CHARACTERS as usize, 0);
        let mut ops = Vec::new();
        for w in 2..2 + CHARACTERS {
            let Ok(Checked::New(r)) = ledger.check(&format!("seed-{w}"), TREASURY, w, STARTING_BALANCE)
            else {
                return Err("seed".into());
            };
            let op = ledger.apply(&r);
            ops.push((op, r));
        }
        let wallets: Vec<(u64, i64)> = ledger
            .balances
            .iter()
            .enumerate()
            .map(|(i, &b)| (i as u64, b))
            .collect();
        store.commit(&ops, &wallets)?;
    }
    let wallets = ledger.balances.len() as u64;
    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let (tx, rx) = mpsc::channel::<Request>();
    let writer_thread = std::thread::spawn(move || writer(ledger, store, group, rx));
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let started = Instant::now();
    let handles: Vec<_> = (0..clients)
        .map(|client| {
            let tx = tx.clone();
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
    drop(tx);
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
    println!(
        "engine={engine} pattern={} clients={clients} commit={} transfers={} tps={:.0} p50_ms={:.3} p99_ms={:.3} \
         max_ms={:.1}",
        if hot { "hot" } else { "uniform" },
        if group { "group" } else { "each" },
        latencies.len(),
        latencies.len() as f64 / elapsed.as_secs_f64(),
        at(0.5),
        at(0.99),
        at(1.0)
    );
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("run") if args.len() == 8 => run(
            &args[2],
            Path::new(&args[3]),
            args[4].parse().unwrap_or(1),
            args[5].parse().unwrap_or(1),
            args[6] == "hot",
            args[7] == "group",
        ),
        Some("recover") if args.len() == 4 => (|| {
            let started = Instant::now();
            let mut store = open(&args[2], Path::new(&args[3]))?;
            let ledger = load(store.as_mut())?;
            let elapsed = started.elapsed();
            let invariants = ledger.check_invariants().map_or_else(|e| e, |()| "ok".into());
            // With the marker of a killed run: every acknowledged operation must still be there.
            let acknowledged = match std::env::var_os("SAWGRASS_ACK_MARKER") {
                Some(path) => {
                    let bytes = std::fs::read(path).map_err(err)?;
                    let n =
                        u64::from_le_bytes(bytes.get(..8).ok_or("short marker")?.try_into().map_err(err)?);
                    let verdict = if ledger.operations >= n {
                        "all present"
                    } else {
                        "LOST"
                    };
                    format!(" acknowledged={n} {verdict}")
                }
                None => String::new(),
            };
            println!(
                "engine={} operations={} seconds={:.2} invariants={invariants}{acknowledged}",
                args[2],
                ledger.operations,
                elapsed.as_secs_f64()
            );
            Ok(())
        })(),
        _ => Err(
            "usage: embedded_ledger run <engine> <dir> <clients> <seconds> <uniform|hot> <each|group> | \
                  recover <engine> <dir>"
                .into(),
        ),
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
