//! Where the writer's state is kept: the [`Store`] trait and its RocksDB implementation.
//!
//! RocksDB is set as ADR 0003 decides. Each batch is one write batch with `sync`, so the write-ahead log is on disk
//! before the batch's callers get a reply. The WAL recovery mode is `TolerateCorruptedTailRecords`: an incomplete
//! last record, never acknowledged, is dropped, and any other damage stops [`RocksStore::open`] with an error for an
//! operator, who restores from the second machine's copy and never opens in a laxer mode. `paranoid_checks` stays
//! on.
//!
//! Keys: `w` + wallet id (big-endian `u64`) holds the wallet's kind and balance; `o` + operation id holds the
//! operation's record. Big-endian ids keep both in id order.

use std::path::Path;

use crate::ledger::{Error, Kind, Operation, OperationId, Outcome, Record, Wallet, WalletId};

/// What the writer loads at start-up.
#[derive(Debug, Default)]
pub struct Loaded {
    /// Every wallet.
    pub wallets: Vec<Wallet>,
    /// The id of the last committed operation, 0 if none.
    pub last_operation: u64,
    /// The records applied at or after the `since_ms` given to [`Store::load`], oldest first: the idempotency
    /// window.
    pub recent: Vec<Record>,
}

/// A durable home for the writer's state. Implementations must make [`Store::commit`] durable and atomic: when it
/// returns `Ok`, the whole batch survives a crash, and after a crash either all of a batch is there or none.
pub trait Store {
    /// Loads the state, with the records applied at or after `since_ms`.
    ///
    /// # Errors
    ///
    /// The store cannot be read or holds something it did not write.
    fn load(&mut self, since_ms: u64) -> Result<Loaded, Error>;

    /// Stores `records` and the new state of `wallets` as one durable batch.
    ///
    /// # Errors
    ///
    /// The batch is not known to be on disk.
    fn commit(&mut self, records: &[Record], wallets: &[Wallet]) -> Result<(), Error>;
}

/// The RocksDB store.
#[derive(Debug)]
pub struct RocksStore {
    db: rocksdb::DB,
}

impl RocksStore {
    /// Opens or creates the store in `dir`.
    ///
    /// # Errors
    ///
    /// RocksDB refuses to open it, for example because its log is damaged other than in an incomplete last record.
    pub fn open(dir: &Path) -> Result<Self, Error> {
        let mut options = rocksdb::Options::default();
        options.create_if_missing(true);
        options.set_wal_recovery_mode(rocksdb::DBRecoveryMode::TolerateCorruptedTailRecords);
        options.set_paranoid_checks(true);
        let db = rocksdb::DB::open(&options, dir).map_err(store)?;
        Ok(Self { db })
    }
}

fn store(e: impl std::fmt::Display) -> Error {
    Error::Store(e.to_string())
}

fn corrupt(what: &str) -> Error {
    Error::Corrupt(what.to_owned())
}

impl Store for RocksStore {
    fn load(&mut self, since_ms: u64) -> Result<Loaded, Error> {
        let mut loaded = Loaded::default();
        let wallets = self
            .db
            .iterator(rocksdb::IteratorMode::From(b"w", rocksdb::Direction::Forward));
        for item in wallets {
            let (key, value) = item.map_err(store)?;
            if key.first() != Some(&b'w') {
                break;
            }
            let id = WalletId(id_of(&key)?);
            let (kind, balance) = decode_wallet(&value).ok_or_else(|| corrupt("wallet"))?;
            loaded.wallets.push(Wallet { id, kind, balance });
        }
        // Newest first, until the window's start: start-up reads only the window, however long the history.
        let operations = self
            .db
            .iterator(rocksdb::IteratorMode::From(b"o\xff", rocksdb::Direction::Reverse));
        for item in operations {
            let (key, value) = item.map_err(store)?;
            if key.first() != Some(&b'o') {
                break;
            }
            let record = decode_record(id_of(&key)?, &value).ok_or_else(|| corrupt("operation"))?;
            loaded.last_operation = loaded.last_operation.max(record.id.0);
            if record.at_ms < since_ms {
                break;
            }
            loaded.recent.push(record);
        }
        loaded.recent.reverse();
        Ok(loaded)
    }

    fn commit(&mut self, records: &[Record], wallets: &[Wallet]) -> Result<(), Error> {
        let mut batch = rocksdb::WriteBatch::default();
        for r in records {
            batch.put(key(b'o', r.id.0), encode_record(r));
        }
        for w in wallets {
            batch.put(key(b'w', w.id.0), encode_wallet(w));
        }
        let mut options = rocksdb::WriteOptions::default();
        options.set_sync(true);
        self.db.write_opt(batch, &options).map_err(store)
    }
}

fn key(prefix: u8, id: u64) -> [u8; 9] {
    let mut k = [prefix; 9];
    k[1..].copy_from_slice(&id.to_be_bytes());
    k
}

fn id_of(key: &[u8]) -> Result<u64, Error> {
    key.get(1..9)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_be_bytes)
        .ok_or_else(|| corrupt("key"))
}

fn kind_byte(kind: Kind) -> u8 {
    match kind {
        Kind::Character => 0,
        Kind::System => 1,
    }
}

fn kind_of(byte: u8) -> Option<Kind> {
    match byte {
        0 => Some(Kind::Character),
        1 => Some(Kind::System),
        _ => None,
    }
}

fn encode_wallet(w: &Wallet) -> [u8; 9] {
    let mut v = [kind_byte(w.kind); 9];
    v[1..].copy_from_slice(&w.balance.to_le_bytes());
    v
}

fn decode_wallet(v: &[u8]) -> Option<(Kind, i64)> {
    let (&kind, balance) = v.split_first()?;
    Some((kind_of(kind)?, i64::from_le_bytes(balance.try_into().ok()?)))
}

/// `at_ms`, a tag, then for a wallet: kind, wallet id; for a transfer: from, to, amount; then the key.
fn encode_record(r: &Record) -> Vec<u8> {
    let mut v = Vec::with_capacity(48 + r.operation.key().len());
    v.extend_from_slice(&r.at_ms.to_le_bytes());
    match (&r.operation, r.outcome) {
        (Operation::OpenWallet { kind, .. }, Outcome::Opened { wallet, .. }) => {
            v.push(0);
            v.push(kind_byte(*kind));
            v.extend_from_slice(&wallet.0.to_le_bytes());
        }
        (Operation::Transfer { from, to, amount, .. }, _) => {
            v.push(1);
            v.extend_from_slice(&from.0.to_le_bytes());
            v.extend_from_slice(&to.0.to_le_bytes());
            v.extend_from_slice(&amount.to_le_bytes());
        }
        // The ledger pairs an opening with an opened outcome; nothing else is built.
        (Operation::OpenWallet { .. }, Outcome::Transferred { .. }) => v.push(u8::MAX),
    }
    v.extend_from_slice(r.operation.key().as_bytes());
    v
}

fn decode_record(id: u64, v: &[u8]) -> Option<Record> {
    let u64_at = |at: usize| {
        v.get(at..at + 8)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_le_bytes)
    };
    let at_ms = u64_at(0)?;
    let operation = OperationId(id);
    let (op, outcome) = match *v.get(8)? {
        0 => {
            let kind = kind_of(*v.get(9)?)?;
            let wallet = WalletId(u64_at(10)?);
            let key = String::from_utf8(v.get(18..)?.to_vec()).ok()?;
            (
                Operation::OpenWallet { key, kind },
                Outcome::Opened { operation, wallet },
            )
        }
        1 => {
            let key = String::from_utf8(v.get(33..)?.to_vec()).ok()?;
            let op = Operation::Transfer {
                key,
                from: WalletId(u64_at(9)?),
                to: WalletId(u64_at(17)?),
                amount: i64::from_le_bytes(v.get(25..33)?.try_into().ok()?),
            };
            (op, Outcome::Transferred { operation })
        }
        _ => return None,
    };
    Some(Record {
        id: operation,
        at_ms,
        operation: op,
        outcome,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_wallets_round_trip() {
        let records = [
            Record {
                id: OperationId(7),
                at_ms: 123,
                operation: Operation::OpenWallet {
                    key: "open-7".into(),
                    kind: Kind::System,
                },
                outcome: Outcome::Opened {
                    operation: OperationId(7),
                    wallet: WalletId(3),
                },
            },
            Record {
                id: OperationId(8),
                at_ms: 124,
                operation: Operation::Transfer {
                    key: "pay é".into(),
                    from: WalletId(3),
                    to: WalletId(4),
                    amount: -1,
                },
                outcome: Outcome::Transferred {
                    operation: OperationId(8),
                },
            },
        ];
        for r in records {
            assert_eq!(decode_record(r.id.0, &encode_record(&r)), Some(r));
        }
        let w = Wallet {
            id: WalletId(1),
            kind: Kind::Character,
            balance: i64::MIN,
        };
        assert_eq!(decode_wallet(&encode_wallet(&w)), Some((w.kind, w.balance)));
        assert_eq!(decode_record(1, &[0; 5]), None);
    }
}
