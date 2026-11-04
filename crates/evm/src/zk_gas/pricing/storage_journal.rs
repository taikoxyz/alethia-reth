//! Unpriced potential ordinary-storage journal work, independent of tariff selectors.

use std::collections::BTreeMap;

use alloy_primitives::Address;

use super::AccessClass;

/// Why potential storage-work diagnostics cannot be treated as complete.
///
/// These failures never invalidate the price, change execution, or trigger admission policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageJournalIncompleteness {
    /// The pre-step storage identity or stack input could not be captured.
    MissingStorageInput,
    /// The already-loaded journal account was unavailable at capture time.
    MissingFootprint,
    /// A conversion, count, unit sum, or rollback product exceeded `u64`.
    ArithmeticOverflow,
    /// No observing collector was available for this report.
    CollectorUnavailable,
}

/// Seed-independent, unpriced proxy for ordinary SLOAD/SSTORE journal work.
///
/// Each captured potential lookup contributes `n_before + 1` units, where `n_before` is the
/// physical transaction-local storage-map length for the storage owner. These are neither
/// guest hash probes nor measured PGU. Static/stipend/OOG failures can be counted before any
/// actual journal lookup; missing stack/account metadata marks this summary incomplete.
/// Frame and outer reverts do not remove computational work. Access-list declarations,
/// resource finalization walks, and transient storage are excluded.
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
    /// First reason these partial diagnostics are incomplete, independent of pricing errors.
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
        incomplete: None,
    };

    /// Adds one finalized transaction summary without affecting priced collector state.
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
#[derive(Clone, Copy, Debug, Default)]
struct OwnerWork {
    /// Conservative maximum physical occupancy after a potential lookup, in slots.
    high_water_upper: u64,
    /// Cold accesses plus all SSTORE attempts, in potential rollback entries.
    potential_entries: u64,
}

/// One transaction's compact ledger, allocated only through the observing pricing path.
#[derive(Debug, Default)]
pub(super) struct StorageJournalLedger {
    /// Additive direct-work diagnostics and finalized rollback reserve.
    pub(super) diagnostics: StorageJournalDiagnostics,
    /// Only owners reached by captured ordinary storage opcodes, reset at transaction end.
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
            let previous = self.owners.get(&owner).copied().unwrap_or_default();
            let owner = OwnerWork {
                high_water_upper: previous.high_water_upper.max(units),
                potential_entries: previous.potential_entries.checked_add(entries)?,
            };
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
            Some((owner, diagnostics))
        })();
        match next {
            Some((work, diagnostics)) => {
                self.owners.insert(owner, work);
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
        ledger.owners.insert(owner, OwnerWork { high_water_upper: 1, potential_entries: u64::MAX });
        ledger.observe(owner, Some(0), AccessClass::Cold, false);
        assert_eq!(
            ledger.diagnostics.incomplete,
            Some(StorageJournalIncompleteness::ArithmeticOverflow)
        );
        let mut ledger = StorageJournalLedger::default();
        ledger.owners.insert(owner, OwnerWork { high_water_upper: u64::MAX, potential_entries: 2 });
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
}
