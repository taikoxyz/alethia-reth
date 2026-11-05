//! Ordinary-storage journal observations and an independently priced comparison envelope.

use std::collections::BTreeMap;

use alloy_primitives::{Address, U256};

use super::AccessClass;

/// Why potential storage-work diagnostics cannot be treated as complete.
///
/// These failures prevent a valid known subtotal, but never change EVM execution or legacy gas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageJournalIncompleteness {
    /// Required storage identity was unavailable; proven pre-lookup stack underflow is excluded.
    MissingStorageInput,
    /// The already-loaded journal account was unavailable at capture time.
    MissingFootprint,
    /// A conversion, count, unit sum, or rollback product exceeded `u64`.
    ArithmeticOverflow,
    /// No observing collector was available for this report.
    CollectorUnavailable,
}

/// Seed-independent observation and conservative comparison envelope for ordinary storage work.
///
/// Each captured potential lookup contributes `n_before + 1` units, where `n_before` is the
/// physical transaction-local storage-map length for the storage owner. These are neither
/// guest hash probes nor measured PGU. Static/stipend/OOG failures can be counted before any
/// actual journal lookup; missing account/footprint metadata marks this summary incomplete.
/// Proven stack operand underflow occurs before a lookup and contributes known zero journal work.
/// Frame and outer reverts do not remove computational work. Access-list declarations,
/// resource finalization walks, and transient storage do not emit lookup events. Initial
/// physically cached slots separately seed the comparison envelope once per transaction.
///
/// All sums and products are checked. On failure, prior partial values remain inspectable and
/// `incomplete` is retained; callers must not treat those partial values as a complete bound.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StorageJournalDiagnostics {
    /// Potential SLOAD captures with a valid owner and footprint, in lookup events.
    pub potential_sload_count: u64,
    /// Potential SSTORE captures with a valid owner and footprint, in lookup events.
    pub potential_sstore_count: u64,
    /// Sum of `n_before + 1` across captured potential lookup events, in occupancy units.
    pub direct_lookup_units: u64,
    /// Largest observed physical map length before an event, in slots.
    pub max_occupancy_before: u64,
    /// Maximum `n_before + 1` across transaction/owner scopes, in slots.
    pub high_water_upper: u64,
    /// Sum of cold potential accesses plus all potential SSTORE changes, in journal entries.
    ///
    /// Every SSTORE contributes one potential changed entry, including noops and failed writes.
    /// A cold SSTORE therefore contributes two. Cold and occupied/vacant are independent axes:
    /// an access-list-warm vacant slot contributes none; a reverted cold occupied slot adds one.
    pub potential_rollback_entries: u64,
    /// Sum of owner high-water upper times owner potential entries, in occupancy units.
    ///
    /// Products are folded per transaction/owner at scope finalization, then added across
    /// scopes. This reserves potential storage-warmed/changed rollback lookups, including
    /// entries made early in a frame that might unwind after the map has grown. It does not
    /// assert that a revert occurred, count actual reverted entries, or bound total guest PGU.
    pub rollback_lookup_units_upper: u64,
    /// Initial cached slots plus potential uncached materialization attempts, in events.
    ///
    /// This is an upper count, not a distinct-key count: repeated failed vacant attempts and
    /// clear/refill may count a key again. It is additive across transaction/owner scopes.
    pub materialization_count_upper: u64,
    /// Largest owner group of materialization events sharing raw-key low 32 bits, in events.
    ///
    /// The group matches the first four bytes of the profiled little-endian U256 comparator.
    /// It is not a count of actual guest hash-bucket or tag collisions.
    pub max_low32_group_upper: u64,
    /// Maximum owner comparison envelope `ceil((N + 7*M)/8)`, in 32-byte equivalents.
    ///
    /// `N` counts materialization events and `M` is that owner's largest low32 group. This
    /// bounds compared bytes for one candidate scan, not complete hash-table work or PGU.
    pub comparison_equivalents_high_water_upper: u64,
    /// Sum of owner `C * (A + N + J)`, in potential 32-byte-comparison work units.
    ///
    /// `A` counts captured SLOAD/SSTORE attempts, `N` materialization events and `J` the
    /// potential cold-plus-SSTORE rollback entries. The sealed model prices this conservative
    /// envelope independently; it is not a measured probe count or a universal PGU upper bound.
    pub comparison_work_units_upper: u64,
    /// First reason these partial observations cannot support a valid comparison-work quote.
    pub incomplete: Option<StorageJournalIncompleteness>,
}

impl StorageJournalDiagnostics {
    /// Empty complete summary for a scope with no captured ordinary storage work.
    pub(super) const EMPTY: Self = Self {
        potential_sload_count: 0,
        potential_sstore_count: 0,
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

    /// Adds one finalized transaction observation with checked counters and sticky incompleteness.
    pub(super) fn merge(&mut self, other: Self) {
        self.incomplete = self.incomplete.or(other.incomplete);
        let next = (|| {
            Some(Self {
                potential_sload_count: self
                    .potential_sload_count
                    .checked_add(other.potential_sload_count)?,
                potential_sstore_count: self
                    .potential_sstore_count
                    .checked_add(other.potential_sstore_count)?,
                direct_lookup_units: self
                    .direct_lookup_units
                    .checked_add(other.direct_lookup_units)?,
                max_occupancy_before: self.max_occupancy_before.max(other.max_occupancy_before),
                high_water_upper: self.high_water_upper.max(other.high_water_upper),
                potential_rollback_entries: self
                    .potential_rollback_entries
                    .checked_add(other.potential_rollback_entries)?,
                rollback_lookup_units_upper: self
                    .rollback_lookup_units_upper
                    .checked_add(other.rollback_lookup_units_upper)?,
                materialization_count_upper: self
                    .materialization_count_upper
                    .checked_add(other.materialization_count_upper)?,
                max_low32_group_upper: self.max_low32_group_upper.max(other.max_low32_group_upper),
                comparison_equivalents_high_water_upper: self
                    .comparison_equivalents_high_water_upper
                    .max(other.comparison_equivalents_high_water_upper),
                comparison_work_units_upper: self
                    .comparison_work_units_upper
                    .checked_add(other.comparison_work_units_upper)?,
                incomplete: self.incomplete,
            })
        })();
        match next {
            Some(next) => *self = next,
            None => self.mark_incomplete(StorageJournalIncompleteness::ArithmeticOverflow),
        }
    }

    /// Retains the first diagnostic failure without changing any execution or fee result.
    pub(super) fn mark_incomplete(&mut self, reason: StorageJournalIncompleteness) {
        self.incomplete.get_or_insert(reason);
    }
}

/// Transaction-local owner state; never shared with global resource-set deduplication.
#[derive(Debug, Default)]
struct OwnerWork {
    /// Conservative maximum physical occupancy after a potential lookup, in slots.
    high_water_upper: u64,
    /// Cold accesses plus all SSTORE attempts, in potential rollback entries.
    potential_entries: u64,
    /// Captured potential SLOAD/SSTORE lookups for this owner, in events.
    potential_lookups: u64,
    /// Initial slots and potential vacant materializations, never removed on clear or revert.
    materializations: u64,
    /// Ordered raw low32 groups; updated only for materialization events, not warm repeats.
    low32_groups: BTreeMap<u32, u64>,
    /// Largest low32 group reached during this owner's transaction scope, in events.
    max_group: u64,
    /// Current monotone compared-byte envelope, in 32-byte comparison equivalents.
    comparison_equivalents: u64,
}

/// One transaction's compact ledger, allocated only through the observing pricing path.
#[derive(Debug, Default)]
pub(super) struct StorageJournalLedger {
    /// Additive direct-work diagnostics and finalized rollback reserve.
    pub(super) diagnostics: StorageJournalDiagnostics,
    /// Owners with initial cached slots or captured storage work, reset at transaction end.
    owners: BTreeMap<Address, OwnerWork>,
}

impl StorageJournalLedger {
    /// Records a potential ordinary lookup without any database or storage-key lookup.
    pub(super) fn observe(
        &mut self,
        owner: Address,
        occupancy: Option<usize>,
        access: AccessClass,
        store: bool,
    ) {
        let Some(occupancy) = occupancy else {
            self.diagnostics.mark_incomplete(StorageJournalIncompleteness::MissingFootprint);
            return;
        };
        let next = (|| {
            let occupancy = u64::try_from(occupancy).ok()?;
            let units = occupancy.checked_add(1)?;
            let entries = u64::from(access == AccessClass::Cold).checked_add(u64::from(store))?;
            let previous = self.owners.get(&owner);
            let high_water_upper = previous.map_or(0, |work| work.high_water_upper).max(units);
            let potential_entries =
                previous.map_or(0, |work| work.potential_entries).checked_add(entries)?;
            let potential_lookups =
                previous.map_or(0, |work| work.potential_lookups).checked_add(1)?;
            let mut diagnostics = self.diagnostics;
            diagnostics.potential_sload_count =
                diagnostics.potential_sload_count.checked_add(u64::from(!store))?;
            diagnostics.potential_sstore_count =
                diagnostics.potential_sstore_count.checked_add(u64::from(store))?;
            diagnostics.direct_lookup_units = diagnostics.direct_lookup_units.checked_add(units)?;
            diagnostics.potential_rollback_entries =
                diagnostics.potential_rollback_entries.checked_add(entries)?;
            diagnostics.max_occupancy_before = diagnostics.max_occupancy_before.max(occupancy);
            diagnostics.high_water_upper = diagnostics.high_water_upper.max(units);
            Some((high_water_upper, potential_entries, potential_lookups, diagnostics))
        })();
        match next {
            Some((high_water_upper, potential_entries, potential_lookups, diagnostics)) => {
                let work = self.owners.entry(owner).or_default();
                work.high_water_upper = high_water_upper;
                work.potential_entries = potential_entries;
                work.potential_lookups = potential_lookups;
                self.diagnostics = diagnostics;
            }
            None => {
                self.diagnostics.mark_incomplete(StorageJournalIncompleteness::ArithmeticOverflow)
            }
        }
    }

    /// Records one already-cached initial slot or potential uncached materialization.
    ///
    /// The runtime reuses cached presence already captured for pricing. No full-key set or
    /// additional storage lookup is performed. Duplicate failed attempts and clear/refill
    /// deliberately overcount rather than lose earlier comparison work.
    pub(super) fn observe_materialization(&mut self, owner: Address, key: U256) {
        let low32 = key.as_limbs()[0] as u32;
        let next = (|| {
            let previous = self.owners.get(&owner);
            let count = previous
                .and_then(|work| work.low32_groups.get(&low32))
                .copied()
                .unwrap_or(0)
                .checked_add(1)?;
            let materializations =
                previous.map_or(0, |work| work.materializations).checked_add(1)?;
            let max_group = previous.map_or(0, |work| work.max_group).max(count);
            let numerator = materializations.checked_add(max_group.checked_mul(7)?)?;
            let comparison_equivalents = numerator / 8 + u64::from(numerator % 8 != 0);
            let mut diagnostics = self.diagnostics;
            diagnostics.materialization_count_upper =
                diagnostics.materialization_count_upper.checked_add(1)?;
            diagnostics.max_low32_group_upper = diagnostics.max_low32_group_upper.max(max_group);
            diagnostics.comparison_equivalents_high_water_upper =
                diagnostics.comparison_equivalents_high_water_upper.max(comparison_equivalents);
            Some((count, materializations, max_group, comparison_equivalents, diagnostics))
        })();
        match next {
            Some((count, materializations, max_group, comparison_equivalents, diagnostics)) => {
                let work = self.owners.entry(owner).or_default();
                work.low32_groups.insert(low32, count);
                work.materializations = materializations;
                work.max_group = max_group;
                work.comparison_equivalents = comparison_equivalents;
                self.diagnostics = diagnostics;
            }
            None => {
                self.diagnostics.mark_incomplete(StorageJournalIncompleteness::ArithmeticOverflow)
            }
        }
    }

    /// Folds each owner's final conservative high-water against all its potential entries.
    pub(super) fn finish(&mut self) {
        let reserve = self.owners.values().try_fold(0_u64, |sum, owner| {
            sum.checked_add(owner.high_water_upper.checked_mul(owner.potential_entries)?)
        });
        match reserve {
            Some(units) => self.diagnostics.rollback_lookup_units_upper = units,
            None => {
                self.diagnostics.mark_incomplete(StorageJournalIncompleteness::ArithmeticOverflow)
            }
        }
        let comparison_work = self.owners.values().try_fold(0_u64, |sum, owner| {
            let events = owner
                .potential_lookups
                .checked_add(owner.materializations)?
                .checked_add(owner.potential_entries)?;
            sum.checked_add(owner.comparison_equivalents.checked_mul(events)?)
        });
        match comparison_work {
            Some(units) => self.diagnostics.comparison_work_units_upper = units,
            None => {
                self.diagnostics.mark_incomplete(StorageJournalIncompleteness::ArithmeticOverflow)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_reserve_uses_later_high_water_without_cross_owner_multiplication() {
        let mut ledger = StorageJournalLedger::default();
        let owner = Address::repeat_byte(1);
        let other = Address::repeat_byte(2);
        // An access-list-warm vacant lookup can grow the map without a cold-entry reserve.
        ledger.observe(owner, Some(0), AccessClass::Warm, false);
        // Occupied but cold after child rollback: occupancy is retained and entry reserve grows.
        ledger.observe(owner, Some(1), AccessClass::Cold, false);
        ledger.observe(owner, Some(2), AccessClass::Warm, false);
        ledger.observe(other, Some(0), AccessClass::Cold, false);
        ledger.finish();
        assert_eq!(ledger.diagnostics.direct_lookup_units, 7);
        assert_eq!(ledger.diagnostics.potential_rollback_entries, 2);
        assert_eq!(ledger.diagnostics.rollback_lookup_units_upper, 3 + 1);
        assert_eq!(ledger.diagnostics.high_water_upper, 3);
    }

    #[test]
    fn every_sstore_reserves_a_potential_change_even_when_noop_or_failed() {
        let mut ledger = StorageJournalLedger::default();
        let owner = Address::ZERO;
        ledger.observe(owner, Some(0), AccessClass::Cold, true);
        ledger.observe(owner, Some(1), AccessClass::Warm, true);
        ledger.observe(owner, Some(1), AccessClass::Warm, false);
        ledger.finish();
        assert_eq!(ledger.diagnostics.potential_sload_count, 1);
        assert_eq!(ledger.diagnostics.potential_sstore_count, 2);
        assert_eq!(ledger.diagnostics.potential_rollback_entries, 3);
        assert_eq!(ledger.diagnostics.rollback_lookup_units_upper, 6);
    }

    #[test]
    fn missing_metadata_and_overflow_are_explicit_and_allow_later_observations() {
        let mut ledger = StorageJournalLedger::default();
        let owner = Address::ZERO;
        ledger.observe(owner, None, AccessClass::Cold, false);
        ledger.observe(owner, Some(0), AccessClass::Cold, false);
        assert_eq!(
            ledger.diagnostics.incomplete,
            Some(StorageJournalIncompleteness::MissingFootprint)
        );
        assert_eq!(ledger.diagnostics.potential_sload_count, 1);
        let mut overflow = StorageJournalLedger::default();
        overflow.diagnostics.direct_lookup_units = u64::MAX;
        overflow.observe(owner, Some(0), AccessClass::Cold, false);
        assert_eq!(
            overflow.diagnostics.incomplete,
            Some(StorageJournalIncompleteness::ArithmeticOverflow)
        );
        assert_eq!(overflow.diagnostics.potential_sload_count, 0);
        assert!(overflow.owners.is_empty());
        overflow.diagnostics.direct_lookup_units = 0;
        overflow.observe(owner, Some(0), AccessClass::Cold, false);
        overflow.finish();
        assert_eq!(overflow.diagnostics.potential_sload_count, 1);
        assert_eq!(overflow.diagnostics.rollback_lookup_units_upper, 1);
        assert!(overflow.diagnostics.incomplete.is_some());
    }

    #[test]
    fn checked_owner_entry_sum_and_rollback_product_cannot_silently_saturate() {
        let owner = Address::ZERO;
        let mut ledger = StorageJournalLedger::default();
        ledger.owners.insert(
            owner,
            OwnerWork { high_water_upper: 1, potential_entries: u64::MAX, ..Default::default() },
        );
        ledger.observe(owner, Some(0), AccessClass::Cold, false);
        assert_eq!(
            ledger.diagnostics.incomplete,
            Some(StorageJournalIncompleteness::ArithmeticOverflow)
        );
        let mut ledger = StorageJournalLedger::default();
        ledger.owners.insert(
            owner,
            OwnerWork { high_water_upper: u64::MAX, potential_entries: 2, ..Default::default() },
        );
        ledger.finish();
        assert_eq!(
            ledger.diagnostics.incomplete,
            Some(StorageJournalIncompleteness::ArithmeticOverflow)
        );
    }

    #[test]
    fn prefix_overflow_retains_partial_values_and_incompleteness() {
        let mut summary =
            StorageJournalDiagnostics { potential_sload_count: u64::MAX, ..Default::default() };
        summary.merge(StorageJournalDiagnostics {
            potential_sload_count: 1,
            direct_lookup_units: 1,
            ..Default::default()
        });
        assert_eq!(summary.potential_sload_count, u64::MAX);
        assert_eq!(summary.direct_lookup_units, 0);
        assert_eq!(summary.incomplete, Some(StorageJournalIncompleteness::ArithmeticOverflow));
    }

    #[test]
    fn low32_groups_use_raw_numeric_low_bits_and_warm_reads_only_add_accesses() {
        let owner = Address::ZERO;
        for same_prefix in [false, true] {
            let mut ledger = StorageJournalLedger::default();
            for n in 0..64 {
                let key = if same_prefix { U256::from(n) << 224 } else { U256::from(n) };
                ledger.observe(owner, Some(n), AccessClass::Cold, false);
                ledger.observe_materialization(owner, key);
            }
            for _ in 0..3 {
                ledger.observe(owner, Some(64), AccessClass::Warm, false);
            }
            ledger.finish();
            let c = if same_prefix { 64 } else { 9 };
            assert_eq!(ledger.diagnostics.materialization_count_upper, 64);
            assert_eq!(ledger.diagnostics.max_low32_group_upper, if same_prefix { 64 } else { 1 });
            assert_eq!(ledger.diagnostics.comparison_equivalents_high_water_upper, c);
            assert_eq!(ledger.diagnostics.comparison_work_units_upper, c * (67 + 64 + 64));
        }
    }

    #[test]
    fn repeated_vacant_attempts_and_clear_refill_are_conservative_without_key_dedup() {
        let owner = Address::ZERO;
        let mut ledger = StorageJournalLedger::default();
        // A failed vacant load or a clear/refill can present the same absent key again.
        for _ in 0..4 {
            ledger.observe(owner, Some(0), AccessClass::Cold, false);
            ledger.observe_materialization(owner, U256::from(7));
        }
        ledger.finish();
        assert_eq!(ledger.diagnostics.materialization_count_upper, 4);
        assert_eq!(ledger.diagnostics.max_low32_group_upper, 4);
        assert_eq!(ledger.diagnostics.comparison_equivalents_high_water_upper, 4);
        assert_eq!(ledger.diagnostics.comparison_work_units_upper, 48);
    }

    #[test]
    fn initial_slots_are_seeded_without_operation_or_cold_entry_counts() {
        let owner = Address::ZERO;
        let mut ledger = StorageJournalLedger::default();
        ledger.observe_materialization(owner, U256::from(3));
        ledger.observe_materialization(owner, U256::from(4));
        ledger.observe(owner, Some(2), AccessClass::Warm, false);
        ledger.finish();
        assert_eq!(ledger.diagnostics.potential_sload_count, 1);
        assert_eq!(ledger.diagnostics.potential_rollback_entries, 0);
        assert_eq!(ledger.diagnostics.materialization_count_upper, 2);
        assert_eq!(ledger.diagnostics.max_low32_group_upper, 1);
        assert_eq!(ledger.diagnostics.comparison_equivalents_high_water_upper, 2);
        assert_eq!(ledger.diagnostics.comparison_work_units_upper, 6);
    }

    #[test]
    fn comparison_work_is_folded_per_owner_not_by_global_group_maxima() {
        let mut ledger = StorageJournalLedger::default();
        for (owner, count) in [(Address::ZERO, 64), (Address::repeat_byte(1), 1)] {
            for n in 0..count {
                ledger.observe(owner, Some(n), AccessClass::Cold, false);
                ledger.observe_materialization(owner, U256::from(n));
            }
        }
        ledger.finish();
        assert_eq!(ledger.diagnostics.materialization_count_upper, 65);
        assert_eq!(ledger.diagnostics.comparison_work_units_upper, 9 * 192 + 3);
    }

    #[test]
    fn materialization_and_comparison_arithmetic_fail_explicitly_without_saturation() {
        let owner = Address::ZERO;
        let mut ledger = StorageJournalLedger::default();
        ledger.diagnostics.materialization_count_upper = u64::MAX;
        ledger.observe_materialization(owner, U256::ZERO);
        assert!(ledger.owners.is_empty(), "failed update must not install a partial prefix");
        assert_eq!(ledger.diagnostics.materialization_count_upper, u64::MAX);
        assert_eq!(ledger.diagnostics.max_low32_group_upper, 0);
        assert_eq!(
            ledger.diagnostics.incomplete,
            Some(StorageJournalIncompleteness::ArithmeticOverflow)
        );

        let mut ledger = StorageJournalLedger::default();
        ledger.observe_materialization(owner, U256::ZERO);
        ledger.owners.get_mut(&owner).unwrap().potential_lookups = u64::MAX;
        ledger.finish();
        assert_eq!(ledger.diagnostics.comparison_work_units_upper, 0);
        assert_eq!(
            ledger.diagnostics.incomplete,
            Some(StorageJournalIncompleteness::ArithmeticOverflow)
        );
    }
}
