//! Transaction-local raw address and storage-key pair reserves for access-list construction.

use std::collections::BTreeMap;

use alloy_primitives::{Address, U256};

use super::generated::{
    ACCESS_LIST_ADDRESS_PAIR_BOUND, ACCESS_LIST_CONSTRUCTION_OTHER_PAIR_BOUND,
    ACCESS_LIST_CONSTRUCTION_PAIR_BOUND,
};

/// A failed observation makes the partial construction receipt unpriceable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessListConstructionIncompleteness {
    /// A required construction observation was unavailable.
    MissingCapture,
    /// The transaction scope received construction capture more than once.
    DuplicateCapture,
    /// Captured raw address-item count differs from transaction metadata.
    ItemCountMismatch,
    /// A raw count, prefix-group count or pair sum overflowed its checked domain.
    ArithmeticOverflow,
    /// Raw owner/bin counts or pair identities were inconsistent.
    InconsistentPairCounts,
    /// One of the raw pair selectors exceeded its independent pricing domain.
    PairBoundExceeded,
}

/// Raw-list concentration diagnostics, separate from journals and physical storage resources.
///
/// Address pairs count raw items within each transaction, including duplicates and empty keys.
/// Key pairs count raw key occurrences within each transaction/owner, split by numeric low32.
/// These conservative empirical reserves are not observed hash collisions or universal
/// hash-table-work bounds. Repeated identical items and keys intentionally remain chargeable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccessListConstructionDiagnostics {
    /// Raw address items represented by the capture, including duplicate owners.
    pub raw_address_items: u64,
    /// Sum of choose2(raw address items) per transaction, including repeated and empty owners.
    pub raw_address_pairs: u64,
    /// Raw key occurrences before any full-key or resource deduplication.
    pub raw_storage_keys: u64,
    /// Largest raw low32 group in any one transaction/owner scope.
    pub max_low32_group: u64,
    /// Sum of n*(n-1)/2 over raw transaction/owner/low32 groups, in pairs.
    pub comparison_pairs: u64,
    /// Same-transaction/owner raw occurrence pairs with different numeric low32 bits.
    pub other_pairs: u64,
    /// All same-transaction/owner raw occurrence pairs; exactly comparison_pairs+other_pairs.
    pub owner_pairs: u64,
    /// First observation failure; partial counters cannot publish a valid quote.
    pub incomplete: Option<AccessListConstructionIncompleteness>,
}

impl AccessListConstructionDiagnostics {
    /// A complete zero-work receipt, requiring no scratch allocation.
    pub(super) const EMPTY: Self = Self {
        raw_address_items: 0,
        raw_address_pairs: 0,
        raw_storage_keys: 0,
        max_low32_group: 0,
        comparison_pairs: 0,
        other_pairs: 0,
        owner_pairs: 0,
        incomplete: None,
    };

    /// Captures one whole raw list and drops its scratch map before execution starts.
    pub(super) fn capture<I, J>(items: I, expected_items: u64) -> Self
    where
        I: IntoIterator<Item = (Address, J)>,
        J: IntoIterator<Item = U256>,
    {
        let mut result = Self::EMPTY;
        let mut groups = BTreeMap::<(Address, u32), u64>::new();
        let mut owners = BTreeMap::<Address, u64>::new();
        for (owner, keys) in items {
            if let Err(error) = result.observe_address_item() {
                result.incomplete = Some(error);
                return result;
            }
            for key in keys {
                // Numeric low32 is independent of host byte order. The signed B256 conversion
                // happens at the hook; these are the first four bytes compared in LE U256.
                let count = groups.entry((owner, key.as_limbs()[0] as u32)).or_default();
                if let Err(error) = result.observe_count(count, owners.entry(owner).or_default()) {
                    result.incomplete = Some(error);
                    return result;
                }
            }
        }
        if result.raw_address_items != expected_items {
            result.incomplete = Some(AccessListConstructionIncompleteness::ItemCountMismatch);
        }
        result
    }

    /// Adds one raw address occurrence without allocating another map or deduplicating owners.
    fn observe_address_item(&mut self) -> Result<(), AccessListConstructionIncompleteness> {
        let overflow = AccessListConstructionIncompleteness::ArithmeticOverflow;
        let pairs = self.raw_address_pairs.checked_add(self.raw_address_items).ok_or(overflow)?;
        let items = self.raw_address_items.checked_add(1).ok_or(overflow)?;
        if pairs > ACCESS_LIST_ADDRESS_PAIR_BOUND {
            return Err(AccessListConstructionIncompleteness::PairBoundExceeded);
        }
        self.raw_address_pairs = pairs;
        self.raw_address_items = items;
        Ok(())
    }

    /// Adds one raw occurrence atomically after validating both pair sums, owner and bin counts.
    fn observe_count(
        &mut self,
        count: &mut u64,
        owner_count: &mut u64,
    ) -> Result<(), AccessListConstructionIncompleteness> {
        let inconsistent = AccessListConstructionIncompleteness::InconsistentPairCounts;
        let overflow = AccessListConstructionIncompleteness::ArithmeticOverflow;
        if self.comparison_pairs.checked_add(self.other_pairs) != Some(self.owner_pairs) {
            return Err(inconsistent);
        }
        let other_delta = owner_count.checked_sub(*count).ok_or(inconsistent)?;
        let keys = self.raw_storage_keys.checked_add(1).ok_or(overflow)?;
        let next_bin = count.checked_add(1).ok_or(overflow)?;
        let next_owner = owner_count.checked_add(1).ok_or(overflow)?;
        let low_pairs = self.comparison_pairs.checked_add(*count).ok_or(overflow)?;
        let other_pairs = self.other_pairs.checked_add(other_delta).ok_or(overflow)?;
        let owner_pairs = self.owner_pairs.checked_add(*owner_count).ok_or(overflow)?;
        if low_pairs.checked_add(other_pairs) != Some(owner_pairs) {
            return Err(inconsistent);
        }
        if low_pairs > ACCESS_LIST_CONSTRUCTION_PAIR_BOUND ||
            other_pairs > ACCESS_LIST_CONSTRUCTION_OTHER_PAIR_BOUND
        {
            return Err(AccessListConstructionIncompleteness::PairBoundExceeded);
        }
        self.raw_storage_keys = keys;
        self.max_low32_group = self.max_low32_group.max(next_bin);
        self.comparison_pairs = low_pairs;
        self.other_pairs = other_pairs;
        self.owner_pairs = owner_pairs;
        *count = next_bin;
        *owner_count = next_owner;
        Ok(())
    }

    /// Produces a known-empty receipt or an explicit missing nonempty observation.
    pub(super) const fn uncaptured(expected_items: u64) -> Self {
        Self {
            raw_address_items: expected_items,
            incomplete: if expected_items == 0 {
                None
            } else {
                Some(AccessListConstructionIncompleteness::MissingCapture)
            },
            ..Self::EMPTY
        }
    }

    /// Merges finalized independent scopes with checked sums and sticky incompleteness.
    pub(super) fn merge(&mut self, other: Self) {
        let incomplete = self.incomplete.or(other.incomplete);
        let sums = self
            .raw_address_items
            .checked_add(other.raw_address_items)
            .zip(self.raw_storage_keys.checked_add(other.raw_storage_keys))
            .zip(self.comparison_pairs.checked_add(other.comparison_pairs))
            .zip(self.other_pairs.checked_add(other.other_pairs))
            .zip(self.owner_pairs.checked_add(other.owner_pairs))
            .zip(self.raw_address_pairs.checked_add(other.raw_address_pairs));
        let Some((((((items, keys), pairs), other_pairs), owner_pairs), address_pairs)) = sums
        else {
            self.incomplete =
                incomplete.or(Some(AccessListConstructionIncompleteness::ArithmeticOverflow));
            return;
        };
        self.raw_address_items = items;
        self.raw_address_pairs = address_pairs;
        self.raw_storage_keys = keys;
        self.max_low32_group = self.max_low32_group.max(other.max_low32_group);
        self.comparison_pairs = pairs;
        self.other_pairs = other_pairs;
        self.owner_pairs = owner_pairs;
        self.incomplete = incomplete.or(
            if address_pairs > ACCESS_LIST_ADDRESS_PAIR_BOUND ||
                pairs > ACCESS_LIST_CONSTRUCTION_PAIR_BOUND ||
                other_pairs > ACCESS_LIST_CONSTRUCTION_OTHER_PAIR_BOUND
            {
                Some(AccessListConstructionIncompleteness::PairBoundExceeded)
            } else if pairs.checked_add(other_pairs) != Some(owner_pairs) {
                Some(AccessListConstructionIncompleteness::InconsistentPairCounts)
            } else {
                None
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            BlockPricingInput, FIXED_POINT_SCALE, ProposalPricingInput, TransactionMetadata,
            TransactionPayloadSelectors, ZkGasAccumulator, ZkGasBlockSnapshot, ZkGasError,
            ZkGasModel, ZkGasShadowStatus,
        },
        *,
    };

    fn begin(collector: &mut ZkGasAccumulator, items: u64) {
        collector
            .begin_transaction(TransactionMetadata {
                access_list_items: items,
                ..Default::default()
            })
            .unwrap();
        collector
            .observe_transaction_payload(TransactionPayloadSelectors {
                canonical_transaction_bytes: 100,
                nonempty_payload_transactions: 0,
            })
            .unwrap();
    }

    #[test]
    fn raw_address_items_have_independent_transaction_pair_reserve() {
        fn price(count: usize) -> i128 {
            let mut collector = ZkGasAccumulator::default();
            begin(&mut collector, count as u64);
            collector
                .observe_access_list_construction(
                    (0..count).map(|_| (Address::ZERO, Vec::<U256>::new())),
                )
                .unwrap();
            collector.commit_transaction().unwrap();
            match collector.committed_snapshot().shadow_report().status() {
                ZkGasShadowStatus::Incomplete { known, .. } => {
                    known.evm_local_known_subtotal_scaled()
                }
                other => panic!("unexpected report: {other:?}"),
            }
        }
        // All four raw empty-key occurrences count, including identical owners.
        assert_eq!(price(4) - price(1), (3 * 8192 + 6 * 128) * FIXED_POINT_SCALE);
    }

    #[test]
    fn raw_address_pairs_include_empty_duplicates_and_reset_at_transaction_boundaries() {
        for n in [0_u64, 1, 4, 6144] {
            let receipt = AccessListConstructionDiagnostics::capture(
                (0..n).map(|_| (Address::ZERO, Vec::<U256>::new())),
                n,
            );
            assert_eq!(receipt.raw_address_items, n);
            assert_eq!(receipt.raw_address_pairs, n * n.saturating_sub(1) / 2);
            assert_eq!((receipt.raw_storage_keys, receipt.owner_pairs), (0, 0));
            assert_eq!(receipt.incomplete, None);
        }
        let mut collector = ZkGasAccumulator::default();
        for commit in [true, false] {
            begin(&mut collector, 6144);
            collector
                .observe_access_list_construction(
                    (0..6144).map(|_| (Address::ZERO, Vec::<U256>::new())),
                )
                .unwrap();
            if commit {
                collector.commit_transaction().unwrap();
            } else {
                collector.reset_transaction();
            }
        }
        collector.reset_transaction(); // No duplicate finalization.
        let committed = collector.committed_snapshot();
        let attempted = collector.attempted_snapshot();
        assert_eq!(committed.access_list_address_pairs().unwrap(), 18_871_296);
        assert_eq!(attempted.block.access_list_address_pairs().unwrap(), 37_742_592);
        assert_eq!(attempted.block.access_list_items, 12_288);
        assert_ne!(37_742_592, 12_288 * 12_287 / 2);
        assert_eq!(attempted.transaction_count, 2);
        match attempted.block.shadow_report().status() {
            ZkGasShadowStatus::Incomplete { known, .. } => {
                assert_eq!(known.access_list_address_pairs(), 37_742_592);
                assert_eq!(known.access_list_address_scaled(), 4_831_051_776 * FIXED_POINT_SCALE);
            }
            other => panic!("unexpected report: {other:?}"),
        }
    }

    #[test]
    fn raw_address_pair_domains_atomic_overflow_and_sticky_failures() {
        use AccessListConstructionIncompleteness::{
            ArithmeticOverflow, MissingCapture, PairBoundExceeded,
        };
        for (items, pairs, error) in [
            (1, ACCESS_LIST_ADDRESS_PAIR_BOUND, PairBoundExceeded),
            (u64::MAX, 0, ArithmeticOverflow),
            (1, u64::MAX, ArithmeticOverflow),
        ] {
            let mut receipt = AccessListConstructionDiagnostics {
                raw_address_items: items,
                raw_address_pairs: pairs,
                ..AccessListConstructionDiagnostics::EMPTY
            };
            let before = receipt;
            assert_eq!(receipt.observe_address_item(), Err(error));
            assert_eq!(receipt, before);
        }
        let mut overflow = AccessListConstructionDiagnostics {
            raw_address_pairs: u64::MAX,
            incomplete: Some(MissingCapture),
            ..AccessListConstructionDiagnostics::EMPTY
        };
        overflow.merge(AccessListConstructionDiagnostics {
            raw_address_pairs: 1,
            ..AccessListConstructionDiagnostics::EMPTY
        });
        assert_eq!(overflow.raw_address_pairs, u64::MAX);
        assert_eq!(overflow.incomplete, Some(MissingCapture));
        let mut domain = AccessListConstructionDiagnostics {
            raw_address_pairs: ACCESS_LIST_ADDRESS_PAIR_BOUND,
            ..AccessListConstructionDiagnostics::EMPTY
        };
        domain.merge(AccessListConstructionDiagnostics {
            raw_address_pairs: 1,
            ..AccessListConstructionDiagnostics::EMPTY
        });
        assert_eq!(domain.incomplete, Some(PairBoundExceeded));
        let model = ZkGasModel;
        for pairs in [0, 1, 37_742_592, ACCESS_LIST_ADDRESS_PAIR_BOUND] {
            assert_eq!(
                model.price_access_list_address_pairs(pairs).unwrap(),
                i128::from(pairs) * 128 * FIXED_POINT_SCALE
            );
        }
        for pairs in [ACCESS_LIST_ADDRESS_PAIR_BOUND + 1, u64::MAX] {
            assert_eq!(
                model.price_access_list_address_pairs(pairs),
                Err(ZkGasError::SelectorOutOfRange {
                    selector: "access_list_address_pairs",
                    value: pairs
                })
            );
        }
        let mismatched = ZkGasBlockSnapshot { access_list_items: 1, ..Default::default() };
        assert_eq!(
            mismatched.access_list_address_pairs(),
            Err(ZkGasError::InvalidInput("access_list_address_item_count"))
        );
        assert!(matches!(mismatched.shadow_report().status(), ZkGasShadowStatus::Diagnostic(_)));
    }

    #[test]
    fn raw_address_pair_full_completion_and_once_are_consistent() {
        let mut collector = ZkGasAccumulator::default();
        begin(&mut collector, 4);
        collector
            .observe_access_list_construction((0..4).map(|_| (Address::ZERO, Vec::<U256>::new())))
            .unwrap();
        collector.commit_transaction().unwrap();
        let full = ZkGasModel
            .complete_block_shadow_report(
                &collector.committed_snapshot().shadow_report(),
                Default::default(),
                Default::default(),
            )
            .unwrap();
        assert_eq!(
            (full.access_list_address_pairs, full.access_list_address_scaled),
            (6, 768 * FIXED_POINT_SCALE)
        );
        let once = ZkGasModel
            .price_proposal_once(&ProposalPricingInput {
                canonical_transaction_bytes: 0,
                nonempty_payload_transactions: 0,
                true_unattempted_transactions: 0,
                source_blob_counts: &[1],
                proposal_blob_hash_count: 1,
                provider_blob_count: 1,
                provider_commitment_count: 1,
                provider_proof_count: 1,
                parent_mpt: Default::default(),
            })
            .unwrap();
        assert_eq!((once.access_list_address_pairs, once.access_list_address_scaled), (0, 0));
        for incomplete in [
            AccessListConstructionIncompleteness::MissingCapture,
            AccessListConstructionIncompleteness::DuplicateCapture,
            AccessListConstructionIncompleteness::ItemCountMismatch,
        ] {
            let snapshot = ZkGasBlockSnapshot {
                access_list_items: 4,
                access_list_construction: AccessListConstructionDiagnostics {
                    raw_address_items: 4,
                    raw_address_pairs: 6,
                    incomplete: Some(incomplete),
                    ..AccessListConstructionDiagnostics::EMPTY
                },
                ..Default::default()
            };
            assert_eq!(
                snapshot.access_list_address_pairs(),
                Err(ZkGasError::InvalidInput("incomplete_access_list_construction"))
            );
            assert!(matches!(snapshot.shadow_report().status(), ZkGasShadowStatus::Diagnostic(_)));
        }
    }

    #[test]
    fn different_low32_raw_pairs_have_their_own_nondiscounted_reserve() {
        fn price(keys: Vec<U256>) -> i128 {
            let mut collector = ZkGasAccumulator::default();
            begin(&mut collector, 1);
            collector.observe_access_list_construction([(Address::ZERO, keys)]).unwrap();
            collector.commit_transaction().unwrap();
            match collector.committed_snapshot().shadow_report().status() {
                ZkGasShadowStatus::Incomplete { known, .. } => {
                    known.evm_local_known_subtotal_scaled()
                }
                other => panic!("unexpected report: {other:?}"),
            }
        }
        let dense = price(vec![U256::ZERO; 3]);
        let distinct = price(vec![U256::from(1), U256::from(2), U256::from(3)]);
        assert_eq!(dense - distinct, 3 * (384 - 96) * FIXED_POINT_SCALE);
    }

    #[test]
    fn mixed_raw_pairs_match_a_quadratic_oracle_across_items_owners_and_permutations() {
        let a = Address::ZERO;
        let b = Address::with_last_byte(1);
        let entries = vec![
            (a, vec![U256::ZERO, U256::from(1), U256::ZERO]),
            (b, vec![U256::from(1), U256::from(2)]),
            (a, vec![U256::from(1) << 32, U256::from(2)]),
        ];
        let flat = entries
            .iter()
            .flat_map(|(owner, keys)| keys.iter().map(|key| (*owner, *key)))
            .collect::<Vec<_>>();
        let mut low = 0;
        let mut other = 0;
        for (index, (owner, key)) in flat.iter().enumerate() {
            for (prior_owner, prior_key) in &flat[..index] {
                if owner == prior_owner {
                    if key.as_limbs()[0] as u32 == prior_key.as_limbs()[0] as u32 {
                        low += 1;
                    } else {
                        other += 1;
                    }
                }
            }
        }
        for reverse in [false, true] {
            let mut items = entries.clone();
            if reverse {
                items.reverse();
                for (_, keys) in &mut items {
                    keys.reverse();
                }
            }
            let r = AccessListConstructionDiagnostics::capture(items, 3);
            assert_eq!(
                (r.comparison_pairs, r.other_pairs, r.owner_pairs),
                (low, other, low + other)
            );
            assert_eq!(r.incomplete, None);
        }
        let split = AccessListConstructionDiagnostics::capture(
            [(a, vec![U256::from(1)]), (b, vec![U256::from(2)])],
            2,
        );
        assert_eq!(split.owner_pairs, 0);
        let repeated_owner = AccessListConstructionDiagnostics::capture(
            [(a, vec![U256::from(1)]), (a, vec![U256::from(2)])],
            2,
        );
        assert_eq!(
            (
                repeated_owner.comparison_pairs,
                repeated_owner.other_pairs,
                repeated_owner.owner_pairs
            ),
            (0, 1, 1)
        );
        let identical = AccessListConstructionDiagnostics::capture([(a, vec![U256::ZERO; 4])], 1);
        assert_eq!(
            (identical.comparison_pairs, identical.other_pairs, identical.owner_pairs),
            (6, 0, 6)
        );
    }

    #[test]
    fn distinct_owner_pairs_reset_per_transaction_and_rejected_work_is_attempted_only() {
        let mut collector = ZkGasAccumulator::default();
        for commit in [true, false] {
            begin(&mut collector, 1);
            collector
                .observe_access_list_construction([(
                    Address::ZERO,
                    vec![U256::from(1), U256::from(2)],
                )])
                .unwrap();
            if commit {
                collector.commit_transaction().unwrap();
            } else {
                collector.reset_transaction();
            }
        }
        let committed = collector.committed_snapshot();
        let attempted = collector.attempted_snapshot();
        assert_eq!(
            (
                committed.access_list_construction.other_pairs,
                attempted.block.access_list_construction.other_pairs
            ),
            (1, 2)
        );
        assert_eq!(
            (
                committed.access_list_construction.owner_pairs,
                attempted.block.access_list_construction.owner_pairs
            ),
            (1, 2)
        );
        assert_eq!(committed.access_list_construction_pairs().unwrap(), 0);
        assert_eq!(committed.access_list_construction_other_pairs().unwrap(), 1);
        assert_eq!(attempted.block.access_list_construction_other_pairs().unwrap(), 2);
        assert_eq!(attempted.block.resources.unique_storage_slot_access_count, 0);
    }

    #[test]
    fn different_pair_bounds_inconsistent_receipts_and_atomic_updates_fail_closed() {
        let mut r = AccessListConstructionDiagnostics {
            other_pairs: ACCESS_LIST_CONSTRUCTION_PAIR_BOUND,
            owner_pairs: ACCESS_LIST_CONSTRUCTION_PAIR_BOUND,
            ..AccessListConstructionDiagnostics::EMPTY
        };
        let original = r;
        let mut bin = 0;
        let mut owner = 1;
        assert_eq!(
            r.observe_count(&mut bin, &mut owner),
            Err(AccessListConstructionIncompleteness::PairBoundExceeded)
        );
        assert_eq!(r, original);
        assert_eq!((bin, owner), (0, 1));
        let mut empty = AccessListConstructionDiagnostics::EMPTY;
        bin = 2;
        owner = 1;
        assert_eq!(
            empty.observe_count(&mut bin, &mut owner),
            Err(AccessListConstructionIncompleteness::InconsistentPairCounts)
        );
        assert_eq!(empty, AccessListConstructionDiagnostics::EMPTY);
        assert_eq!((bin, owner), (2, 1));
        let mut merged = original;
        merged.merge(AccessListConstructionDiagnostics {
            other_pairs: 1,
            owner_pairs: 1,
            ..AccessListConstructionDiagnostics::EMPTY
        });
        assert_eq!(
            merged.incomplete,
            Some(AccessListConstructionIncompleteness::PairBoundExceeded)
        );
        let mut sticky = AccessListConstructionDiagnostics {
            incomplete: Some(AccessListConstructionIncompleteness::MissingCapture),
            ..original
        };
        sticky.merge(AccessListConstructionDiagnostics {
            other_pairs: 1,
            owner_pairs: 1,
            ..AccessListConstructionDiagnostics::EMPTY
        });
        assert_eq!(sticky.incomplete, Some(AccessListConstructionIncompleteness::MissingCapture));
        let invalid = ZkGasBlockSnapshot {
            access_list_construction: AccessListConstructionDiagnostics {
                other_pairs: 1,
                ..AccessListConstructionDiagnostics::EMPTY
            },
            ..Default::default()
        };
        assert_eq!(
            invalid.access_list_construction_other_pairs(),
            Err(ZkGasError::InvalidInput("inconsistent_access_list_construction_pairs"))
        );
        let model = ZkGasModel;
        for pairs in [0, 1, ACCESS_LIST_CONSTRUCTION_PAIR_BOUND] {
            assert_eq!(
                model.price_access_list_construction_other_pairs(pairs).unwrap(),
                i128::from(pairs) * 96 * FIXED_POINT_SCALE
            );
        }
        assert!(matches!(
            model.price_access_list_construction_other_pairs(
                ACCESS_LIST_CONSTRUCTION_PAIR_BOUND + 1
            ),
            Err(ZkGasError::SelectorOutOfRange {
                selector: "access_list_construction_other_pairs",
                ..
            })
        ));
        let once = model
            .price_proposal_once(&ProposalPricingInput {
                canonical_transaction_bytes: 0,
                nonempty_payload_transactions: 0,
                true_unattempted_transactions: 0,
                source_blob_counts: &[1],
                proposal_blob_hash_count: 1,
                provider_blob_count: 1,
                provider_commitment_count: 1,
                provider_proof_count: 1,
                parent_mpt: Default::default(),
            })
            .unwrap();
        assert_eq!(
            (once.access_list_construction_other_pairs, once.access_list_construction_other_scaled),
            (0, 0)
        );
    }

    #[test]
    fn raw_groups_share_owner_items_and_count_duplicates_without_full_key_dedup() {
        let owner = Address::ZERO;
        let receipt = AccessListConstructionDiagnostics::capture(
            [
                (owner, vec![U256::ZERO, U256::from(1) << 32]),
                (owner, vec![U256::ZERO]),
                (Address::with_last_byte(1), vec![U256::ZERO]),
            ],
            3,
        );
        assert_eq!(receipt.raw_address_items, 3);
        assert_eq!(receipt.raw_storage_keys, 4);
        assert_eq!(receipt.max_low32_group, 3);
        assert_eq!(receipt.comparison_pairs, 3);
        assert_eq!(receipt.incomplete, None);
    }

    #[test]
    fn numeric_low32_and_distinct_transaction_scopes_do_not_create_false_pairs() {
        let one = AccessListConstructionDiagnostics::capture(
            [(Address::ZERO, vec![U256::from(1), U256::from(2), U256::from(3)])],
            1,
        );
        assert_eq!(one.comparison_pairs, 0);
        assert_eq!(one.max_low32_group, 1);
        let mut merged = one;
        merged.merge(one);
        assert_eq!(merged.raw_storage_keys, 6);
        assert_eq!(merged.comparison_pairs, 0);
        let dense =
            AccessListConstructionDiagnostics::capture([(Address::ZERO, vec![U256::ZERO; 3])], 1);
        let mut total = dense;
        total.merge(dense);
        assert_eq!(total.comparison_pairs, 6);
        assert_eq!(total.max_low32_group, 3);
    }

    #[test]
    fn empty_mismatched_and_missing_captures_remain_distinguishable() {
        let empty = AccessListConstructionDiagnostics::capture(
            std::iter::empty::<(Address, Vec<U256>)>(),
            0,
        );
        assert_eq!(empty, AccessListConstructionDiagnostics::EMPTY);
        assert_eq!(AccessListConstructionDiagnostics::uncaptured(0), empty);
        assert_eq!(
            AccessListConstructionDiagnostics::uncaptured(1).incomplete,
            Some(AccessListConstructionIncompleteness::MissingCapture)
        );
        let wrong =
            AccessListConstructionDiagnostics::capture([(Address::ZERO, Vec::<U256>::new())], 2);
        assert_eq!(wrong.incomplete, Some(AccessListConstructionIncompleteness::ItemCountMismatch));
    }

    #[test]
    fn checked_per_group_and_aggregate_failures_never_wrap_or_erase_prior_failure() {
        let mut receipt = AccessListConstructionDiagnostics::EMPTY;
        let mut count = u64::MAX;
        let mut owner_count = u64::MAX;
        assert_eq!(
            receipt.observe_count(&mut count, &mut owner_count),
            Err(AccessListConstructionIncompleteness::ArithmeticOverflow)
        );
        assert_eq!(receipt.comparison_pairs, 0);
        receipt.comparison_pairs = ACCESS_LIST_CONSTRUCTION_PAIR_BOUND;
        receipt.owner_pairs = ACCESS_LIST_CONSTRUCTION_PAIR_BOUND;
        count = 1;
        assert_eq!(
            receipt.observe_count(&mut count, &mut 1),
            Err(AccessListConstructionIncompleteness::PairBoundExceeded)
        );
        let mut overflow = AccessListConstructionDiagnostics {
            raw_storage_keys: u64::MAX,
            incomplete: Some(AccessListConstructionIncompleteness::MissingCapture),
            ..AccessListConstructionDiagnostics::EMPTY
        };
        overflow.merge(AccessListConstructionDiagnostics {
            raw_storage_keys: 1,
            ..AccessListConstructionDiagnostics::EMPTY
        });
        assert_eq!(overflow.raw_storage_keys, u64::MAX);
        assert_eq!(overflow.incomplete, Some(AccessListConstructionIncompleteness::MissingCapture));
        let mut bound = AccessListConstructionDiagnostics {
            comparison_pairs: ACCESS_LIST_CONSTRUCTION_PAIR_BOUND,
            owner_pairs: ACCESS_LIST_CONSTRUCTION_PAIR_BOUND,
            ..AccessListConstructionDiagnostics::EMPTY
        };
        bound.merge(AccessListConstructionDiagnostics {
            comparison_pairs: 1,
            owner_pairs: 1,
            ..AccessListConstructionDiagnostics::EMPTY
        });
        assert_eq!(bound.incomplete, Some(AccessListConstructionIncompleteness::PairBoundExceeded));
    }

    #[test]
    fn committed_and_rejected_scopes_keep_independent_receipts_without_cross_tx_pairs() {
        let mut collector = ZkGasAccumulator::default();
        begin(&mut collector, 1);
        collector.observe_access_list_construction([(Address::ZERO, vec![U256::ZERO; 3])]).unwrap();
        assert_eq!(collector.committed_snapshot().access_list_construction.comparison_pairs, 0);
        collector.commit_transaction().unwrap();
        begin(&mut collector, 1);
        collector.observe_access_list_construction([(Address::ZERO, vec![U256::ZERO; 3])]).unwrap();
        collector.reset_transaction();
        collector.reset_transaction();
        assert_eq!(collector.committed_snapshot().access_list_construction.comparison_pairs, 3);
        let attempted = collector.attempted_snapshot();
        assert_eq!(attempted.transaction_count, 2);
        assert_eq!(attempted.block.access_list_construction.comparison_pairs, 6);
        assert_eq!(attempted.block.access_list_construction.raw_storage_keys, 6);
        assert_eq!(attempted.block.resources.unique_storage_slot_access_count, 0);
        assert!(matches!(
            attempted.block.shadow_report().status(),
            ZkGasShadowStatus::Incomplete { .. }
        ));
    }

    #[test]
    fn missing_duplicate_and_mismatched_receipts_do_not_publish_a_valid_quote() {
        let mut missing = ZkGasAccumulator::default();
        begin(&mut missing, 1);
        missing.commit_transaction().unwrap();
        assert_eq!(
            missing.committed_snapshot().access_list_construction.incomplete,
            Some(AccessListConstructionIncompleteness::MissingCapture)
        );
        assert!(matches!(
            missing.committed_snapshot().shadow_report().status(),
            ZkGasShadowStatus::Diagnostic(_)
        ));
        let mut duplicate = ZkGasAccumulator::default();
        begin(&mut duplicate, 1);
        duplicate.observe_access_list_construction([(Address::ZERO, vec![U256::ZERO; 3])]).unwrap();
        assert!(
            duplicate
                .observe_access_list_construction([(Address::ZERO, Vec::<U256>::new())])
                .is_err()
        );
        duplicate.commit_transaction().unwrap();
        let snapshot = duplicate.committed_snapshot();
        assert_eq!(snapshot.access_list_construction.comparison_pairs, 3);
        assert_eq!(
            snapshot.access_list_construction.incomplete,
            Some(AccessListConstructionIncompleteness::DuplicateCapture)
        );
        assert!(matches!(snapshot.shadow_report().status(), ZkGasShadowStatus::Diagnostic(_)));
        let mut mismatch = ZkGasAccumulator::default();
        begin(&mut mismatch, 2);
        assert!(
            mismatch.observe_access_list_construction([(Address::ZERO, vec![U256::ZERO])]).is_err()
        );
        mismatch.reset_transaction();
        assert_eq!(
            mismatch.attempted_snapshot().block.access_list_construction.incomplete,
            Some(AccessListConstructionIncompleteness::ItemCountMismatch)
        );
        let mut empty = ZkGasAccumulator::default();
        begin(&mut empty, 0);
        empty.commit_transaction().unwrap();
        assert_eq!(
            empty.committed_snapshot().access_list_construction,
            AccessListConstructionDiagnostics::EMPTY
        );
        assert!(matches!(
            empty.committed_snapshot().shadow_report().status(),
            ZkGasShadowStatus::Incomplete { .. }
        ));
    }

    #[test]
    fn normalized_pair_component_has_its_own_domain_and_is_zero_at_proposal_once() {
        let model = ZkGasModel;
        for pairs in [0, 1, 1_000_000_001, ACCESS_LIST_CONSTRUCTION_PAIR_BOUND] {
            assert_eq!(
                model.price_access_list_construction_pairs(pairs).unwrap(),
                i128::from(pairs) * 384 * FIXED_POINT_SCALE
            );
        }
        for pairs in [ACCESS_LIST_CONSTRUCTION_PAIR_BOUND + 1, u64::MAX] {
            assert_eq!(
                model.price_access_list_construction_pairs(pairs),
                Err(ZkGasError::SelectorOutOfRange {
                    selector: "access_list_construction_pairs",
                    value: pairs
                })
            );
        }
        let block = model
            .price_block(&BlockPricingInput {
                started_non_anchor_transactions: 0,
                native_value_transfers: 0,
                authorization_validations: 0,
                access_list_items: 1,
                access_list_construction_pairs: 3,
                access_list_construction_other_pairs: 0,
                access_list_address_pairs: 0,
                code_preparation: Default::default(),
                storage_journal_work_units: 0,
                operation_routes: &[],
                resources: Default::default(),
                transaction_payload: Default::default(),
                mpt: Default::default(),
            })
            .unwrap();
        assert_eq!(block.access_list_construction_pairs, 3);
        assert_eq!(block.access_list_construction_scaled, 3 * 384 * FIXED_POINT_SCALE);
        let snapshot = ZkGasBlockSnapshot {
            access_list_items: 1,
            access_list_construction: AccessListConstructionDiagnostics {
                raw_address_items: 1,
                raw_address_pairs: 0,
                raw_storage_keys: 3,
                max_low32_group: 3,
                comparison_pairs: 3,
                other_pairs: 0,
                owner_pairs: 3,
                incomplete: None,
            },
            ..Default::default()
        };
        let completed = model
            .complete_block_shadow_report(
                &snapshot.shadow_report(),
                Default::default(),
                Default::default(),
            )
            .unwrap();
        assert_eq!(completed, block);
        let once = model
            .price_proposal_once(&ProposalPricingInput {
                canonical_transaction_bytes: 0,
                nonempty_payload_transactions: 0,
                true_unattempted_transactions: 0,
                source_blob_counts: &[1],
                proposal_blob_hash_count: 1,
                provider_blob_count: 1,
                provider_commitment_count: 1,
                provider_proof_count: 1,
                parent_mpt: Default::default(),
            })
            .unwrap();
        assert_eq!(once.access_list_construction_pairs, 0);
        assert_eq!(once.access_list_construction_scaled, 0);
    }
}
