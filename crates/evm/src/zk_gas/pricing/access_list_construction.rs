//! Transaction-local raw-prefix reserve for access-list warm-set construction.

use std::collections::BTreeMap;

use alloy_primitives::{Address, U256};

use super::generated::ACCESS_LIST_CONSTRUCTION_PAIR_BOUND;

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
    /// The comparison-pair selector exceeded its independent pricing domain.
    PairBoundExceeded,
}

/// Raw-list concentration diagnostics, separate from journals and physical storage resources.
///
/// A pair is two raw key occurrences in one transaction/owner scope with identical numeric
/// low32 bits. These are a conservative empirical reserve, not observed hash collisions or a
/// universal hash-table-work bound. Repeated identical raw keys intentionally count as pairs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccessListConstructionDiagnostics {
    /// Raw address items represented by the capture, including duplicate owners.
    pub raw_address_items: u64,
    /// Raw key occurrences before any full-key or resource deduplication.
    pub raw_storage_keys: u64,
    /// Largest raw low32 group in any one transaction/owner scope.
    pub max_low32_group: u64,
    /// Sum of n*(n-1)/2 over raw transaction/owner/low32 groups, in pairs.
    pub comparison_pairs: u64,
    /// First observation failure; partial counters cannot publish a valid quote.
    pub incomplete: Option<AccessListConstructionIncompleteness>,
}

impl AccessListConstructionDiagnostics {
    /// A complete zero-work receipt, requiring no scratch allocation.
    pub(super) const EMPTY: Self = Self {
        raw_address_items: 0,
        raw_storage_keys: 0,
        max_low32_group: 0,
        comparison_pairs: 0,
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
        for (owner, keys) in items {
            let Some(items) = result.raw_address_items.checked_add(1) else {
                result.incomplete = Some(AccessListConstructionIncompleteness::ArithmeticOverflow);
                return result;
            };
            result.raw_address_items = items;
            for key in keys {
                // Numeric low32 is independent of host byte order. The signed B256 conversion
                // happens at the hook; these are the first four bytes compared in LE U256.
                let count = groups.entry((owner, key.as_limbs()[0] as u32)).or_default();
                if let Err(error) = result.observe_count(count) {
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

    /// Adds one raw key using checked arithmetic before updating either receipt or group.
    fn observe_count(
        &mut self,
        count: &mut u64,
    ) -> Result<(), AccessListConstructionIncompleteness> {
        let keys = self
            .raw_storage_keys
            .checked_add(1)
            .ok_or(AccessListConstructionIncompleteness::ArithmeticOverflow)?;
        let next =
            count.checked_add(1).ok_or(AccessListConstructionIncompleteness::ArithmeticOverflow)?;
        let pairs = self
            .comparison_pairs
            .checked_add(*count)
            .ok_or(AccessListConstructionIncompleteness::ArithmeticOverflow)?;
        if pairs > ACCESS_LIST_CONSTRUCTION_PAIR_BOUND {
            return Err(AccessListConstructionIncompleteness::PairBoundExceeded);
        }
        self.raw_storage_keys = keys;
        self.max_low32_group = self.max_low32_group.max(next);
        self.comparison_pairs = pairs;
        *count = next;
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
            .zip(self.comparison_pairs.checked_add(other.comparison_pairs));
        let Some(((items, keys), pairs)) = sums else {
            self.incomplete =
                incomplete.or(Some(AccessListConstructionIncompleteness::ArithmeticOverflow));
            return;
        };
        self.raw_address_items = items;
        self.raw_storage_keys = keys;
        self.max_low32_group = self.max_low32_group.max(other.max_low32_group);
        self.comparison_pairs = pairs;
        self.incomplete = incomplete.or(if pairs > ACCESS_LIST_CONSTRUCTION_PAIR_BOUND {
            Some(AccessListConstructionIncompleteness::PairBoundExceeded)
        } else {
            None
        });
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
        assert_eq!(
            receipt.observe_count(&mut count),
            Err(AccessListConstructionIncompleteness::ArithmeticOverflow)
        );
        assert_eq!(receipt.comparison_pairs, 0);
        receipt.comparison_pairs = ACCESS_LIST_CONSTRUCTION_PAIR_BOUND;
        count = 1;
        assert_eq!(
            receipt.observe_count(&mut count),
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
            ..AccessListConstructionDiagnostics::EMPTY
        };
        bound.merge(AccessListConstructionDiagnostics {
            comparison_pairs: 1,
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
                raw_storage_keys: 3,
                max_low32_group: 3,
                comparison_pairs: 3,
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
