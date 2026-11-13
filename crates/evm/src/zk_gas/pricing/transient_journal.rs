//! Transaction-scoped transient-map work with distinct-ever-materialized key accounting.

use std::collections::BTreeSet;

use alloy_primitives::{Address, U256};

use super::{
    AccessClass,
    storage_journal::{
        StorageJournalDiagnostics, StorageJournalIncompleteness, StorageJournalLedger,
    },
};

/// Independent transient-map diagnostics; never persistent slots, MPT targets or hash collisions.
///
/// Materialization candidates are full `(owner, U256)` keys first encountered at an initially
/// physical slot or potential vacant nonzero TSTORE. Misses and absent zero stores do not enter
/// the seen set. Deletion, frame/root rollback and failed writes never remove already seen keys.
/// A/J still grow on every valid-operand attempt, so ordinary repeated clear/refill stays linear.
/// Partial diagnostics remain inspectable after checked-arithmetic failure but cannot price work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransientStorageJournalDiagnostics {
    /// Potential TLOAD attempts with valid operands and a known owner footprint, in events.
    pub potential_tload_count: u64,
    /// Potential TSTORE attempts, including noops and failed writes, in events.
    pub potential_tstore_count: u64,
    /// Sum of physical owner occupancy plus one per potential access, in occupancy units.
    pub direct_lookup_units: u64,
    /// Largest captured physical inner-map length before an event, in slots.
    pub max_occupancy_before: u64,
    /// Maximum observed occupancy plus one within any transaction/owner scope, in slots.
    pub high_water_upper: u64,
    /// One potential changed-entry reserve per TSTORE attempt; transient storage has no cold
    /// event.
    pub potential_rollback_entries: u64,
    /// Sum of owner occupancy high-water times potential changed entries; diagnostic occupancy
    /// units.
    pub rollback_lookup_units_upper: u64,
    /// Distinct full-U256 materialization candidates per transaction/owner, summed across scopes.
    pub materialization_count_upper: u64,
    /// Largest group of distinct materialized keys sharing raw low32 bits; not hash collisions.
    pub max_low32_group_upper: u64,
    /// Maximum owner C=ceil((N+7*M)/8), in conservative 32-byte-comparison equivalents.
    pub comparison_equivalents_high_water_upper: u64,
    /// Sum of owner C*(A+N+J), with A all attempts, distinct N and J potential TSTORE entries.
    pub comparison_work_units_upper: u64,
    /// First metadata/arithmetic failure; incomplete observations cannot yield a valid subtotal.
    pub incomplete: Option<StorageJournalIncompleteness>,
}

impl TransientStorageJournalDiagnostics {
    /// Empty complete transient summary for a scope with no observed work.
    pub(super) const EMPTY: Self = Self {
        potential_tload_count: 0,
        potential_tstore_count: 0,
        direct_lookup_units: 0,
        max_occupancy_before: 0,
        high_water_upper: 0,
        potential_rollback_entries: 0,
        rollback_lookup_units_upper: 0,
        materialization_count_upper: 0,
        max_low32_group_upper: 0,
        comparison_equivalents_high_water_upper: 0,
        comparison_work_units_upper: 0,
        incomplete: None,
    };

    /// Merges transaction scopes with checked sums, maximum group/envelope and sticky failure.
    pub(super) fn merge(&mut self, other: Self) {
        let mut common = self.into_common();
        common.merge(other.into_common());
        *self = Self::from_common(common);
    }

    /// Maps the shared arithmetic engine's internal counters into explicitly transient names.
    fn from_common(value: StorageJournalDiagnostics) -> Self {
        Self {
            potential_tload_count: value.potential_sload_count,
            potential_tstore_count: value.potential_sstore_count,
            direct_lookup_units: value.direct_lookup_units,
            max_occupancy_before: value.max_occupancy_before,
            high_water_upper: value.high_water_upper,
            potential_rollback_entries: value.potential_rollback_entries,
            rollback_lookup_units_upper: value.rollback_lookup_units_upper,
            materialization_count_upper: value.materialization_count_upper,
            max_low32_group_upper: value.max_low32_group_upper,
            comparison_equivalents_high_water_upper: value.comparison_equivalents_high_water_upper,
            comparison_work_units_upper: value.comparison_work_units_upper,
            incomplete: value.incomplete,
        }
    }

    /// Reuses checked arithmetic only; this conversion never emits persistent resource events.
    fn into_common(self) -> StorageJournalDiagnostics {
        StorageJournalDiagnostics {
            potential_sload_count: self.potential_tload_count,
            potential_sstore_count: self.potential_tstore_count,
            direct_lookup_units: self.direct_lookup_units,
            max_occupancy_before: self.max_occupancy_before,
            high_water_upper: self.high_water_upper,
            potential_rollback_entries: self.potential_rollback_entries,
            rollback_lookup_units_upper: self.rollback_lookup_units_upper,
            materialization_count_upper: self.materialization_count_upper,
            max_low32_group_upper: self.max_low32_group_upper,
            comparison_equivalents_high_water_upper: self.comparison_equivalents_high_water_upper,
            comparison_work_units_upper: self.comparison_work_units_upper,
            incomplete: self.incomplete,
        }
    }
}

/// One transaction's transient-only ledger; completely separate from persistent materializations.
#[derive(Debug, Default)]
pub(super) struct TransientStorageJournalLedger {
    /// Existing checked owner arithmetic, always observed with no cold-access entry.
    core: StorageJournalLedger,
    /// Full keys ever potentially materialized in this transaction; never removed on rollback.
    materialized_seen: BTreeSet<(Address, U256)>,
}

impl TransientStorageJournalLedger {
    /// Records a potential TLOAD/TSTORE attempt without a full-key seen lookup for reads.
    pub(super) fn observe(&mut self, owner: Address, occupancy: Option<usize>, store: bool) {
        self.core.observe(owner, occupancy, AccessClass::Warm, store);
    }

    /// Seeds an actual initial slot or records a vacant nonzero-store candidate once per full key.
    ///
    /// Callers must not call this for a read miss or absent zero store. No deletion or EVM
    /// checkpoint callback removes seen keys. A failed candidate may conservatively consume
    /// its distinct slot once; repeated failed candidates still accumulate attempt/rollback work.
    pub(super) fn observe_materialization(&mut self, owner: Address, key: U256) {
        if self.materialized_seen.insert((owner, key)) {
            self.core.observe_materialization(owner, key);
        }
    }

    /// Finalizes per-owner reserve before the transaction ledger is dropped and reset.
    pub(super) fn finish(&mut self) {
        self.core.finish();
    }

    /// Returns the currently retained explicitly transient observation, including partial values.
    pub(super) fn diagnostics(&self) -> TransientStorageJournalDiagnostics {
        TransientStorageJournalDiagnostics::from_common(self.core.diagnostics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};

    #[test]
    fn clear_refill_and_rollback_reinsertions_keep_same_key_linear() {
        for repeats in [1_u64, 64, 512] {
            let mut ledger = TransientStorageJournalLedger::default();
            for _ in 0..repeats {
                ledger.observe(Address::ZERO, Some(0), true);
                ledger.observe_materialization(Address::ZERO, U256::ZERO);
                ledger.observe(Address::ZERO, Some(1), true);
                // A clear or rollback removes the physical key, never the observer's seen set.
            }
            ledger.finish();
            let d = ledger.diagnostics();
            assert_eq!(d.materialization_count_upper, 1);
            assert_eq!(d.max_low32_group_upper, 1);
            assert_eq!(d.comparison_equivalents_high_water_upper, 1);
            assert_eq!(d.potential_tstore_count, 2 * repeats);
            assert_eq!(d.potential_rollback_entries, 2 * repeats);
            assert_eq!(d.comparison_work_units_upper, 4 * repeats + 1);
        }
    }

    #[test]
    fn read_misses_and_absent_zero_stores_cannot_consume_materialization_seen() {
        let mut ledger = TransientStorageJournalLedger::default();
        for _ in 0..512 {
            ledger.observe(Address::ZERO, Some(0), false);
        }
        ledger.observe(Address::ZERO, Some(0), true); // Absent-key TSTORE0, not materialization.
        for n in 1..=512_u64 {
            ledger.observe(Address::ZERO, Some((n - 1) as usize), true);
            ledger.observe_materialization(Address::ZERO, U256::from(n) << 32);
        }
        ledger.finish();
        let d = ledger.diagnostics();
        assert_eq!(d.materialization_count_upper, 512);
        assert_eq!(d.max_low32_group_upper, 512);
        assert_eq!(d.comparison_equivalents_high_water_upper, 512);
        assert_eq!(d.potential_tload_count, 512);
        assert_eq!(d.potential_tstore_count, 513);
        assert_eq!(d.comparison_work_units_upper, 512 * (1025 + 512 + 513));
    }

    #[test]
    fn full_u256_distinctness_is_not_low32_deduplication() {
        for concentrated in [false, true] {
            let mut ledger = TransientStorageJournalLedger::default();
            ledger.observe(Address::ZERO, Some(0), false); // Known initial query miss.
            for n in 0..512_u64 {
                let key = if concentrated { U256::from(n) << 32 } else { U256::from(n) };
                ledger.observe(Address::ZERO, Some(n as usize), true);
                ledger.observe_materialization(Address::ZERO, key);
            }
            for _ in 0..=32768 {
                ledger.observe(Address::ZERO, Some(512), false);
            }
            ledger.finish();
            let d = ledger.diagnostics();
            assert_eq!(d.materialization_count_upper, 512);
            assert_eq!(d.max_low32_group_upper, if concentrated { 512 } else { 1 });
            assert_eq!(
                d.comparison_equivalents_high_water_upper,
                if concentrated { 512 } else { 65 }
            );
            assert_eq!(
                d.comparison_work_units_upper,
                if concentrated { 17_564_672 } else { 2_229_890 }
            );
            assert_eq!(d.incomplete, None);
        }
    }

    #[test]
    fn owner_and_transaction_boundaries_never_share_seen_or_multiply_work() {
        let mut combined = TransientStorageJournalDiagnostics::default();
        for _ in 0..2 {
            let mut ledger = TransientStorageJournalLedger::default();
            for owner in [Address::ZERO, Address::repeat_byte(1)] {
                ledger.observe(owner, Some(0), true);
                ledger.observe_materialization(owner, U256::ZERO);
                ledger.observe_materialization(owner, U256::ZERO); // Duplicate candidate.
            }
            ledger.finish();
            assert_eq!(ledger.diagnostics().comparison_work_units_upper, 6);
            combined.merge(ledger.diagnostics());
        }
        assert_eq!(combined.materialization_count_upper, 4);
        assert_eq!(combined.max_low32_group_upper, 1);
        assert_eq!(combined.comparison_work_units_upper, 12);
    }

    #[test]
    fn failed_vacant_attempts_and_preexisting_physical_keys_are_conservative() {
        let mut ledger = TransientStorageJournalLedger::default();
        for _ in 0..2 {
            ledger.observe(Address::ZERO, Some(0), true);
            ledger.observe_materialization(Address::ZERO, U256::from(7));
        }
        ledger.finish();
        assert_eq!(ledger.diagnostics().materialization_count_upper, 1);
        assert_eq!(ledger.diagnostics().comparison_work_units_upper, 5);
        let mut seeded = TransientStorageJournalLedger::default();
        seeded.observe_materialization(Address::ZERO, U256::from(1));
        seeded.observe_materialization(Address::ZERO, U256::from(2));
        seeded.observe(Address::ZERO, Some(2), false);
        seeded.finish();
        assert_eq!(seeded.diagnostics().materialization_count_upper, 2);
        assert_eq!(seeded.diagnostics().comparison_work_units_upper, 6);
    }

    #[test]
    fn incomplete_and_checked_merge_overflow_never_publish_partial_zero_as_complete() {
        let mut ledger = TransientStorageJournalLedger::default();
        ledger.observe(Address::ZERO, None, true);
        ledger.observe(Address::ZERO, Some(0), true);
        ledger.observe_materialization(Address::ZERO, U256::ZERO);
        ledger.finish();
        assert_eq!(ledger.diagnostics().comparison_work_units_upper, 3);
        assert_eq!(
            ledger.diagnostics().incomplete,
            Some(StorageJournalIncompleteness::MissingFootprint)
        );
        let mut left = TransientStorageJournalDiagnostics {
            materialization_count_upper: u64::MAX,
            ..Default::default()
        };
        left.merge(ledger.diagnostics());
        assert_eq!(left.materialization_count_upper, u64::MAX);
        assert_eq!(left.incomplete, Some(StorageJournalIncompleteness::MissingFootprint));
        let mut arithmetic = TransientStorageJournalDiagnostics {
            comparison_work_units_upper: u64::MAX,
            ..Default::default()
        };
        arithmetic.merge(TransientStorageJournalDiagnostics {
            comparison_work_units_upper: 1,
            ..Default::default()
        });
        assert_eq!(arithmetic.comparison_work_units_upper, u64::MAX);
        assert_eq!(arithmetic.incomplete, Some(StorageJournalIncompleteness::ArithmeticOverflow));
    }
}
