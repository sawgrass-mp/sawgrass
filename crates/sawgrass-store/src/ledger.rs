//! The money ledger's rules, applied in memory by the writer.
//!
//! The rules come from `sawgrass-db` (DB-1): amounts are positive whole minor units; money moves only between two
//! different wallets that exist; a character wallet never goes negative, and only system wallets (the treasury, a
//! shop) may, so every operation sums to zero and so do all balances; an idempotency key names one operation, a retry
//! with the same parameters gets the original outcome and other parameters are an error.

use std::collections::{HashMap, VecDeque};
use std::fmt;

/// A wallet's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WalletId(pub u64);

/// A committed operation's id: 1 for the first, then in commit order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OperationId(pub u64);

/// Who a wallet belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A character's: never below zero.
    Character,
    /// The economy's own (treasury, shops): where money enters and leaves, so it may go negative.
    System,
}

/// A change to state with value, as a caller submits it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Operation {
    /// Opens an empty wallet.
    OpenWallet {
        /// Idempotency key.
        key: String,
        /// Whose it is.
        kind: Kind,
    },
    /// Moves `amount` from one wallet to another.
    Transfer {
        /// Idempotency key.
        key: String,
        /// Paying wallet.
        from: WalletId,
        /// Receiving wallet.
        to: WalletId,
        /// Minor units, above zero.
        amount: i64,
    },
}

impl Operation {
    /// The idempotency key.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::OpenWallet { key, .. } | Self::Transfer { key, .. } => key,
        }
    }
}

/// What an operation did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The wallet was opened.
    Opened {
        /// The operation.
        operation: OperationId,
        /// The new wallet.
        wallet: WalletId,
    },
    /// The money moved.
    Transferred {
        /// The operation.
        operation: OperationId,
    },
}

/// One committed operation, as it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Its id.
    pub id: OperationId,
    /// When it was applied, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// What the caller asked.
    pub operation: Operation,
    /// What it did.
    pub outcome: Outcome,
}

/// A wallet's state, as it is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wallet {
    /// Its id.
    pub id: WalletId,
    /// Whose it is.
    pub kind: Kind,
    /// Its balance in minor units.
    pub balance: i64,
}

/// Why an operation was refused, or could not be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The amount is zero or negative.
    InvalidAmount(i64),
    /// A transfer from a wallet to itself.
    SameWallet(WalletId),
    /// No wallet has this id.
    UnknownWallet(WalletId),
    /// A character wallet would go below zero.
    InsufficientFunds {
        /// The paying wallet.
        wallet: WalletId,
        /// What it holds.
        balance: i64,
        /// What was asked.
        amount: i64,
    },
    /// A balance would leave the `i64` range.
    Overflow(WalletId),
    /// The idempotency key belongs to an earlier operation with other parameters.
    KeyReused(OperationId),
    /// The store failed; nothing of this batch is known to be on disk.
    Store(String),
    /// The stored state breaks a ledger rule; the store must be restored, not used.
    Corrupt(String),
    /// The writer has stopped, after a store failure or a shutdown.
    Stopped,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAmount(a) => write!(f, "amount {a} is not above zero"),
            Self::SameWallet(w) => write!(f, "transfer from wallet {} to itself", w.0),
            Self::UnknownWallet(w) => write!(f, "no wallet {}", w.0),
            Self::InsufficientFunds {
                wallet,
                balance,
                amount,
            } => write!(f, "wallet {} holds {balance}, needs {amount}", wallet.0),
            Self::Overflow(w) => write!(f, "balance of wallet {} out of range", w.0),
            Self::KeyReused(op) => write!(
                f,
                "idempotency key already used by operation {} with other parameters",
                op.0
            ),
            Self::Store(e) => write!(f, "store: {e}"),
            Self::Corrupt(e) => write!(f, "stored state is corrupt: {e}"),
            Self::Stopped => write!(f, "the writer has stopped"),
        }
    }
}

impl std::error::Error for Error {}

/// What a new operation changed: its record and the wallets it touched, to be stored.
pub(crate) type Change = (Record, Vec<Wallet>);

/// Balances and recent idempotency keys, in memory.
#[derive(Debug, Default)]
pub(crate) struct Ledger {
    wallets: HashMap<WalletId, (Kind, i64)>,
    next_wallet: u64,
    last_operation: u64,
    keys: HashMap<String, (Operation, Outcome)>,
    /// Keys in the order they were added, with their time, to forget them after the window.
    ages: VecDeque<(u64, String)>,
}

impl Ledger {
    /// Rebuilds the ledger from stored wallets and the records of the idempotency window, checking the rules that
    /// must hold of any stored state.
    pub(crate) fn load(
        wallets: Vec<Wallet>,
        last_operation: u64,
        recent: Vec<Record>,
    ) -> Result<Self, Error> {
        let mut ledger = Self {
            last_operation,
            ..Self::default()
        };
        let mut total: i128 = 0;
        for w in wallets {
            if w.kind == Kind::Character && w.balance < 0 {
                return Err(Error::Corrupt(format!(
                    "character wallet {} holds {}",
                    w.id.0, w.balance
                )));
            }
            total += i128::from(w.balance);
            ledger.next_wallet = ledger.next_wallet.max(w.id.0 + 1);
            ledger.wallets.insert(w.id, (w.kind, w.balance));
        }
        if total != 0 {
            return Err(Error::Corrupt(format!("balances sum to {total}")));
        }
        for r in recent {
            ledger.remember(&r);
        }
        Ok(ledger)
    }

    /// The balance of `wallet`.
    pub(crate) fn balance(&self, wallet: WalletId) -> Result<i64, Error> {
        self.wallets
            .get(&wallet)
            .map(|&(_, b)| b)
            .ok_or(Error::UnknownWallet(wallet))
    }

    /// Checks `operation` and applies it. Returns its outcome, and the record and changed wallets to store when it is
    /// new; a retry of a known key returns the original outcome and nothing to store.
    pub(crate) fn apply(
        &mut self,
        operation: Operation,
        at_ms: u64,
    ) -> Result<(Outcome, Option<Change>), Error> {
        if let Some((earlier, outcome)) = self.keys.get(operation.key()) {
            return if *earlier == operation {
                Ok((*outcome, None))
            } else {
                Err(Error::KeyReused(id_of(outcome)))
            };
        }
        let id = OperationId(self.last_operation + 1);
        let (outcome, changed) = match &operation {
            Operation::OpenWallet { kind, .. } => {
                let wallet = WalletId(self.next_wallet);
                self.next_wallet += 1;
                self.wallets.insert(wallet, (*kind, 0));
                let changed = vec![Wallet {
                    id: wallet,
                    kind: *kind,
                    balance: 0,
                }];
                (
                    Outcome::Opened {
                        operation: id,
                        wallet,
                    },
                    changed,
                )
            }
            Operation::Transfer { from, to, amount, .. } => {
                let changed = self.transfer(*from, *to, *amount)?;
                (Outcome::Transferred { operation: id }, changed)
            }
        };
        self.last_operation = id.0;
        let record = Record {
            id,
            at_ms,
            operation,
            outcome,
        };
        self.remember(&record);
        Ok((outcome, Some((record, changed))))
    }

    fn transfer(&mut self, from: WalletId, to: WalletId, amount: i64) -> Result<Vec<Wallet>, Error> {
        if amount <= 0 {
            return Err(Error::InvalidAmount(amount));
        }
        if from == to {
            return Err(Error::SameWallet(from));
        }
        let &(from_kind, from_balance) = self.wallets.get(&from).ok_or(Error::UnknownWallet(from))?;
        let &(to_kind, to_balance) = self.wallets.get(&to).ok_or(Error::UnknownWallet(to))?;
        let from_after = from_balance.checked_sub(amount).ok_or(Error::Overflow(from))?;
        if from_kind == Kind::Character && from_after < 0 {
            return Err(Error::InsufficientFunds {
                wallet: from,
                balance: from_balance,
                amount,
            });
        }
        let to_after = to_balance.checked_add(amount).ok_or(Error::Overflow(to))?;
        self.wallets.insert(from, (from_kind, from_after));
        self.wallets.insert(to, (to_kind, to_after));
        Ok(vec![
            Wallet {
                id: from,
                kind: from_kind,
                balance: from_after,
            },
            Wallet {
                id: to,
                kind: to_kind,
                balance: to_after,
            },
        ])
    }

    fn remember(&mut self, record: &Record) {
        let key = record.operation.key().to_owned();
        self.keys
            .insert(key.clone(), (record.operation.clone(), record.outcome));
        self.ages.push_back((record.at_ms, key));
    }

    /// Forgets the keys applied before `cutoff_ms`.
    pub(crate) fn forget_before(&mut self, cutoff_ms: u64) {
        while let Some((at, _)) = self.ages.front() {
            if *at >= cutoff_ms {
                break;
            }
            if let Some((_, key)) = self.ages.pop_front() {
                self.keys.remove(&key);
            }
        }
    }
}

fn id_of(outcome: &Outcome) -> OperationId {
    match outcome {
        Outcome::Opened { operation, .. } | Outcome::Transferred { operation } => *operation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(ledger: &mut Ledger, key: &str, kind: Kind) -> WalletId {
        match ledger.apply(
            Operation::OpenWallet {
                key: key.into(),
                kind,
            },
            0,
        ) {
            Ok((Outcome::Opened { wallet, .. }, _)) => wallet,
            other => panic!("{other:?}"),
        }
    }

    fn pay(
        ledger: &mut Ledger,
        key: &str,
        from: WalletId,
        to: WalletId,
        amount: i64,
    ) -> Result<Outcome, Error> {
        let op = Operation::Transfer {
            key: key.into(),
            from,
            to,
            amount,
        };
        ledger.apply(op, 0).map(|(outcome, _)| outcome)
    }

    #[test]
    fn money_moves_by_the_rules() {
        let mut l = Ledger::default();
        let treasury = open(&mut l, "t", Kind::System);
        let (a, b) = (
            open(&mut l, "a", Kind::Character),
            open(&mut l, "b", Kind::Character),
        );
        // The treasury may go negative: that is how money enters.
        pay(&mut l, "mint", treasury, a, 100).expect("mint");
        assert_eq!(l.balance(treasury), Ok(-100));
        pay(&mut l, "ab", a, b, 60).expect("pay");
        assert_eq!((l.balance(a), l.balance(b)), (Ok(40), Ok(60)));
        assert_eq!(
            pay(&mut l, "ab2", a, b, 41),
            Err(Error::InsufficientFunds {
                wallet: a,
                balance: 40,
                amount: 41
            })
        );
        assert_eq!(pay(&mut l, "z", a, b, 0), Err(Error::InvalidAmount(0)));
        assert_eq!(pay(&mut l, "s", a, a, 1), Err(Error::SameWallet(a)));
        assert_eq!(
            pay(&mut l, "u", a, WalletId(99), 1),
            Err(Error::UnknownWallet(WalletId(99)))
        );
        assert_eq!(
            pay(&mut l, "o", treasury, a, i64::MAX),
            Err(Error::Overflow(treasury))
        );
        // Refusals changed nothing.
        assert_eq!(
            (l.balance(a), l.balance(b), l.balance(treasury)),
            (Ok(40), Ok(60), Ok(-100))
        );
    }

    #[test]
    fn a_retry_gets_the_original_outcome_and_a_reused_key_is_refused() {
        let mut l = Ledger::default();
        let treasury = open(&mut l, "t", Kind::System);
        let a = open(&mut l, "a", Kind::Character);
        let first = pay(&mut l, "k", treasury, a, 10).expect("first");
        assert_eq!(pay(&mut l, "k", treasury, a, 10), Ok(first));
        assert_eq!(l.balance(a), Ok(10));
        let Outcome::Transferred { operation } = first else {
            panic!("{first:?}")
        };
        assert_eq!(
            pay(&mut l, "k", treasury, a, 11),
            Err(Error::KeyReused(operation))
        );
        // After the window the key is forgotten and names a new operation.
        l.forget_before(1);
        assert_ne!(pay(&mut l, "k", treasury, a, 10), Ok(first));
        assert_eq!(l.balance(a), Ok(20));
    }

    #[test]
    fn stored_state_that_breaks_a_rule_is_refused() {
        let w = |id, kind, balance| Wallet {
            id: WalletId(id),
            kind,
            balance,
        };
        assert!(Ledger::load(vec![w(0, Kind::System, -5), w(1, Kind::Character, 5)], 0, vec![]).is_ok());
        assert!(matches!(
            Ledger::load(vec![w(0, Kind::System, -5), w(1, Kind::Character, 4)], 0, vec![]),
            Err(Error::Corrupt(_))
        ));
        assert!(matches!(
            Ledger::load(vec![w(0, Kind::System, 5), w(1, Kind::Character, -5)], 0, vec![]),
            Err(Error::Corrupt(_))
        ));
    }
}
