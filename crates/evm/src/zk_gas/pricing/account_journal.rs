//! Transaction-local physical Account density and empirical potential-work accounting.

use super::ZkGasModel;
use alloy_primitives::Address;
use std::collections::BTreeMap;

/// Why a partial Account reserve cannot produce a trustworthy subtotal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountJournalIncompleteness {
    /// No physical finalized state was supplied for an observing transaction.
    MissingPopulation,
    /// An observing report had no collector available.
    CollectorUnavailable,
    /// More than one physical receipt was supplied for the same transaction.
    DuplicatePopulation,
    /// Opcode work arrived after its population receipt had been sealed.
    ObservationAfterPopulation,
    /// A counter, product, merge or independently declared domain overflowed.
    ArithmeticOverflow,
}

/// Additive empirical Account work; none of these values are guest hash-probe counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccountJournalDiagnostics {
    /// Assigned potential forward/unwind opcode budget, summed across transaction scopes.
    pub potential_query_budget: u64,
    /// Actual physical Account entries, summed per transaction rather than block-deduplicated.
    pub physical_account_count: u64,
    /// Largest first-four-network-byte population group in any finalized transaction.
    pub max_prefix_group: u64,
    /// Largest rounded twenty-byte comparison proxy C in any finalized transaction.
    pub comparison_equivalents: u64,
    /// Sum of finalized C*(Q+N), in independently priced empirical work units.
    pub work_units: u64,
    /// Number of successfully supplied population receipts.
    pub completed_transactions: u64,
    /// First sticky failure; remaining fields are only partial diagnostic values.
    pub incomplete: Option<AccountJournalIncompleteness>,
}

impl AccountJournalDiagnostics {
    /// Complete zero for a prefix that contains no observing transaction scopes.
    pub const EMPTY: Self = Self {
        potential_query_budget: 0,
        physical_account_count: 0,
        max_prefix_group: 0,
        comparison_equivalents: 0,
        work_units: 0,
        completed_transactions: 0,
        incomplete: None,
    };

    /// Atomically adds per-TX sums and high-water diagnostics, retaining the first failure.
    pub(super) fn merge(&mut self, other: Self) {
        self.incomplete = self.incomplete.or(other.incomplete);
        let next = (|| {
            Some(Self {
                potential_query_budget: self
                    .potential_query_budget
                    .checked_add(other.potential_query_budget)?,
                physical_account_count: self
                    .physical_account_count
                    .checked_add(other.physical_account_count)?,
                max_prefix_group: self.max_prefix_group.max(other.max_prefix_group),
                comparison_equivalents: self
                    .comparison_equivalents
                    .max(other.comparison_equivalents),
                work_units: self.work_units.checked_add(other.work_units)?,
                completed_transactions: self
                    .completed_transactions
                    .checked_add(other.completed_transactions)?,
                incomplete: self.incomplete,
            })
        })();
        if let Some(next) =
            next.filter(|d| ZkGasModel.price_account_journal_work(d.work_units).is_ok())
        {
            *self = next;
        } else {
            self.incomplete.get_or_insert(AccountJournalIncompleteness::ArithmeticOverflow);
        }
    }
}

/// One scalar budget plus exactly one final-state population receipt; no per-step map reads.
#[derive(Debug, Default)]
pub(super) struct AccountJournalLedger {
    /// Retained partial or finalized counters for the current observing transaction.
    diagnostics: AccountJournalDiagnostics,
    /// Whether a population receipt has already been attempted, including failed captures.
    population_supplied: bool,
}

impl AccountJournalLedger {
    /// Counts an opcode attempt conservatively, including early failures and rejected steps.
    pub(super) fn observe_opcode(&mut self, opcode: u8) {
        if self.population_supplied {
            self.diagnostics
                .incomplete
                .get_or_insert(AccountJournalIncompleteness::ObservationAfterPopulation);
            return;
        }
        let budget = super::generated::ACCOUNT_JOURNAL_OPCODE_BUDGETS[usize::from(opcode)];
        match self.diagnostics.potential_query_budget.checked_add(budget) {
            Some(next) => self.diagnostics.potential_query_budget = next,
            None => {
                self.diagnostics
                    .incomplete
                    .get_or_insert(AccountJournalIncompleteness::ArithmeticOverflow);
            }
        }
    }

    /// Consumes distinct keys from the actual finalized EvmState, never AL/resource declarations.
    ///
    /// The runtime supplies its map-key iterator exactly once, even after handler failure.
    /// Scratch prefixes are dropped here. This trusts the standard journal append-only key
    /// lifetime; custom inspectors that delete physical keys are outside that high-water claim.
    pub(super) fn complete(&mut self, addresses: impl IntoIterator<Item = Address>) {
        if self.population_supplied {
            self.diagnostics
                .incomplete
                .get_or_insert(AccountJournalIncompleteness::DuplicatePopulation);
            return;
        }
        self.population_supplied = true;
        let result = (|| {
            let mut groups = BTreeMap::<[u8; 4], u64>::new();
            let mut n = 0_u64;
            let mut m = 0_u64;
            for address in addresses {
                n = n.checked_add(1)?;
                let prefix = [address[0], address[1], address[2], address[3]];
                let count = groups.entry(prefix).or_default();
                *count = count.checked_add(1)?;
                m = m.max(*count);
            }
            let numerator = n.checked_add(m.checked_mul(4)?)?;
            let c = numerator / 5 + u64::from(numerator % 5 != 0);
            let units = c.checked_mul(self.diagnostics.potential_query_budget.checked_add(n)?)?;
            ZkGasModel.price_account_journal_work(units).ok()?;
            Some((n, m, c, units))
        })();
        match result {
            Some((n, m, c, units)) => {
                self.diagnostics.physical_account_count = n;
                self.diagnostics.max_prefix_group = m;
                self.diagnostics.comparison_equivalents = c;
                self.diagnostics.work_units = units;
                self.diagnostics.completed_transactions = 1;
            }
            None => {
                self.diagnostics
                    .incomplete
                    .get_or_insert(AccountJournalIncompleteness::ArithmeticOverflow);
            }
        }
    }

    /// Reads the finalized or partial diagnostics without changing their capture state.
    pub(super) const fn diagnostics(&self) -> AccountJournalDiagnostics {
        self.diagnostics
    }

    /// Returns a finalized receipt; absence is sticky rather than an implicit empty population.
    pub(super) fn finish(&mut self) -> AccountJournalDiagnostics {
        if !self.population_supplied {
            self.diagnostics
                .incomplete
                .get_or_insert(AccountJournalIncompleteness::MissingPopulation);
        }
        self.diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    #[test]
    fn account_journal_budget_and_physical_population() {
        let mut ledger = AccountJournalLedger::default();
        for opcode in [0x31, 0x3b, 0x3c, 0x3f, 0x47] {
            ledger.observe_opcode(opcode);
        }
        for opcode in [0x54, 0x55] {
            ledger.observe_opcode(opcode);
        }
        for opcode in [0xf0, 0xf1, 0xf2, 0xf4, 0xf5, 0xfa, 0xff] {
            ledger.observe_opcode(opcode);
        }
        for opcode in [0x01, 0x5c, 0x5d, 0xfd] {
            ledger.observe_opcode(opcode);
        }
        ledger.complete([Address::ZERO, Address::with_last_byte(1)]);
        let d = ledger.finish();
        assert_eq!(d.potential_query_budget, 242);
        assert_eq!(
            (d.physical_account_count, d.max_prefix_group, d.comparison_equivalents),
            (2, 2, 2)
        );
        assert_eq!(d.work_units, 488);
        assert_eq!(d.completed_transactions, 1);
        assert_eq!(d.incomplete, None);
    }

    #[test]
    fn account_journal_requires_exactly_one_physical_receipt() {
        let mut missing = AccountJournalLedger::default();
        assert_eq!(
            missing.finish().incomplete,
            Some(AccountJournalIncompleteness::MissingPopulation)
        );
        let mut duplicate = AccountJournalLedger::default();
        duplicate.complete([]);
        duplicate.complete([]);
        assert_eq!(
            duplicate.finish().incomplete,
            Some(AccountJournalIncompleteness::DuplicatePopulation)
        );
    }

    #[test]
    fn account_journal_known_empty_and_network_prefix_rounding() {
        let mut empty = AccountJournalLedger::default();
        empty.observe_opcode(0x31);
        empty.complete([]);
        assert_eq!(empty.finish().work_units, 0);
        let mut ledger = AccountJournalLedger::default();
        ledger.observe_opcode(0x47);
        ledger.complete([Address::ZERO, Address::repeat_byte(1), Address::repeat_byte(2)]);
        let d = ledger.finish();
        assert_eq!(
            (d.physical_account_count, d.max_prefix_group, d.comparison_equivalents, d.work_units),
            (3, 1, 2, 10)
        );
    }
    #[test]
    fn account_journal_checked_arithmetic_and_domain_are_sticky() {
        let mut overflow = AccountJournalLedger::default();
        overflow.diagnostics.potential_query_budget = u64::MAX;
        overflow.observe_opcode(0x31);
        assert_eq!(overflow.diagnostics.potential_query_budget, u64::MAX);
        overflow.complete([]);
        assert_eq!(
            overflow.finish().incomplete,
            Some(AccountJournalIncompleteness::ArithmeticOverflow)
        );
        let mut domain = AccountJournalLedger::default();
        domain.diagnostics.potential_query_budget = 1_000_000_000_000;
        domain.complete([Address::ZERO]);
        assert_eq!(
            domain.finish().incomplete,
            Some(AccountJournalIncompleteness::ArithmeticOverflow)
        );
        assert_eq!(domain.diagnostics.work_units, 0);
        let mut late = AccountJournalLedger::default();
        late.complete([]);
        late.observe_opcode(0x31);
        assert_eq!(
            late.finish().incomplete,
            Some(AccountJournalIncompleteness::ObservationAfterPopulation)
        );
    }

    #[test]
    fn account_journal_transaction_fold_does_not_multiply_block_sums() {
        use super::super::{TransactionMetadata, ZkGasAccumulator};
        let mut accumulator = ZkGasAccumulator::default();
        for committed in [true, false] {
            accumulator.begin_transaction(TransactionMetadata::default()).unwrap();
            accumulator.observe_account_journal_opcode(0x31);
            accumulator.complete_account_journal_population([Address::ZERO]).unwrap();
            accumulator.complete_storage_transition_population([]).unwrap();
            if committed {
                accumulator.commit_transaction().unwrap();
            } else {
                accumulator.reset_transaction();
            }
        }
        assert_eq!(accumulator.committed_snapshot().account_journal_work_units().unwrap(), 3);
        assert_eq!(accumulator.attempted_snapshot().block.account_journal_work_units().unwrap(), 6);
        assert_eq!(
            accumulator.attempted_snapshot().block.account_journal.completed_transactions,
            2
        );
        let mut missing = ZkGasAccumulator::default();
        missing.begin_transaction(TransactionMetadata::default()).unwrap();
        missing.commit_transaction().unwrap();
        assert!(missing.committed_snapshot().account_journal_work_units().is_err());
        let mut duplicate = ZkGasAccumulator::default();
        duplicate.begin_transaction(TransactionMetadata::default()).unwrap();
        duplicate.complete_account_journal_population([]).unwrap();
        duplicate.complete_storage_transition_population([]).unwrap();
        assert!(duplicate.complete_account_journal_population([]).is_err());
        duplicate.commit_transaction().unwrap();
        assert!(duplicate.committed_snapshot().account_journal_work_units().is_err());
    }

    #[test]
    fn account_journal_histogram_is_network_order_and_merge_is_atomic() {
        let mut keys = Vec::new();
        for i in 0..=255 {
            let mut bytes = [0u8; 20];
            bytes[0] = i;
            keys.push(Address::from(bytes));
        }
        let mut ledger = AccountJournalLedger::default();
        ledger.complete(keys);
        let d = ledger.finish();
        assert_eq!(
            (d.physical_account_count, d.max_prefix_group, d.comparison_equivalents),
            (256, 1, 52)
        );
        let mut prefix =
            AccountJournalDiagnostics { work_units: 1_000_000_000_000, ..Default::default() };
        prefix.merge(AccountJournalDiagnostics { work_units: 1, ..Default::default() });
        assert_eq!(prefix.work_units, 1_000_000_000_000);
        assert_eq!(prefix.incomplete, Some(AccountJournalIncompleteness::ArithmeticOverflow));
        assert!(ZkGasModel.price_account_journal_work(1_000_000_000_001).is_err());
        assert_eq!(
            ZkGasModel.price_account_journal_work(1_000_000_000_000).unwrap(),
            128 * super::super::FIXED_POINT_SCALE * 1_000_000_000_000
        );
    }
    #[test]
    fn account_journal_malformed_snapshot_statistics_cannot_seal_zero() {
        use super::super::ZkGasBlockSnapshot;
        for d in [
            AccountJournalDiagnostics {
                physical_account_count: 1,
                completed_transactions: 1,
                ..Default::default()
            },
            AccountJournalDiagnostics { potential_query_budget: 1, ..Default::default() },
            AccountJournalDiagnostics {
                physical_account_count: 1,
                max_prefix_group: 2,
                comparison_equivalents: 2,
                work_units: 2,
                completed_transactions: 1,
                ..Default::default()
            },
        ] {
            assert!(
                ZkGasBlockSnapshot { account_journal: d, ..Default::default() }
                    .account_journal_work_units()
                    .is_err()
            );
        }
    }
}
