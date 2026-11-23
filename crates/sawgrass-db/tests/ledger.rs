//! Ledger tests against a disposable PostgreSQL database.
//!
//! Set `SAWGRASS_TEST_DATABASE_URL` to a server where the user may create databases, for example
//! `postgres://postgres:postgres@localhost/postgres`. Each test creates its own database, runs the migrations and
//! drops it at the end. Without the variable every test prints `SKIP` and passes.

use std::sync::atomic::{AtomicU32, Ordering};

use sqlx::AssertSqlSafe;

use sawgrass_db::{
    Error, PgPool, Transfer, TransferOutcome, WalletId, balance, connect, create_character, migrate,
    system_wallet, transfer,
};

const URL_VAR: &str = "SAWGRASS_TEST_DATABASE_URL";

/// A database that exists for one test.
struct TestDb {
    admin: PgPool,
    name: String,
    pool: PgPool,
}

impl TestDb {
    async fn create(test: &str) -> Option<Self> {
        let Ok(url) = std::env::var(URL_VAR) else {
            println!("SKIP {test}: {URL_VAR} is not set");
            return None;
        };
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "sawgrass_test_{}_{}_{test}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = connect(&url, 2).await.expect("connect to the admin database");
        // The name is built above from digits and a fixed test label, never from input.
        sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&admin)
            .await
            .expect("create the test database");
        let mut options: sqlx::postgres::PgConnectOptions = url.parse().expect("parse the URL");
        options = options.database(&name);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(16)
            .connect_with(options)
            .await
            .expect("connect to the test database");
        migrate(&pool).await.expect("run the migrations");
        Some(Self { admin, name, pool })
    }

    async fn drop(self) {
        self.pool.close().await;
        sqlx::query(AssertSqlSafe(format!("DROP DATABASE {} WITH (FORCE)", self.name)))
            .execute(&self.admin)
            .await
            .expect("drop the test database");
    }
}

/// Checks the invariants that must hold after any sequence of operations.
async fn assert_ledger_consistent(pool: &PgPool) {
    let total: Option<i64> = sqlx::query_scalar("SELECT sum(balance)::BIGINT FROM wallets")
        .fetch_one(pool)
        .await
        .expect("sum balances");
    assert_eq!(total.unwrap_or(0), 0, "all balances must sum to zero");

    let negative: i64 =
        sqlx::query_scalar("SELECT count(*) FROM wallets WHERE character_id IS NOT NULL AND balance < 0")
            .fetch_one(pool)
            .await
            .expect("count negative wallets");
    assert_eq!(negative, 0, "no character wallet may be negative");

    let mismatched: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM wallets w \
         WHERE w.balance <> COALESCE((SELECT sum(delta) FROM ledger_entries e WHERE e.wallet_id = w.id), 0)",
    )
    .fetch_one(pool)
    .await
    .expect("compare balances with the ledger");
    assert_eq!(
        mismatched, 0,
        "every balance must equal the sum of its ledger entries"
    );

    let unbalanced: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT operation_id FROM ledger_entries \
         GROUP BY operation_id HAVING sum(delta) <> 0 OR count(*) <> 2) t",
    )
    .fetch_one(pool)
    .await
    .expect("check operations");
    assert_eq!(
        unbalanced, 0,
        "every operation must have two entries summing to zero"
    );
}

async fn fund(pool: &PgPool, treasury: WalletId, wallet: WalletId, amount: i64) {
    let key = format!("fund-{}", wallet.0);
    let outcome = transfer(
        pool,
        &Transfer {
            idempotency_key: &key,
            from: treasury,
            to: wallet,
            amount,
            reason: "test_funding",
        },
    )
    .await
    .expect("fund the wallet");
    assert!(matches!(outcome, TransferOutcome::Applied(_)));
}

/// A small deterministic generator, so a failure can be reproduced.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_transfers_never_create_or_lose_money() {
    let Some(db) = TestDb::create("concurrent").await else {
        return;
    };
    let pool = &db.pool;
    let treasury = system_wallet(pool, "treasury").await.expect("treasury");

    const CHARACTERS: usize = 8;
    const START: i64 = 10_000;
    let mut wallets = Vec::new();
    for i in 0..CHARACTERS {
        let c = create_character(pool, "Test", &format!("Character{i}"))
            .await
            .expect("create character");
        fund(pool, treasury, c.cash, START).await;
        wallets.push(c.cash);
        wallets.push(c.bank);
    }

    let mut tasks = Vec::new();
    for worker in 0..16u64 {
        let pool = pool.clone();
        let wallets = wallets.clone();
        tasks.push(tokio::spawn(async move {
            let mut state = 0x9e37_79b9_7f4a_7c15 ^ (worker + 1);
            let (mut applied, mut refused) = (0u32, 0u32);
            for n in 0..200 {
                let from = wallets[(next(&mut state) as usize) % wallets.len()];
                let to = wallets[(next(&mut state) as usize) % wallets.len()];
                if from == to {
                    continue;
                }
                let amount = (next(&mut state) % 3_000) as i64 + 1;
                let key = format!("w{worker}-{n}");
                let request = Transfer {
                    idempotency_key: &key,
                    from,
                    to,
                    amount,
                    reason: "test_random",
                };
                match transfer(&pool, &request).await {
                    Ok(TransferOutcome::Applied(_)) => applied += 1,
                    Err(Error::InsufficientFunds { .. }) => refused += 1,
                    other => panic!("unexpected result {other:?}"),
                }
            }
            (applied, refused)
        }));
    }
    let mut applied = 0;
    for task in tasks {
        applied += task.await.expect("worker").0;
    }
    assert!(applied > 0, "some transfers must succeed");

    let mut sum = 0;
    for wallet in &wallets {
        sum += balance(pool, *wallet).await.expect("balance");
    }
    assert_eq!(
        sum,
        START * CHARACTERS as i64,
        "character money must be conserved"
    );
    assert_eq!(balance(pool, treasury).await.expect("treasury"), -sum);
    assert_ledger_consistent(pool).await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_withdrawals_never_overdraw() {
    let Some(db) = TestDb::create("overdraw").await else {
        return;
    };
    let pool = &db.pool;
    let treasury = system_wallet(pool, "treasury").await.expect("treasury");
    let shop = system_wallet(pool, "shop").await.expect("shop");
    let buyer = create_character(pool, "Big", "Spender")
        .await
        .expect("create character");
    fund(pool, treasury, buyer.cash, 1_000).await;

    let mut tasks = Vec::new();
    for n in 0..50 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            let key = format!("buy-{n}");
            transfer(
                &pool,
                &Transfer {
                    idempotency_key: &key,
                    from: buyer.cash,
                    to: shop,
                    amount: 30,
                    reason: "test_purchase",
                },
            )
            .await
        }));
    }
    let mut applied = 0;
    for task in tasks {
        match task.await.expect("task") {
            Ok(TransferOutcome::Applied(_)) => applied += 1,
            Err(Error::InsufficientFunds { .. }) => {}
            other => panic!("unexpected result {other:?}"),
        }
    }
    assert_eq!(applied, 33, "1000 buys exactly 33 items at 30");
    assert_eq!(balance(pool, buyer.cash).await.expect("balance"), 10);
    assert_eq!(balance(pool, shop).await.expect("balance"), 990);
    assert_ledger_consistent(pool).await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_retried_transfer_is_applied_once() {
    let Some(db) = TestDb::create("idempotent").await else {
        return;
    };
    let pool = &db.pool;
    let treasury = system_wallet(pool, "treasury").await.expect("treasury");
    let c = create_character(pool, "Pay", "Day").await.expect("create");

    let mut tasks = Vec::new();
    for _ in 0..20 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            transfer(
                &pool,
                &Transfer {
                    idempotency_key: "salary-2026-11-23",
                    from: treasury,
                    to: c.bank,
                    amount: 500,
                    reason: "salary",
                },
            )
            .await
            .expect("transfer")
        }));
    }
    let mut applied = Vec::new();
    let mut repeated = Vec::new();
    for task in tasks {
        match task.await.expect("task") {
            TransferOutcome::Applied(op) => applied.push(op),
            TransferOutcome::AlreadyApplied(op) => repeated.push(op),
        }
    }
    assert_eq!(applied.len(), 1);
    assert!(repeated.iter().all(|op| *op == applied[0]));
    assert_eq!(balance(pool, c.bank).await.expect("balance"), 500);

    let reused = transfer(
        pool,
        &Transfer {
            idempotency_key: "salary-2026-11-23",
            from: treasury,
            to: c.bank,
            amount: 5_000,
            reason: "salary",
        },
    )
    .await;
    assert!(matches!(reused, Err(Error::KeyReused(op)) if op == applied[0]));
    assert_eq!(balance(pool, c.bank).await.expect("balance"), 500);
    assert_ledger_consistent(pool).await;
    db.drop().await;
}

#[tokio::test]
async fn invalid_requests_change_nothing() {
    let Some(db) = TestDb::create("invalid").await else {
        return;
    };
    let pool = &db.pool;
    let c = create_character(pool, "Broke", "Person").await.expect("create");
    let request = |amount, from, to| Transfer {
        idempotency_key: "k",
        from,
        to,
        amount,
        reason: "test",
    };
    assert!(matches!(
        transfer(pool, &request(0, c.cash, c.bank)).await,
        Err(Error::InvalidAmount(0))
    ));
    assert!(matches!(
        transfer(pool, &request(-5, c.cash, c.bank)).await,
        Err(Error::InvalidAmount(-5))
    ));
    assert!(matches!(
        transfer(pool, &request(5, c.cash, c.cash)).await,
        Err(Error::SameWallet(_))
    ));
    assert!(matches!(
        transfer(pool, &request(5, c.cash, WalletId(999_999))).await,
        Err(Error::UnknownWallet(WalletId(999_999)))
    ));
    assert!(matches!(
        transfer(pool, &request(5, c.cash, c.bank)).await,
        Err(Error::InsufficientFunds {
            balance: 0,
            amount: 5,
            ..
        })
    ));
    let operations: i64 = sqlx::query_scalar("SELECT count(*) FROM operations")
        .fetch_one(pool)
        .await
        .expect("count operations");
    assert_eq!(operations, 0, "a refused transfer leaves no operation behind");
    assert_ledger_consistent(pool).await;
    db.drop().await;
}

/// The schema itself refuses changes that bypass `transfer`.
#[tokio::test]
async fn the_database_refuses_untracked_changes() {
    let Some(db) = TestDb::create("triggers").await else {
        return;
    };
    let pool = &db.pool;
    let treasury = system_wallet(pool, "treasury").await.expect("treasury");
    let c = create_character(pool, "Sneaky", "Admin").await.expect("create");
    fund(pool, treasury, c.cash, 100).await;

    let refused = [
        format!("UPDATE wallets SET balance = 1000000 WHERE id = {}", c.cash.0),
        "INSERT INTO wallets (kind, balance) VALUES ('mint', 5)".to_owned(),
        format!(
            "UPDATE wallets SET character_id = NULL, kind = 'stolen' WHERE id = {}",
            c.cash.0
        ),
        format!("DELETE FROM wallets WHERE id = {}", c.bank.0),
        "UPDATE ledger_entries SET delta = delta * 2".to_owned(),
        "DELETE FROM ledger_entries".to_owned(),
        "TRUNCATE ledger_entries, operations, wallets CASCADE".to_owned(),
        "UPDATE operations SET reason = 'hidden'".to_owned(),
        "DELETE FROM operations".to_owned(),
    ];
    for statement in &refused {
        let result = sqlx::query(AssertSqlSafe(statement.as_str())).execute(pool).await;
        assert!(result.is_err(), "the database accepted: {statement}");
    }

    // An operation whose entries do not sum to zero fails at commit, even with a matching wallet update.
    let mut tx = pool.begin().await.expect("begin");
    let op: i64 = sqlx::query_scalar(
        "INSERT INTO operations (idempotency_key, reason) VALUES ('forged', 'test') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("insert operation");
    sqlx::query("INSERT INTO ledger_entries (operation_id, wallet_id, delta, balance_after) VALUES ($1, $2, 900, 1000)")
        .bind(op)
        .bind(c.cash.0)
        .execute(&mut *tx)
        .await
        .expect("insert entry");
    sqlx::query("UPDATE wallets SET balance = 1000 WHERE id = $1")
        .bind(c.cash.0)
        .execute(&mut *tx)
        .await
        .expect("update matches the newest entry");
    assert!(
        tx.commit().await.is_err(),
        "an unbalanced operation must not commit"
    );

    assert_eq!(balance(pool, c.cash).await.expect("balance"), 100);
    assert_ledger_consistent(pool).await;
    db.drop().await;
}
