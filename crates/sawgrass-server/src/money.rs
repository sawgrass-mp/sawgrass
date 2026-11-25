//! Players' money in the ledger's single writer (task DB-8, ADR 0003).
//!
//! With a ledger, every player who connects gets a character wallet. The wallet is funded with a starting amount from
//! the server's system wallet, so money only moves between wallets and every balance sums to zero. A client pays
//! another player with [`ClientEvent::Pay`](sawgrass_protocol::game::ClientEvent::Pay). That is the load-test path
//! for money until resources move it through the scripting API, so the server takes the event only when started
//! for it (`--money-events`).
//!
//! The ledger answers only after the batch that holds an operation is on disk, so its calls block. Connection
//! tasks wait for them on tokio's blocking pool, never on the tick thread: the tick thread only keeps the map from
//! entity ids to wallets that payments look up.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use sawgrass_protocol::game::PayResult;
use sawgrass_store::{Error, Handle, Kind, Operation, Outcome, Pending, WalletId};

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

    /// Opens connection `connection`'s wallet and funds it. Waits for the disk, off the async threads.
    pub(crate) async fn open_wallet(self: &Arc<Self>, connection: u32) -> Result<WalletId, Error> {
        let money = self.clone();
        blocking(move || {
            let wallet = match money
                .ledger
                .submit(Operation::OpenWallet {
                    key: format!("player/{}/{connection}", money.run),
                    kind: Kind::Character,
                })
                .wait()?
            {
                Outcome::Opened { wallet, .. } => wallet,
                Outcome::Transferred { .. } => {
                    return Err(Error::Corrupt("wallet key names a transfer".to_owned()));
                }
            };
            if money.start_cash > 0 {
                money
                    .ledger
                    .submit(Operation::Transfer {
                        key: format!("start/{}/{connection}", money.run),
                        from: money.treasury,
                        to: wallet,
                        amount: money.start_cash,
                    })
                    .wait()?;
            }
            Ok(wallet)
        })
        .await
    }

    /// Pays `amount` from `from` to the player with entity id `to`, once per client `key`, and returns the result
    /// with the payer's balance.
    pub(crate) async fn pay(
        self: &Arc<Self>,
        from: WalletId,
        key: u64,
        to: u32,
        amount: u32,
    ) -> (PayResult, i64) {
        let payee = self.payees.read().ok().and_then(|p| p.get(&to).copied());
        let money = self.clone();
        blocking(move || {
            let result = match payee {
                None => PayResult::UnknownPayee,
                Some(payee) => match money
                    .ledger
                    .submit(Operation::Transfer {
                        // Keys are per payer, so one client cannot spend another's key.
                        key: format!("pay/{}/{key}", from.0),
                        from,
                        to: payee,
                        amount: i64::from(amount),
                    })
                    .wait()
                {
                    Ok(_) => PayResult::Done,
                    Err(e) => result_of(&e),
                },
            };
            let balance = wait_or(money.ledger.balance(from), 0);
            Ok((result, balance))
        })
        .await
        .unwrap_or((PayResult::Failed, 0))
    }

    /// Records that entity `wire` pays and is paid through `wallet`.
    pub(crate) fn register(&self, wire: u32, wallet: WalletId) {
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

fn wait_or<T>(pending: Pending<T>, fallback: T) -> T {
    pending.wait().unwrap_or(fallback)
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or(Err(Error::Stopped))
}
