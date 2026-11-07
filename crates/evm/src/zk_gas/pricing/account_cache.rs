//! Progressive observed-transaction population proxy for the block-long State account cache.
//!
//! This empirical supplement does not enumerate preexecution/system or arbitrary prefilled DB
//! keys. Physical journal membership counts absent/untouched/reverted entries. It is deliberately
//! independent of transaction-local Account opcode budgets and logical resource/AL declarations.

use super::ZkGasModel;
use alloy_primitives::Address;
use std::collections::{BTreeMap, BTreeSet};

/// A missing or invalid physical receipt cannot be silently priced as an empty cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountCacheIncompleteness {
    /// Missing or invalid transaction-local physical population receipt.
    InvalidPopulation,
    /// Shadow collector was unavailable.
    CollectorUnavailable,
    /// Checked population, budget, product, merge or declared domain overflowed.
    ArithmeticOverflow,
}

/// Empirical progressive cache population and additive work, never actual guest query counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccountCacheDiagnostics {
    /// Distinct observed journal addresses in this block prefix.
    pub observed_accounts: u64,
    /// Largest first-four-network-byte group in that union.
    pub max_prefix_group: u64,
    /// Current rounded twenty-byte comparison proxy ceil((N+4M)/5).
    pub comparison_equivalents: u64,
    /// Sum of per-transaction 2*N_tx + persistent materialization upper budgets.
    pub query_budget: u64,
    /// Sum of progressive comparison proxy times the incoming transaction budget.
    pub work_units: u64,
    /// Number of physical population receipts included in this prefix.
    pub completed_transactions: u64,
    /// First sticky failure; counters remain partial diagnostics.
    pub incomplete: Option<AccountCacheIncompleteness>,
}
impl AccountCacheDiagnostics {
    /// Complete zero for a fresh block prefix with no observing transaction.
    pub const EMPTY: Self = Self {
        observed_accounts: 0,
        max_prefix_group: 0,
        comparison_equivalents: 0,
        query_budget: 0,
        work_units: 0,
        completed_transactions: 0,
        incomplete: None,
    };
}

/// Separate virtual membership for each committed/attempted prefix; never physical DB telemetry.
#[derive(Debug, Default)]
pub(super) struct AccountCacheLedger {
    accounts: BTreeSet<Address>,
    groups: BTreeMap<[u8; 4], u64>,
    diagnostics: AccountCacheDiagnostics,
}
impl AccountCacheLedger {
    /// Prevalidates all incoming arithmetic before changing the persistent virtual population.
    /// Each physical address budgets one possible basic lookup plus one possible commit entry;
    /// storage materializations budget their possible additional State::storage owner lookup.
    pub(super) fn merge(&mut self, keys: Option<&[Address]>, storage: u64, valid: bool) {
        if !valid || keys.is_none() {
            self.diagnostics
                .incomplete
                .get_or_insert(AccountCacheIncompleteness::InvalidPopulation);
            return;
        }
        if self.diagnostics.incomplete.is_some() {
            return;
        }
        let keys = keys.unwrap();
        let next = (|| {
            let mut added = BTreeSet::new();
            let mut delta = BTreeMap::<[u8; 4], u64>::new();
            for address in keys {
                if !self.accounts.contains(address) && added.insert(*address) {
                    let prefix = [address[0], address[1], address[2], address[3]];
                    let count = delta.entry(prefix).or_default();
                    *count = count.checked_add(1)?;
                }
            }
            let n =
                self.diagnostics.observed_accounts.checked_add(u64::try_from(added.len()).ok()?)?;
            let mut m = self.diagnostics.max_prefix_group;
            for (prefix, count) in &delta {
                m = m.max(self.groups.get(prefix).copied().unwrap_or(0).checked_add(*count)?);
            }
            let numerator = n.checked_add(m.checked_mul(4)?)?;
            let c = numerator / 5 + u64::from(numerator % 5 != 0);
            let budget = u64::try_from(keys.len()).ok()?.checked_mul(2)?.checked_add(storage)?;
            let work_units = self.diagnostics.work_units.checked_add(c.checked_mul(budget)?)?;
            ZkGasModel.price_account_cache_work(work_units).ok()?;
            Some((
                added,
                delta,
                AccountCacheDiagnostics {
                    observed_accounts: n,
                    max_prefix_group: m,
                    comparison_equivalents: c,
                    query_budget: self.diagnostics.query_budget.checked_add(budget)?,
                    work_units,
                    completed_transactions: self
                        .diagnostics
                        .completed_transactions
                        .checked_add(1)?,
                    incomplete: None,
                },
            ))
        })();
        match next {
            Some((added, delta, diagnostics)) => {
                self.accounts.extend(added);
                for (prefix, count) in delta {
                    *self.groups.entry(prefix).or_default() += count;
                }
                self.diagnostics = diagnostics;
            }
            None => {
                self.diagnostics.incomplete = Some(AccountCacheIncompleteness::ArithmeticOverflow);
            }
        }
    }
    /// Copies only public scalar diagnostics, never the virtual union.
    pub(super) const fn diagnostics(&self) -> AccountCacheDiagnostics {
        self.diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_prefix_dispersion_rounding_and_storage_budget() {
        let mut ledger = AccountCacheLedger::default();
        let a = Address::ZERO;
        let mut b = a;
        b.0[0] = 1;
        ledger.merge(Some(&[a, b]), 3, true);
        let d = ledger.diagnostics();
        assert_eq!((d.observed_accounts, d.max_prefix_group, d.comparison_equivalents), (2, 1, 2));
        assert_eq!((d.query_budget, d.work_units), (7, 14));
    }
    #[test]
    fn overflow_is_atomic_and_missing_population_sticky() {
        let mut ledger = AccountCacheLedger::default();
        ledger.merge(Some(&[Address::ZERO]), u64::MAX, true);
        assert_eq!(
            ledger.diagnostics.incomplete,
            Some(AccountCacheIncompleteness::ArithmeticOverflow)
        );
        assert!(ledger.accounts.is_empty());
        assert!(ledger.groups.is_empty());
        let mut missing = AccountCacheLedger::default();
        missing.merge(None, 0, true);
        missing.merge(Some(&[]), 0, true);
        assert_eq!(
            missing.diagnostics.incomplete,
            Some(AccountCacheIncompleteness::InvalidPopulation)
        );
    }

    #[test]
    fn rounded_disperse_population_and_independent_domain_are_checked_before_mutation() {
        let mut ledger = AccountCacheLedger::default();
        let keys: Vec<_> = (0..6)
            .map(|n| {
                let mut address = Address::ZERO;
                address.0[0] = n;
                address
            })
            .collect();
        ledger.merge(Some(&keys), 0, true);
        assert_eq!(ledger.diagnostics.comparison_equivalents, 2); // ceil((6+4)/5)
        assert_eq!(ledger.diagnostics.work_units, 24);
        let accounts = ledger.accounts.clone();
        let groups = ledger.groups.clone();
        let previous = ledger.diagnostics;
        ledger.diagnostics.work_units = super::super::generated::ACCOUNT_CACHE_WORK_BOUND;
        ledger.merge(Some(&[Address::repeat_byte(0xff)]), 0, true);
        assert_eq!(ledger.accounts, accounts);
        assert_eq!(ledger.groups, groups);
        assert_eq!(ledger.diagnostics.observed_accounts, previous.observed_accounts);
        assert_eq!(
            ledger.diagnostics.incomplete,
            Some(AccountCacheIncompleteness::ArithmeticOverflow)
        );
    }
}
