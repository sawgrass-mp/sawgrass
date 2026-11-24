//! The Sawgrass money ledger (migrations/0001) re-implemented as a SpacetimeDB module, for a like-for-like load
//! test: same tables, same checks, one transaction per transfer.
use spacetimedb::{reducer, table, ReducerContext, Table};

// Tables are public only so that the load client and `spacetime sql` can read wallet ids and check results.
#[table(accessor = character, public)]
pub struct Character {
    #[primary_key]
    #[auto_inc]
    id: u64,
    first_name: String,
    last_name: String,
}

#[table(accessor = wallet, public)]
pub struct Wallet {
    #[primary_key]
    #[auto_inc]
    id: u64,
    /// 0 for a system wallet.
    #[index(btree)]
    character_id: u64,
    kind: String,
    balance: i64,
}

#[table(accessor = operation, public)]
pub struct Operation {
    #[primary_key]
    #[auto_inc]
    id: u64,
    #[unique]
    idempotency_key: String,
    reason: String,
}

#[table(accessor = ledger_entry, public)]
pub struct LedgerEntry {
    #[primary_key]
    #[auto_inc]
    id: u64,
    #[index(btree)]
    operation_id: u64,
    #[index(btree)]
    wallet_id: u64,
    delta: i64,
    balance_after: i64,
}

fn new_wallet(ctx: &ReducerContext, character_id: u64, kind: &str) -> u64 {
    ctx.db
        .wallet()
        .insert(Wallet {
            id: 0,
            character_id,
            kind: kind.to_string(),
            balance: 0,
        })
        .id
}

/// Creates the treasury (wallet 1), the shop (wallet 2) and `characters` characters whose bank wallets the
/// treasury funds with `amount` each.
#[reducer]
pub fn seed(ctx: &ReducerContext, characters: u32, amount: i64) -> Result<(), String> {
    let treasury = new_wallet(ctx, 0, "treasury");
    new_wallet(ctx, 0, "shop");
    for i in 0..characters {
        let c = ctx.db.character().insert(Character {
            id: 0,
            first_name: "Load".into(),
            last_name: format!("Bot{i}"),
        });
        new_wallet(ctx, c.id, "cash");
        let bank = new_wallet(ctx, c.id, "bank");
        apply(ctx, &format!("seed-{bank}"), treasury, bank, amount, "seed")?;
    }
    Ok(())
}

#[reducer]
pub fn transfer(
    ctx: &ReducerContext,
    idempotency_key: String,
    from: u64,
    to: u64,
    amount: i64,
    reason: String,
) -> Result<(), String> {
    apply(ctx, &idempotency_key, from, to, amount, &reason)
}

fn apply(
    ctx: &ReducerContext,
    key: &str,
    from: u64,
    to: u64,
    amount: i64,
    reason: &str,
) -> Result<(), String> {
    if amount <= 0 {
        return Err(format!("amount {amount} is not positive"));
    }
    if from == to {
        return Err("same wallet".into());
    }
    if let Some(op) = ctx.db.operation().idempotency_key().find(key.to_string()) {
        let mut entries: Vec<(u64, i64)> = ctx
            .db
            .ledger_entry()
            .operation_id()
            .filter(op.id)
            .map(|e| (e.wallet_id, e.delta))
            .collect();
        entries.sort_by_key(|e| e.1);
        return if op.reason == reason && entries == [(from, -amount), (to, amount)] {
            Ok(())
        } else {
            Err(format!("idempotency key reused by operation {}", op.id))
        };
    }
    let wallets = ctx.db.wallet().id();
    let mut src = wallets
        .find(from)
        .ok_or_else(|| format!("wallet {from} does not exist"))?;
    let mut dst = wallets
        .find(to)
        .ok_or_else(|| format!("wallet {to} does not exist"))?;
    let src_after = src.balance.checked_sub(amount).ok_or("overflow")?;
    if src_after < 0 && src.character_id != 0 {
        return Err(format!(
            "wallet {from} holds {}, needs {amount}",
            src.balance
        ));
    }
    let dst_after = dst.balance.checked_add(amount).ok_or("overflow")?;
    let op = ctx.db.operation().insert(Operation {
        id: 0,
        idempotency_key: key.to_string(),
        reason: reason.to_string(),
    });
    for (wallet, delta, after) in [(from, -amount, src_after), (to, amount, dst_after)] {
        ctx.db.ledger_entry().insert(LedgerEntry {
            id: 0,
            operation_id: op.id,
            wallet_id: wallet,
            delta,
            balance_after: after,
        });
    }
    src.balance = src_after;
    dst.balance = dst_after;
    wallets.update(src);
    wallets.update(dst);
    Ok(())
}

/// Fails unless every invariant of the ledger holds: balances sum to zero, no character wallet is negative, and
/// every balance equals the sum of its ledger entries.
#[reducer]
pub fn check(ctx: &ReducerContext) -> Result<(), String> {
    let mut total: i128 = 0;
    let mut from_ledger = std::collections::HashMap::<u64, i64>::new();
    for e in ctx.db.ledger_entry().iter() {
        *from_ledger.entry(e.wallet_id).or_default() += e.delta;
    }
    for w in ctx.db.wallet().iter() {
        total += i128::from(w.balance);
        if w.character_id != 0 && w.balance < 0 {
            return Err(format!("wallet {} is negative", w.id));
        }
        if from_ledger.get(&w.id).copied().unwrap_or(0) != w.balance {
            return Err(format!("wallet {} does not match its ledger", w.id));
        }
    }
    if total != 0 {
        return Err(format!("balances sum to {total}"));
    }
    log::info!(
        "ledger consistent: {} operations",
        ctx.db.operation().count()
    );
    Ok(())
}
