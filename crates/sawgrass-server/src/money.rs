//! Players' money in the ledger's single writer (task DB-8, ADR 0003).
//!
//! With a ledger, every player who connects gets a character wallet. The wallet is funded with a starting amount from
//! the server's system wallet once the player has a place in the world, so a join the server refuses moves no
//! money, and every balance sums to zero.
//!
//! A wallet belongs to one connection (`player/{run}/{connection}`), not to a person: reconnecting opens a new one
//! with a fresh starting amount, and the old one stays in the ledger unused. That is what load tests need. Wallets
//! per character come with persistent character identities in the roleplay base (roadmap step 8); until then
//! `--ledger` is for load tests, not for keeping players' money. A client pays
//! another player with [`ClientEvent::Pay`](sawgrass_protocol::game::ClientEvent::Pay). That is the load-test path
//! for money until resources move it through the scripting API, so the server takes the event only when started
//! for it (`--money-events`).
//!
//! The ledger answers only after the batch that holds an operation is on disk. Connection tasks await those answers
//! through the writer's callbacks, holding no thread while a batch commits; the tick thread only keeps the map
//! from entity ids to wallets that payments look up.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use sawgrass_protocol::game::PayResult;
use sawgrass_store::{Error, Handle, Kind, Operation, Outcome, WalletId};
use tokio::sync::oneshot;

/// Players' wallets, and how payments between them reach the ledger.
#[derive(Debug)]
pub struct Money {
    ledger: Handle,
    treasury: WalletId,
    start_cash: i64,
    /// Distinguishes this run's keys from earlier runs': connection ids restart at 0.
    run: u64,
    payees: RwLock<HashMap<u32, WalletId>>,
}

impl Money {
    /// Opens (or, after a restart, finds) the system wallet that funds new players with `start_cash` each. `run`
    /// must differ between runs of the server within the ledger's key window; the start time in milliseconds does.
    pub fn open(ledger: Handle, start_cash: i64, run: u64) -> Result<Arc<Self>, Error> {
        let treasury = match ledger
            .submit(Operation::OpenWallet {
                key: "treasury".to_owned(),
                kind: Kind::System,
            })
            .wait()?
        {
            Outcome::Opened { wallet, .. } => wallet,
            Outcome::Transferred { .. } => {
                return Err(Error::Corrupt("treasury key names a transfer".to_owned()));
            }
        };
        Ok(Arc::new(Self {
            ledger,
            treasury,
            start_cash,
            run,
            payees: RwLock::new(HashMap::new()),
        }))
    }

    /// Opens connection `connection`'s wallet, once it is on disk. It holds nothing until [`Money::register`].
    pub(crate) async fn open_wallet(&self, connection: u32) -> Result<WalletId, Error> {
        let opened = submit(
            &self.ledger,
            Operation::OpenWallet {
                key: format!("player/{}/{connection}", self.run),
                kind: Kind::Character,
            },
        );
        match opened.await? {
            Outcome::Opened { wallet, .. } => Ok(wallet),
            Outcome::Transferred { .. } => Err(Error::Corrupt("wallet key names a transfer".to_owned())),
        }
    }

    /// Pays `amount` from `from` to the player with entity id `to`, once per client `key`, and returns the result
    /// with the payer's balance.
    pub(crate) async fn pay(&self, from: WalletId, key: u64, to: u32, amount: u32) -> (PayResult, i64) {
        let payee = self.payees.read().ok().and_then(|p| p.get(&to).copied());
        let Some(payee) = payee else {
            return (
                PayResult::UnknownPayee,
                balance(&self.ledger, from).await.unwrap_or(0),
            );
        };
        // Both requests go in together: the balance is read after the transfer, in the same batch.
        let paid = submit(
            &self.ledger,
            Operation::Transfer {
                // Keys are per payer, so one client cannot spend another's key.
                key: format!("pay/{}/{key}", from.0),
                from,
                to: payee,
                amount: i64::from(amount),
            },
        );
        let after = balance(&self.ledger, from);
        let result = match paid.await {
            Ok(_) => PayResult::Done,
            Err(e) => result_of(&e),
        };
        (result, after.await.unwrap_or(0))
    }

    /// Records that entity `wire`, connection `connection`'s player, pays and is paid through `wallet`, and funds
    /// the wallet with the starting amount. Called on the tick thread once the player has a slot; it does not wait
    /// for the disk. The funding is submitted before the player hears it joined, so the writer applies it before
    /// any payment the player makes. A failed funding means the writer has stopped, and every payment fails too.
    pub(crate) fn register(&self, wire: u32, connection: u32, wallet: WalletId) {
        if self.start_cash > 0 {
            self.ledger.submit_then(
                Operation::Transfer {
                    key: format!("start/{}/{connection}", self.run),
                    from: self.treasury,
                    to: wallet,
                    amount: self.start_cash,
                },
                |_| {},
            );
        }
        if let Ok(mut payees) = self.payees.write() {
            payees.insert(wire, wallet);
        }
    }

    /// Forgets entity `wire`: payments to it are refused until it is registered again.
    pub(crate) fn unregister(&self, wire: u32) {
        if let Ok(mut payees) = self.payees.write() {
            payees.remove(&wire);
        }
    }

    /// The balance of `wallet`, after every operation submitted before this call. Blocks.
    pub fn balance(&self, wallet: WalletId) -> Result<i64, Error> {
        self.ledger.balance(wallet).wait()
    }

    /// The ledger handle, for its stats.
    #[must_use]
    pub fn ledger(&self) -> &Handle {
        &self.ledger
    }
}

fn result_of(error: &Error) -> PayResult {
    match error {
        Error::InsufficientFunds { .. } => PayResult::InsufficientFunds,
        Error::UnknownWallet(_) => PayResult::UnknownPayee,
        Error::InvalidAmount(_) | Error::SameWallet(_) | Error::Overflow(_) | Error::KeyReused(_) => {
            PayResult::Invalid
        }
        Error::Store(_) | Error::Corrupt(_) | Error::Stopped => PayResult::Failed,
    }
}

/// Submits `operation` and returns its answer without holding a thread while the batch commits.
fn submit(ledger: &Handle, operation: Operation) -> impl Future<Output = Result<Outcome, Error>> + use<> {
    let (tx, rx) = oneshot::channel();
    ledger.submit_then(operation, move |answer| {
        let _ = tx.send(answer);
    });
    async move { rx.await.unwrap_or(Err(Error::Stopped)) }
}

/// The balance of `wallet` once everything submitted before is durable, without holding a thread.
fn balance(ledger: &Handle, wallet: WalletId) -> impl Future<Output = Result<i64, Error>> + use<> {
    let (tx, rx) = oneshot::channel();
    ledger.balance_then(wallet, move |answer| {
        let _ = tx.send(answer);
    });
    async move { rx.await.unwrap_or(Err(Error::Stopped)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sawgrass_store::{Config, RocksStore, Writer};

    #[test]
    fn a_wallet_is_funded_only_once_its_player_has_joined() {
        let dir = std::env::temp_dir().join(format!("sawgrass-money-fund-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let writer =
            Writer::start(RocksStore::open(&dir).expect("store"), Config::default()).expect("writer");
        let money = Money::open(writer.handle(), 1_000, 7).expect("money");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let refused = runtime.block_on(money.open_wallet(1)).expect("open");
        let joined = runtime.block_on(money.open_wallet(2)).expect("open");
        // Connection 1 never got a slot: the tick never registers it. Connection 2 did, and registers twice (a
        // repeated join message), which funds it once.
        money.register(5, 2, joined);
        money.register(5, 2, joined);
        assert_eq!(money.balance(refused), Ok(0));
        assert_eq!(money.balance(joined), Ok(1_000));
        assert_eq!(money.balance(money.treasury), Ok(-1_000));
        drop(money);
        writer.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
