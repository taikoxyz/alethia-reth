//! Block-local read-through and forward storage-map population and comparison reserve.

use alloy_primitives::{Address, U256};
use std::collections::{BTreeMap, BTreeSet};

/// Why a finalized storage receipt or progressive reserve cannot support a valid quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageTransitionIncompleteness {
    /// No final physical storage receipt was supplied for the transaction.
    MissingPopulation,
    /// More than one final receipt was supplied for the same transaction.
    DuplicatePopulation,
    /// A purported physical receipt repeated one owner/key pair.
    DuplicateKey,
    /// Checked arithmetic or the independent work-unit domain was exceeded.
    ArithmeticOverflow,
    /// No observing collector was available for the requested report.
    CollectorUnavailable,
}

/// Empirical read-through/forward storage reserve; these are not guest hash-probe counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StorageTransitionDiagnostics {
    /// Sum of physical storage entries across finalized transaction receipts.
    pub physical_slot_receipts: u64,
    /// Sum of final transaction-relative changed slots, including cross-TX restoration.
    pub changed_slots: u64,
    /// Distinct observed owner/key pairs in this block prefix.
    pub observed_storage_slots: u64,
    /// Largest single-owner union population in this prefix.
    pub max_owner_slots: u64,
    /// Largest single-owner group sharing semantic raw-key low32.
    pub max_low32_group: u64,
    /// Largest owner C=ceil((N+7M)/8), not a formula over total population.
    pub comparison_equivalents: u64,
    /// Sum of physical key counts plus twice each transaction/owner changed count.
    pub query_budget: u64,
    /// Sum of owner C after the whole incoming batch times its J+2X budget.
    pub work_units: u64,
    /// Number of single-use physical receipts included in this prefix.
    pub completed_transactions: u64,
    /// First sticky reason these partial diagnostics cannot be quoted.
    pub incomplete: Option<StorageTransitionIncompleteness>,
}
impl StorageTransitionDiagnostics {
    /// Complete zero for a fresh prefix; nonempty readonly receipts reserve read-through work.
    pub const EMPTY: Self = Self {
        physical_slot_receipts: 0,
        changed_slots: 0,
        observed_storage_slots: 0,
        max_owner_slots: 0,
        max_low32_group: 0,
        comparison_equivalents: 0,
        query_budget: 0,
        work_units: 0,
        completed_transactions: 0,
        incomplete: None,
    };
}

/// One finalized transaction's physical keys, each with its transaction-relative changed flag.
#[derive(Debug, Default)]
pub(super) struct StorageTransitionReceipt {
    /// Owner-separated physical keys; duplicates are invalid rather than extra work.
    owners: BTreeMap<Address, BTreeMap<U256, bool>>,
    /// First invalid capture; prefixes keep prior complete values instead of pricing a partial
    /// set.
    pub(super) incomplete: Option<StorageTransitionIncompleteness>,
}
impl StorageTransitionReceipt {
    /// Captures the actual final map iterator; never declared AL or logical resource keys.
    pub(super) fn capture(slots: impl IntoIterator<Item = (Address, U256, bool)>) -> Self {
        let mut receipt = Self::default();
        for (owner, key, changed) in slots {
            let keys = receipt.owners.entry(owner).or_default();
            if let std::collections::btree_map::Entry::Vacant(entry) = keys.entry(key) {
                entry.insert(changed);
            } else {
                receipt.incomplete = Some(StorageTransitionIncompleteness::DuplicateKey);
                break;
            }
        }
        receipt
    }
}

/// Ordered virtual membership for one owner in one independent pricing prefix.
#[derive(Debug, Default)]
struct OwnerPopulation {
    /// Full-key identity prevents repeated transactions from multiplying population.
    keys: BTreeSet<U256>,
    /// Raw-low32 density among that owner's observed physical keys.
    groups: BTreeMap<u32, u64>,
    /// Largest group retained monotonically across clear/restore/revert.
    max_group: u64,
}

/// One owner delta prepared before any prefix mutation.
struct OwnerDelta {
    /// Actual state address whose population changes.
    address: Address,
    /// New full-key identities only.
    added: BTreeSet<U256>,
    /// Checked increments for that owner's raw-low32 groups.
    groups: BTreeMap<u32, u64>,
    /// Resulting largest owner group.
    max_group: u64,
}

/// Per-block virtual population; committed and attempted prefixes own separate instances.
#[derive(Debug, Default)]
pub(super) struct StorageTransitionLedger {
    /// Independent owner populations, never the EVM or DB container itself.
    owners: BTreeMap<Address, OwnerPopulation>,
    /// Additive work and scalar population diagnostics only.
    diagnostics: StorageTransitionDiagnostics,
}
impl StorageTransitionLedger {
    /// Validates every owner delta and domain before changing persistent populations or work.
    pub(super) fn merge(&mut self, receipt: Option<&StorageTransitionReceipt>) {
        let Some(receipt) = receipt else {
            self.diagnostics
                .incomplete
                .get_or_insert(StorageTransitionIncompleteness::MissingPopulation);
            return;
        };
        if let Some(reason) = receipt.incomplete {
            self.diagnostics.incomplete.get_or_insert(reason);
        }
        if self.diagnostics.incomplete.is_some() {
            return;
        }
        let next = (|| {
            let mut diagnostics = self.diagnostics;
            let mut deltas = Vec::new();
            for (address, keys) in &receipt.owners {
                let previous = self.owners.get(address);
                let mut added = BTreeSet::new();
                let mut groups = BTreeMap::<u32, u64>::new();
                for key in keys.keys() {
                    if !previous.is_some_and(|owner| owner.keys.contains(key)) {
                        added.insert(*key);
                        let count = groups.entry(key.as_limbs()[0] as u32).or_default();
                        *count = count.checked_add(1)?;
                    }
                }
                let previous_n = previous.map_or(0, |owner| owner.keys.len());
                let n = u64::try_from(previous_n)
                    .ok()?
                    .checked_add(u64::try_from(added.len()).ok()?)?;
                let mut m = previous.map_or(0, |owner| owner.max_group);
                for (prefix, count) in &groups {
                    let old =
                        previous.and_then(|owner| owner.groups.get(prefix)).copied().unwrap_or(0);
                    m = m.max(old.checked_add(*count)?);
                }
                let numerator = n.checked_add(m.checked_mul(7)?)?;
                let c = numerator / 8 + u64::from(numerator % 8 != 0);
                let j = u64::try_from(keys.len()).ok()?;
                let x = u64::try_from(keys.values().filter(|flag| **flag).count()).ok()?;
                let budget = j.checked_add(x.checked_mul(2)?)?;
                diagnostics.physical_slot_receipts =
                    diagnostics.physical_slot_receipts.checked_add(j)?;
                diagnostics.changed_slots = diagnostics.changed_slots.checked_add(x)?;
                diagnostics.observed_storage_slots = diagnostics
                    .observed_storage_slots
                    .checked_add(u64::try_from(added.len()).ok()?)?;
                diagnostics.max_owner_slots = diagnostics.max_owner_slots.max(n);
                diagnostics.max_low32_group = diagnostics.max_low32_group.max(m);
                diagnostics.comparison_equivalents = diagnostics.comparison_equivalents.max(c);
                diagnostics.query_budget = diagnostics.query_budget.checked_add(budget)?;
                diagnostics.work_units =
                    diagnostics.work_units.checked_add(c.checked_mul(budget)?)?;
                deltas.push(OwnerDelta { address: *address, added, groups, max_group: m });
            }
            if diagnostics.work_units > super::generated::STORAGE_TRANSITION_WORK_BOUND {
                return None;
            }
            diagnostics.completed_transactions =
                diagnostics.completed_transactions.checked_add(1)?;
            Some((deltas, diagnostics))
        })();
        match next {
            Some((deltas, diagnostics)) => {
                for delta in deltas {
                    let owner = self.owners.entry(delta.address).or_default();
                    owner.keys.extend(delta.added);
                    for (prefix, count) in delta.groups {
                        // The complete incoming batch and existing count were checked above.
                        *owner.groups.entry(prefix).or_default() += count;
                    }
                    owner.max_group = delta.max_group;
                }
                self.diagnostics = diagnostics;
            }
            None => {
                self.diagnostics.incomplete =
                    Some(StorageTransitionIncompleteness::ArithmeticOverflow);
            }
        }
    }
    /// Exposes scalar diagnostics without cloning private virtual populations.
    pub(super) const fn diagnostics(&self) -> StorageTransitionDiagnostics {
        self.diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::generated::{STORAGE_TRANSITION_WORK_BOUND, STORAGE_TRANSITION_WORK_TARIFF},
        *,
    };
    fn receipt(rows: &[(Address, U256, bool)]) -> StorageTransitionReceipt {
        StorageTransitionReceipt::capture(rows.iter().copied())
    }
    #[test]
    fn read_through_queries_are_reserved_once_per_receipt_with_owner_density() {
        let owner = Address::ZERO;
        let mut ledger = StorageTransitionLedger::default();
        let keys: Vec<_> = (1_u64..=4).map(|i| U256::from(i) << 48).collect();
        ledger.merge(Some(&receipt(
            &keys.iter().map(|key| (owner, *key, false)).collect::<Vec<_>>(),
        )));
        assert_eq!((ledger.diagnostics().query_budget, ledger.diagnostics().work_units), (4, 16));
        ledger.merge(Some(&receipt(&[(owner, keys[0], false), (owner, keys[1], false)])));
        assert_eq!(
            (ledger.diagnostics().observed_storage_slots, ledger.diagnostics().work_units),
            (4, 24)
        );
        ledger.merge(Some(&receipt(&[(owner, keys[0], true)])));
        assert_eq!((ledger.diagnostics().query_budget, ledger.diagnostics().work_units), (9, 36));
        ledger.merge(Some(&receipt(&[(Address::repeat_byte(1), keys[0], true)])));
        assert_eq!((ledger.diagnostics().query_budget, ledger.diagnostics().work_units), (12, 39));
    }

    #[test]
    fn growth_readonly_then_write_and_repeated_key_budget() {
        let a = Address::ZERO;
        let mut ledger = StorageTransitionLedger::default();
        ledger.merge(Some(&receipt(&[(a, U256::from(1), true), (a, U256::from(2), false)])));
        let first = ledger.diagnostics();
        assert_eq!(
            (
                first.physical_slot_receipts,
                first.changed_slots,
                first.observed_storage_slots,
                first.comparison_equivalents,
                first.query_budget,
                first.work_units
            ),
            (2, 1, 2, 2, 4, 8)
        );
        ledger.merge(Some(&receipt(&[(a, U256::from(3), false)])));
        assert_eq!(
            (ledger.diagnostics().observed_storage_slots, ledger.diagnostics().work_units),
            (3, 10)
        );
        let same_prefix = (U256::from(1) << 64) + U256::from(1);
        ledger.merge(Some(&receipt(&[(a, U256::from(1), true), (a, same_prefix, true)])));
        let d = ledger.diagnostics();
        assert_eq!(
            (
                d.observed_storage_slots,
                d.max_low32_group,
                d.comparison_equivalents,
                d.changed_slots,
                d.query_budget,
                d.work_units
            ),
            (4, 2, 3, 3, 11, 28)
        );
        assert_eq!(
            i128::from(d.work_units) * STORAGE_TRANSITION_WORK_TARIFF,
            10752 * 10_i128.pow(18)
        );
    }
    #[test]
    fn batch_order_owner_separation_and_readonly_population_are_valid() {
        let a = Address::ZERO;
        let b = Address::repeat_byte(1);
        let rows: Vec<_> = (1..=6)
            .map(|n| (a, U256::from(n) << 64, n == 1))
            .chain((1..=10).map(|n| (b, U256::from(n), false)))
            .collect();
        let mut reverse = rows.clone();
        reverse.reverse();
        let mut one = StorageTransitionLedger::default();
        let mut two = StorageTransitionLedger::default();
        one.merge(Some(&receipt(&rows)));
        two.merge(Some(&receipt(&reverse)));
        assert_eq!(one.diagnostics(), two.diagnostics());
        let d = one.diagnostics();
        assert_eq!(
            (
                d.observed_storage_slots,
                d.max_owner_slots,
                d.max_low32_group,
                d.comparison_equivalents,
                d.work_units
            ),
            (16, 10, 6, 6, 78)
        );
        let mut readonly = StorageTransitionLedger::default();
        readonly.merge(Some(&receipt(&[(a, U256::ZERO, false)])));
        assert_eq!(
            (readonly.diagnostics().observed_storage_slots, readonly.diagnostics().work_units),
            (1, 1)
        );
    }
    #[test]
    fn repeated_changes_are_charged_again_without_repopulating_union() {
        let a = Address::ZERO;
        let mut ledger = StorageTransitionLedger::default();
        for _ in 0..3 {
            ledger.merge(Some(&receipt(&[(a, U256::ZERO, true)])));
        }
        let d = ledger.diagnostics();
        assert_eq!(
            (
                d.physical_slot_receipts,
                d.changed_slots,
                d.observed_storage_slots,
                d.query_budget,
                d.work_units
            ),
            (3, 3, 1, 9, 9)
        );
    }
    #[test]
    fn missing_duplicate_and_domain_failure_are_sticky_and_atomic() {
        let a = Address::ZERO;
        let mut ledger = StorageTransitionLedger::default();
        ledger.merge(Some(&receipt(&[])));
        assert_eq!(ledger.diagnostics().completed_transactions, 1);
        ledger.merge(None);
        ledger.merge(Some(&receipt(&[(a, U256::ZERO, true)])));
        assert_eq!(
            ledger.diagnostics().incomplete,
            Some(StorageTransitionIncompleteness::MissingPopulation)
        );
        assert_eq!(ledger.diagnostics().observed_storage_slots, 0);
        let mut duplicate = StorageTransitionLedger::default();
        duplicate.merge(Some(&receipt(&[(a, U256::ZERO, false), (a, U256::ZERO, true)])));
        assert_eq!(
            duplicate.diagnostics().incomplete,
            Some(StorageTransitionIncompleteness::DuplicateKey)
        );
        let mut domain = StorageTransitionLedger::default();
        domain.diagnostics.work_units = STORAGE_TRANSITION_WORK_BOUND;
        domain.merge(Some(&receipt(&[(a, U256::ZERO, true)])));
        assert_eq!(
            domain.diagnostics().incomplete,
            Some(StorageTransitionIncompleteness::ArithmeticOverflow)
        );
        assert!(domain.owners.is_empty());
        assert_eq!(domain.diagnostics().work_units, STORAGE_TRANSITION_WORK_BOUND);
    }
}

#[cfg(test)]
mod layout_and_finite_tests {
    use super::*;
    #[test]
    fn big_endian_construction_uses_numeric_low32_and_batch_ceiling() {
        let mut ledger = StorageTransitionLedger::default();
        let owner = Address::ZERO;
        let mut raw = [0_u8; 32];
        raw[0] = 1;
        let high = U256::from_be_bytes(raw);
        let receipt = StorageTransitionReceipt::capture([
            (owner, U256::ZERO, true),
            (owner, high, true),
            (owner, U256::from(1), true),
        ]);
        ledger.merge(Some(&receipt));
        let d = ledger.diagnostics();
        assert_eq!(
            (d.observed_storage_slots, d.max_low32_group, d.comparison_equivalents, d.work_units),
            (3, 2, 3, 27)
        );
    }
    #[test]
    fn packed_read_shape_reserves_queries_without_claiming_actual_pgu() {
        let owner = Address::ZERO;
        let mut ledger = StorageTransitionLedger::default();
        for t in 0_u64..89 {
            let mut slots = vec![(owner, U256::ZERO, true)];
            slots.extend((1..=128).map(|i| (owner, U256::from(t * 128 + i) << 48, false)));
            ledger.merge(Some(&StorageTransitionReceipt::capture(slots)));
        }
        let d = ledger.diagnostics();
        assert_eq!(
            (d.physical_slot_receipts, d.changed_slots, d.observed_storage_slots),
            (11481, 89, 11393)
        );
        assert_eq!((d.query_budget, d.work_units), (11659, 67_167_499));
    }

    #[test]
    fn finite_transition_shape_has_target_units_without_claiming_whole_block_units() {
        let mut ledger = StorageTransitionLedger::default();
        let owner = Address::ZERO;
        for t in 0_u64..64 {
            let mut slots = vec![(owner, U256::ZERO, true)];
            slots.extend((1..=64).map(|i| (owner, U256::from(t * 64 + i) << 48, true)));
            ledger.merge(Some(&StorageTransitionReceipt::capture(slots)));
        }
        let d = ledger.diagnostics();
        assert_eq!(
            (d.physical_slot_receipts, d.changed_slots, d.observed_storage_slots),
            (4160, 4160, 4097)
        );
        assert_eq!((d.query_budget, d.work_units), (12480, 25_970_880));
        assert_eq!(
            super::super::ZkGasModel.price_storage_transition_work(d.work_units).unwrap(),
            9_972_817_920 * super::super::FIXED_POINT_SCALE
        );
    }
}
