//! State with roleplay value (money now; inventory, property and factions later) and the single writer that owns
//! it, as ADR 0003 decides.
//!
//! One [`Writer`] thread holds the state in memory, checks every rule against it, and commits waiting operations in
//! batches to a [`Store`], [`RocksStore`] in production: one durable write per batch, and no answer leaves before
//! the write that covers it. Callers hold a [`Handle`]; nothing else changes the state.
//!
//! ```no_run
//! use sawgrass_store::{Config, Kind, Operation, RocksStore, Writer};
//!
//! let writer = Writer::start(RocksStore::open("store".as_ref())?, Config::default())?;
//! let ledger = writer.handle();
//! let opened = ledger.submit(Operation::OpenWallet { key: "treasury".into(), kind: Kind::System }).wait()?;
//! # Ok::<(), sawgrass_store::Error>(())
//! ```
//!
//! Not here yet: the copy to a second machine before an answer (DB-6) and the PostgreSQL archive (DB-7).

pub mod ledger;
pub mod store;
pub mod writer;

pub use ledger::{Error, Kind, Operation, OperationId, Outcome, Record, Wallet, WalletId};
pub use store::{Loaded, RocksStore, Store};
pub use writer::{Config, Handle, MAX_BATCH, Pending, Stats, Writer};
