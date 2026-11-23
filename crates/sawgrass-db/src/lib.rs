//! PostgreSQL storage for Sawgrass: characters, wallets and the money ledger.
//!
//! PostgreSQL is the source of truth for everything with roleplay value (see `docs/architecture.md`). Money moves
//! only through [`transfer`], which writes an operation and its ledger entries in one transaction. In the schema in
//! `migrations/` the ledger drives the balances: inserting an entry applies it to its wallet, a direct balance
//! update is refused, the entries of an operation must sum to zero and the ledger is append-only. So a
//! hand-written query cannot create or lose money either.
//!
//! These triggers bind every role except the owner of the tables, which can disable or drop them. Run the
//! migrations as an owner role and the server as a separate role with `SELECT` and `INSERT` on the tables, `USAGE`
//! on their sequences and `EXECUTE` on `wallets_lock`, and no `UPDATE` right. Balances change only inside the
//! owner's `SECURITY DEFINER` trigger, so that role cannot change a balance outside the ledger.
//!
//! Amounts are whole minor units (cents) in `i64`. Money enters and leaves the economy through system wallets
//! (see [`system_wallet`]), the only wallets allowed to go negative, so the sum of all balances is always zero.

use std::fmt;

use sqlx::migrate::{MigrateError, Migrator};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Postgres, Transaction};

pub use sqlx::PgPool;

/// The migrations in `migrations/` at the repository root, embedded at compile time.
pub static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

/// Opens a connection pool, for example to `postgres://user:password@host/sawgrass`.
pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool, Error> {
    Ok(PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(url)
        .await?)
}

/// Applies every migration that the database has not run yet.
pub async fn migrate(pool: &PgPool) -> Result<(), Error> {
    Ok(MIGRATOR.run(pool).await?)
}

/// The database id of a character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CharacterId(pub i64);

/// The database id of a wallet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WalletId(pub i64);

/// The database id of a money operation in the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OperationId(pub i64);

/// A newly created character and its two empty wallets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewCharacter {
    pub id: CharacterId,
    pub cash: WalletId,
    pub bank: WalletId,
}

/// Creates a character with an empty `cash` and an empty `bank` wallet.
pub async fn create_character(
    pool: &PgPool,
    first_name: &str,
    last_name: &str,
) -> Result<NewCharacter, Error> {
    let mut tx = pool.begin().await?;
    let id: i64 =
        sqlx::query_scalar("INSERT INTO characters (first_name, last_name) VALUES ($1, $2) RETURNING id")
            .bind(first_name)
            .bind(last_name)
            .fetch_one(&mut *tx)
            .await?;
    let cash = insert_wallet(&mut tx, Some(id), "cash").await?;
    let bank = insert_wallet(&mut tx, Some(id), "bank").await?;
    tx.commit().await?;
    Ok(NewCharacter {
        id: CharacterId(id),
        cash,
        bank,
    })
}

/// Returns the system wallet with this name, creating it if it does not exist. A system wallet belongs to no
/// character and may go negative: paying a salary is a transfer from the treasury, buying from a shop is a
/// transfer to it.
pub async fn system_wallet(pool: &PgPool, name: &str) -> Result<WalletId, Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO wallets (kind) VALUES ($1) \
         ON CONFLICT (kind) WHERE character_id IS NULL DO NOTHING",
    )
    .bind(name)
    .execute(&mut *tx)
    .await?;
    let id: i64 = sqlx::query_scalar("SELECT id FROM wallets WHERE character_id IS NULL AND kind = $1")
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(WalletId(id))
}

/// The current balance of a wallet.
pub async fn balance(pool: &PgPool, wallet: WalletId) -> Result<i64, Error> {
    sqlx::query_scalar("SELECT balance FROM wallets WHERE id = $1")
        .bind(wallet.0)
        .fetch_optional(pool)
        .await?
        .ok_or(Error::UnknownWallet(wallet))
}

/// A request to move money from one wallet to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer<'a> {
    /// Unique per logical request, chosen by the caller (for example the event id that asked for it). A transfer
    /// retried with the same key is applied once.
    pub idempotency_key: &'a str,
    pub from: WalletId,
    pub to: WalletId,
    /// Minor units; must be positive.
    pub amount: i64,
    /// A short machine-readable cause, such as `salary` or `atm_withdrawal`, kept in the ledger for investigation.
    pub reason: &'a str,
}

/// What [`transfer`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferOutcome {
    /// The money moved in this call.
    Applied(OperationId),
    /// An earlier call with the same key and the same parameters already moved it; nothing changed now.
    AlreadyApplied(OperationId),
}

/// Moves `amount` from one wallet to another and records it in the ledger, all in one transaction.
///
/// Both wallet rows are locked in id order, so concurrent transfers serialise per wallet and cannot deadlock on each
/// other. A character wallet never goes below zero.
pub async fn transfer(pool: &PgPool, request: &Transfer<'_>) -> Result<TransferOutcome, Error> {
    if request.amount <= 0 {
        return Err(Error::InvalidAmount(request.amount));
    }
    if request.from == request.to {
        return Err(Error::SameWallet(request.from));
    }

    let mut tx = pool.begin().await?;

    // A concurrent call with the same key waits here until the first one commits or rolls back.
    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO operations (idempotency_key, reason) VALUES ($1, $2) \
         ON CONFLICT (idempotency_key) DO NOTHING RETURNING id",
    )
    .bind(request.idempotency_key)
    .bind(request.reason)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(operation) = inserted else {
        drop(tx);
        return previous_outcome(pool, request).await;
    };

    let rows: Vec<(i64, i64, bool)> = sqlx::query_as("SELECT id, balance, is_system FROM wallets_lock($1)")
        .bind(vec![request.from.0, request.to.0])
        .fetch_all(&mut *tx)
        .await?;
    let find = |wallet: WalletId| {
        rows.iter()
            .find(|(id, _, _)| *id == wallet.0)
            .map(|&(_, balance, system)| (balance, system))
            .ok_or(Error::UnknownWallet(wallet))
    };
    let (from_balance, from_system) = find(request.from)?;
    let (to_balance, _) = find(request.to)?;

    let from_after = from_balance
        .checked_sub(request.amount)
        .ok_or(Error::Overflow(request.from))?;
    if from_after < 0 && !from_system {
        return Err(Error::InsufficientFunds {
            wallet: request.from,
            balance: from_balance,
            amount: request.amount,
        });
    }
    let to_after = to_balance
        .checked_add(request.amount)
        .ok_or(Error::Overflow(request.to))?;

    // Each entry applies itself to its wallet (trigger `ledger_entries_apply`), which checks `balance_after`.
    for (wallet, delta, after) in [
        (request.from, -request.amount, from_after),
        (request.to, request.amount, to_after),
    ] {
        sqlx::query(
            "INSERT INTO ledger_entries (operation_id, wallet_id, delta, balance_after) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(operation)
        .bind(wallet.0)
        .bind(delta)
        .bind(after)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(TransferOutcome::Applied(OperationId(operation)))
}

/// The outcome for a key that is already in the ledger: the same transfer, or a different request reusing the key.
async fn previous_outcome(pool: &PgPool, request: &Transfer<'_>) -> Result<TransferOutcome, Error> {
    let (operation, reason): (i64, String) =
        sqlx::query_as("SELECT id, reason FROM operations WHERE idempotency_key = $1")
            .bind(request.idempotency_key)
            .fetch_one(pool)
            .await?;
    let entries: Vec<(i64, i64)> =
        sqlx::query_as("SELECT wallet_id, delta FROM ledger_entries WHERE operation_id = $1 ORDER BY delta")
            .bind(operation)
            .fetch_all(pool)
            .await?;
    let same = reason == request.reason
        && entries == [(request.from.0, -request.amount), (request.to.0, request.amount)];
    if same {
        Ok(TransferOutcome::AlreadyApplied(OperationId(operation)))
    } else {
        Err(Error::KeyReused(OperationId(operation)))
    }
}

async fn insert_wallet(
    tx: &mut Transaction<'_, Postgres>,
    character: Option<i64>,
    kind: &str,
) -> Result<WalletId, Error> {
    let id: i64 = sqlx::query_scalar("INSERT INTO wallets (character_id, kind) VALUES ($1, $2) RETURNING id")
        .bind(character)
        .bind(kind)
        .fetch_one(&mut **tx)
        .await?;
    Ok(WalletId(id))
}

/// Why a storage call failed.
#[derive(Debug)]
pub enum Error {
    /// The amount was zero or negative.
    InvalidAmount(i64),
    /// The source and the destination are the same wallet.
    SameWallet(WalletId),
    /// No wallet has this id.
    UnknownWallet(WalletId),
    /// A character wallet holds less than the amount.
    InsufficientFunds {
        wallet: WalletId,
        balance: i64,
        amount: i64,
    },
    /// The new balance does not fit in `i64`.
    Overflow(WalletId),
    /// The idempotency key belongs to an earlier operation with different parameters.
    KeyReused(OperationId),
    /// A migration failed.
    Migrate(MigrateError),
    /// The database returned an error.
    Database(sqlx::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAmount(amount) => write!(f, "amount {amount} is not positive"),
            Self::SameWallet(wallet) => write!(f, "transfer from wallet {} to itself", wallet.0),
            Self::UnknownWallet(wallet) => write!(f, "wallet {} does not exist", wallet.0),
            Self::InsufficientFunds {
                wallet,
                balance,
                amount,
            } => write!(f, "wallet {} holds {balance}, needs {amount}", wallet.0),
            Self::Overflow(wallet) => write!(f, "balance of wallet {} would overflow", wallet.0),
            Self::KeyReused(operation) => write!(
                f,
                "idempotency key already used by operation {} with different parameters",
                operation.0
            ),
            Self::Migrate(error) => write!(f, "migration failed: {error}"),
            Self::Database(error) => write!(f, "database error: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Migrate(error) => Some(error),
            _ => None,
        }
    }
}

impl From<MigrateError> for Error {
    fn from(error: MigrateError) -> Self {
        Self::Migrate(error)
    }
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}
