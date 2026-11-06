//! Runtime mode and typed shadow-report contracts for zk gas pricing.

use super::{
    AccessListConstructionDiagnostics, AccessListConstructionIncompleteness,
    CodePreparationSelectors, RUNTIME_SPEC_SHA256, ResourceSelectors, StorageJournalDiagnostics,
    StorageJournalIncompleteness, TransactionPayloadSelectors, TransientStorageJournalDiagnostics,
    ZkGasAccumulatorError, ZkGasBlockSnapshot, ZkGasError, ZkGasModel,
    model::KnownBlockPricingInput,
};
use core::fmt;

/// Priced block components observed from execution hooks and the signed transaction pipeline.
///
/// The resource term is only the EVM-local subset: pre-block system calls and validation can load
/// additional state directly into the block database cache. There is deliberately no exact
/// block-end resource term, MPT field, or complete-block total in this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZkGasKnownBlockBreakdown {
    /// Block fixed-cost contribution in fixed-point units.
    fixed_scaled: i128,
    /// Raw authorization validations, including entries later ignored.
    authorization_validations: u64,
    /// Authorization validation contribution in fixed-point PGU.
    authorization_scaled: i128,
    /// Raw access-list address items retained with their sealed contribution.
    access_list_items: u64,
    /// Raw access-list item contribution in normalized fixed-point PGU.
    access_list_scaled: i128,
    /// Raw-prefix pairs from complete transaction-local construction receipts.
    access_list_construction_pairs: u64,
    /// Independent normalized construction supplement, included once in the known subtotal.
    access_list_construction_scaled: i128,
    /// Raw same-owner occurrence pairs with different numeric low32 bits.
    access_list_construction_other_pairs: u64,
    /// Independent normalized different-prefix pair contribution.
    access_list_construction_other_scaled: i128,
    /// Raw address pairs summed over finalized transaction scopes.
    access_list_address_pairs: u64,
    /// Independent normalized raw address-pair contribution.
    access_list_address_scaled: i128,
    /// Actual accepted code preparations sealed with their contribution.
    code_preparation: CodePreparationSelectors,
    /// Normalized per-occurrence code preparation contribution.
    code_preparation_scaled: i128,
    /// Conservative journal comparison units sealed with their price.
    storage_journal_work_units: u64,
    /// Independent journal contribution in normalized fixed-point PGU.
    storage_journal_scaled: i128,
    /// Block operation contribution in fixed-point units.
    operations_scaled: i128,
    /// Normal dispatched opcode/confirmed-spawn count sealed with its reserve.
    normal_dispatch_count: u64,
    /// Additive reserve already included in `operations_scaled`.
    normal_dispatch_scaled: i128,
    /// Explicit feature-error contribution included in `operations_scaled`.
    feature_error_scaled: i128,
    /// Raw EVM-local resource subset sealed alongside its priced contribution.
    evm_local_resources: ResourceSelectors,
    /// EVM-local unique-resource contribution in fixed-point units.
    evm_local_resources_scaled: i128,
    /// Canonical committed signed-transaction payload selectors.
    transaction_payload: TransactionPayloadSelectors,
    /// Signed-transaction payload contribution in fixed-point units.
    transaction_payload_scaled: i128,
    /// Known block subtotal in fixed-point units, excluding exact block-end resources and MPT
    /// work.
    evm_local_known_subtotal_scaled: i128,
    /// Ceiling of the EVM-local subtotal, excluding exact block-end resources and MPT work.
    evm_local_known_subtotal_pgu: u128,
}

impl ZkGasKnownBlockBreakdown {
    /// Constructs the immutable report inside the sealed evaluator boundary.
    #[allow(clippy::too_many_arguments)] // Mirrors the independently checked report components.
    pub(super) const fn new(
        fixed_scaled: i128,
        authorization_validations: u64,
        authorization_scaled: i128,
        access_list_items: u64,
        access_list_scaled: i128,
        access_list_construction_pairs: u64,
        access_list_construction_scaled: i128,
        operations_scaled: i128,
        normal_dispatch_count: u64,
        normal_dispatch_scaled: i128,
        feature_error_scaled: i128,
        evm_local_resources: ResourceSelectors,
        evm_local_resources_scaled: i128,
        transaction_payload: TransactionPayloadSelectors,
        transaction_payload_scaled: i128,
        evm_local_known_subtotal_scaled: i128,
        evm_local_known_subtotal_pgu: u128,
        code_preparation: CodePreparationSelectors,
        code_preparation_scaled: i128,
        storage_journal_work_units: u64,
        storage_journal_scaled: i128,
        access_list_construction_other_pairs: u64,
        access_list_construction_other_scaled: i128,
        access_list_address_pairs: u64,
        access_list_address_scaled: i128,
    ) -> Self {
        Self {
            fixed_scaled,
            code_preparation,
            code_preparation_scaled,
            storage_journal_work_units,
            storage_journal_scaled,
            authorization_validations,
            authorization_scaled,
            access_list_items,
            access_list_scaled,
            access_list_construction_pairs,
            access_list_construction_scaled,
            access_list_construction_other_pairs,
            access_list_construction_other_scaled,
            access_list_address_pairs,
            access_list_address_scaled,
            operations_scaled,
            normal_dispatch_count,
            normal_dispatch_scaled,
            feature_error_scaled,
            evm_local_resources,
            evm_local_resources_scaled,
            transaction_payload,
            transaction_payload_scaled,
            evm_local_known_subtotal_scaled,
            evm_local_known_subtotal_pgu,
        }
    }

    /// Block fixed-cost contribution in fixed-point units.
    pub const fn fixed_scaled(self) -> i128 {
        self.fixed_scaled
    }

    /// Raw EIP-7702 authorization validations in the sealed block prefix.
    pub const fn authorization_validations(self) -> u64 {
        self.authorization_validations
    }

    /// Authorization validation contribution in normalized fixed-point PGU.
    pub const fn authorization_scaled(self) -> i128 {
        self.authorization_scaled
    }

    /// Raw non-legacy access-list address items, including duplicates and Anchor entries.
    pub const fn access_list_items(self) -> u64 {
        self.access_list_items
    }

    /// Raw access-list item contribution in normalized fixed-point PGU.
    pub const fn access_list_scaled(self) -> i128 {
        self.access_list_scaled
    }

    /// Raw transaction/owner/low32 occurrence pairs priced by this immutable report.
    pub const fn access_list_construction_pairs(self) -> u64 {
        self.access_list_construction_pairs
    }

    /// Additive construction supplement, independent of journals and operation-route maxima.
    pub const fn access_list_construction_scaled(self) -> i128 {
        self.access_list_construction_scaled
    }

    /// Raw same-transaction/owner occurrence pairs with different numeric low32 bits.
    pub const fn access_list_construction_other_pairs(self) -> u64 {
        self.access_list_construction_other_pairs
    }

    /// Independent normalized different-prefix construction contribution.
    pub const fn access_list_construction_other_scaled(self) -> i128 {
        self.access_list_construction_other_scaled
    }

    /// Raw address-item pairs finalized per transaction, including duplicates and empty keys.
    pub const fn access_list_address_pairs(self) -> u64 {
        self.access_list_address_pairs
    }

    /// Normalized address-pair reserve, charged once without another discount.
    pub const fn access_list_address_scaled(self) -> i128 {
        self.access_list_address_scaled
    }

    /// Actual accepted nonempty code preparations without hash deduplication.
    pub const fn code_preparation(self) -> CodePreparationSelectors {
        self.code_preparation
    }

    /// Accepted code preparation contribution in normalized fixed-point PGU.
    pub const fn code_preparation_scaled(self) -> i128 {
        self.code_preparation_scaled
    }

    /// Conservative journal comparison work across the included transaction scopes.
    pub const fn storage_journal_work_units(self) -> u64 {
        self.storage_journal_work_units
    }

    /// Additive journal reserve, included exactly once in the known subtotal.
    pub const fn storage_journal_scaled(self) -> i128 {
        self.storage_journal_scaled
    }

    /// Block operation contribution in fixed-point units.
    pub const fn operations_scaled(self) -> i128 {
        self.operations_scaled
    }

    /// Normally priced Opcode/ConfirmedSpawn events, including Anchor and child execution.
    pub const fn normal_dispatch_count(self) -> u64 {
        self.normal_dispatch_count
    }

    /// Additive dispatch reserve already included in `operations_scaled`; do not add it again.
    pub const fn normal_dispatch_scaled(self) -> i128 {
        self.normal_dispatch_scaled
    }

    /// Explicit feature-error attribution included in the operation contribution.
    pub const fn feature_error_scaled(self) -> i128 {
        self.feature_error_scaled
    }

    /// Raw EVM-local resource subset observed by runtime hooks.
    pub const fn evm_local_resources(self) -> ResourceSelectors {
        self.evm_local_resources
    }

    /// EVM-local unique-resource contribution in fixed-point units.
    pub const fn evm_local_resources_scaled(self) -> i128 {
        self.evm_local_resources_scaled
    }

    /// Canonical committed signed-transaction payload selectors.
    pub const fn transaction_payload(self) -> TransactionPayloadSelectors {
        self.transaction_payload
    }

    /// Signed-transaction payload contribution in fixed-point units.
    pub const fn transaction_payload_scaled(self) -> i128 {
        self.transaction_payload_scaled
    }

    /// Known block subtotal excluding exact block-end resources and MPT work.
    pub const fn evm_local_known_subtotal_scaled(self) -> i128 {
        self.evm_local_known_subtotal_scaled
    }

    /// Ceiling of the known block subtotal, including payload and excluding exact state/MPT.
    pub const fn evm_local_known_subtotal_pgu(self) -> u128 {
        self.evm_local_known_subtotal_pgu
    }

    #[cfg(test)]
    pub(super) const fn with_storage_journal_for_test(mut self, units: u64, scaled: i128) -> Self {
        self.storage_journal_work_units = units;
        self.storage_journal_scaled = scaled;
        self
    }

    #[cfg(test)]
    pub(super) const fn with_normal_dispatch_for_test(mut self, count: u64, scaled: i128) -> Self {
        self.normal_dispatch_count = count;
        self.normal_dispatch_scaled = scaled;
        self
    }

    #[cfg(test)]
    pub(in crate::zk_gas) const fn with_payload_for_test(
        mut self,
        payload: TransactionPayloadSelectors,
        scaled: i128,
    ) -> Self {
        self.transaction_payload = payload;
        self.transaction_payload_scaled = scaled;
        self
    }

    #[cfg(test)]
    pub(in crate::zk_gas) const fn from_parts_for_test(
        fixed_scaled: i128,
        operations_scaled: i128,
        feature_error_scaled: i128,
        evm_local_resources: ResourceSelectors,
        resources_scaled: i128,
        known_subtotal_scaled: i128,
        known_subtotal_pgu: u128,
    ) -> Self {
        Self::new(
            fixed_scaled,
            0,
            0,
            0,
            0,
            0,
            0,
            operations_scaled,
            0,
            0,
            feature_error_scaled,
            evm_local_resources,
            resources_scaled,
            TransactionPayloadSelectors {
                canonical_transaction_bytes: 0,
                nonempty_payload_transactions: 0,
            },
            0,
            known_subtotal_scaled,
            known_subtotal_pgu,
            CodePreparationSelectors { nonempty_events: 0, total_bytes: 0 },
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        )
    }
}

/// Runtime collection mode for the accepted zk gas pricing model.
///
/// Admission is intentionally absent: the production state-root pipeline does not yet expose the
/// exact absolute candidate-prefix MPT selector snapshot required for a complete quote. That
/// snapshot describes the canonically aggregated post-state and replaces the prior prefix snapshot;
/// it is not an additive per-transaction delta.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ZkGasMode {
    /// Do not collect or evaluate pricing selectors.
    #[default]
    Disabled,
    /// Collect execution/resource selectors and publish a non-enforcing diagnostic report.
    Shadow,
}

impl ZkGasMode {
    /// Modes accepted by runtime configuration.
    pub const ALL: [Self; 2] = [Self::Disabled, Self::Shadow];
}

/// Exact selector group that prevents a shadow subtotal from being a complete block quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZkGasIncompleteness {
    /// Exact block-end resources and the absolute block/prefix MPT snapshot are unavailable.
    ExactBlockResourcesAndMptSelectorsUnavailable,
}

/// Typed failure that prevents publication of even the known-component subtotal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZkGasShadowDiagnostic {
    /// Shadow mode was requested but no selector collector was installed.
    CollectorUnavailable,
    /// Selector collection retained a non-fatal diagnostic from included execution.
    Collector(ZkGasAccumulatorError),
    /// Required journal observations were incomplete; partial work cannot produce a valid quote.
    StorageJournal(StorageJournalIncompleteness),
    /// The pure model rejected the committed execution/resource selectors.
    Pricing(ZkGasError),
}

/// Typed failure while completing a Shadow report with exact external block resources and MPT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZkGasCompletionError {
    /// The report did not identify the sealed runtime model.
    ModelIdentity,
    /// The report named an incompleteness that this completion input cannot satisfy.
    UnexpectedZkGasIncompleteness(ZkGasIncompleteness),
    /// The sealed known-component fields were internally inconsistent.
    InconsistentKnownBreakdown(&'static str),
    /// An alleged exact resource dimension was below its sealed EVM-local subset.
    ExactResourcesBelowEvmLocal(&'static str),
    /// Shadow execution reported a collector or pricing diagnostic.
    ZkGasShadowDiagnostic(ZkGasShadowDiagnostic),
    /// Checked model validation or arithmetic rejected the exact completion input.
    Pricing(ZkGasError),
}

impl From<ZkGasError> for ZkGasCompletionError {
    /// Wraps an exact-completion pricing failure without losing its typed cause.
    fn from(error: ZkGasError) -> Self {
        Self::Pricing(error)
    }
}

impl fmt::Display for ZkGasCompletionError {
    /// Formats a stable description of why an incomplete Shadow report could not be completed.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelIdentity => {
                formatter.write_str("zk gas pricing runtime model identity differs")
            }
            Self::UnexpectedZkGasIncompleteness(missing) => {
                write!(formatter, "unexpected zk gas pricing incompleteness: {missing:?}")
            }
            Self::InconsistentKnownBreakdown(field) => {
                write!(formatter, "inconsistent zk gas pricing known breakdown: {field}")
            }
            Self::ExactResourcesBelowEvmLocal(field) => {
                write!(formatter, "exact zk gas pricing resource is below EVM-local {field}")
            }
            Self::ZkGasShadowDiagnostic(diagnostic) => {
                write!(formatter, "zk gas pricing shadow diagnostic: {diagnostic:?}")
            }
            Self::Pricing(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ZkGasCompletionError {}

/// Status of one block-local shadow evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    clippy::large_enum_variant,
    reason = "Keep sealed block reports Copy without introducing per-block heap allocation or a public API break"
)]
pub enum ZkGasShadowStatus {
    /// Execution, signed payload and local resources were priced, but the result is incomplete.
    Incomplete {
        /// Non-enforcing fixed, operation, signed payload and EVM-local resource subtotal.
        known: ZkGasKnownBlockBreakdown,
        /// Exact selector group that remains unavailable.
        missing: ZkGasIncompleteness,
    },
    /// A typed collector or pricing diagnostic prevented a trustworthy subtotal.
    Diagnostic(ZkGasShadowDiagnostic),
}

impl ZkGasShadowStatus {
    /// Returns a bounded-cardinality label suitable for logs and metrics.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Incomplete { .. } => "incomplete_missing_block_resources_and_mpt",
            Self::Diagnostic(ZkGasShadowDiagnostic::CollectorUnavailable) => {
                "collector_unavailable"
            }
            Self::Diagnostic(ZkGasShadowDiagnostic::Collector(ZkGasAccumulatorError::Pricing(
                ZkGasError::UnknownRoute(_),
            ))) => "collector_unknown_route",
            Self::Diagnostic(ZkGasShadowDiagnostic::Collector(
                ZkGasAccumulatorError::NoActiveTransaction,
            )) => "collector_no_active_transaction",
            Self::Diagnostic(ZkGasShadowDiagnostic::Collector(_)) => "collector_error",
            Self::Diagnostic(ZkGasShadowDiagnostic::StorageJournal(_)) => {
                "storage_journal_incomplete"
            }
            Self::Diagnostic(ZkGasShadowDiagnostic::Pricing(ZkGasError::UnknownRoute(_))) => {
                "pricing_unknown_route"
            }
            Self::Diagnostic(ZkGasShadowDiagnostic::Pricing(_)) => "pricing_error",
        }
    }
}

/// Block-local non-enforcing output published by Shadow mode.
///
/// External callers can inspect reports but cannot forge one from public fields.
///
/// ```compile_fail
/// use alethia_reth_evm::zk_gas::pricing::{RUNTIME_SPEC_SHA256, ZkGasShadowDiagnostic,
///     ZkGasShadowReport, ZkGasShadowStatus};
/// let _ = ZkGasShadowReport {
///     model_id: RUNTIME_SPEC_SHA256,
///     status: ZkGasShadowStatus::Diagnostic(ZkGasShadowDiagnostic::CollectorUnavailable),
/// };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZkGasShadowReport {
    /// Content-addressed runtime model identifier.
    model_id: &'static str,
    /// Incomplete known-component subtotal or a typed diagnostic.
    status: ZkGasShadowStatus,
    /// Journal observation whose complete comparison units contribute to the known subtotal.
    storage_journal: StorageJournalDiagnostics,
    /// Independent transient diagnostics whose units join the same sealed journal price once.
    transient_storage_journal: TransientStorageJournalDiagnostics,
    /// Raw access-list construction receipt diagnostics, separate from physical journal work.
    access_list_construction: AccessListConstructionDiagnostics,
}

impl ZkGasShadowReport {
    #[cfg(test)]
    /// Constructs a synthetic report for independent model-completion failure tests.
    pub(super) const fn from_status(status: ZkGasShadowStatus) -> Self {
        Self {
            model_id: RUNTIME_SPEC_SHA256,
            status,
            storage_journal: StorageJournalDiagnostics::EMPTY,
            transient_storage_journal: TransientStorageJournalDiagnostics::EMPTY,
            access_list_construction: AccessListConstructionDiagnostics::EMPTY,
        }
    }

    /// Returns the content-addressed runtime model identifier.
    pub const fn model_id(self) -> &'static str {
        self.model_id
    }

    /// Returns the typed non-enforcing report status.
    pub const fn status(self) -> ZkGasShadowStatus {
        self.status
    }

    /// Returns potential storage-map work; incomplete observations prevent a valid known subtotal.
    pub const fn storage_journal(self) -> StorageJournalDiagnostics {
        self.storage_journal
    }

    /// Returns transient-only observations, separate from ordinary SLOAD/SSTORE diagnostics.
    pub const fn transient_storage_journal(self) -> TransientStorageJournalDiagnostics {
        self.transient_storage_journal
    }

    /// Raw-prefix construction diagnostics, including a retained failure if capture was incomplete.
    pub const fn access_list_construction(self) -> AccessListConstructionDiagnostics {
        self.access_list_construction
    }

    /// Returns a typed report for an unexpectedly missing Shadow collector.
    pub const fn collector_unavailable() -> Self {
        Self {
            model_id: RUNTIME_SPEC_SHA256,
            status: ZkGasShadowStatus::Diagnostic(ZkGasShadowDiagnostic::CollectorUnavailable),
            storage_journal: StorageJournalDiagnostics {
                incomplete: Some(StorageJournalIncompleteness::CollectorUnavailable),
                ..StorageJournalDiagnostics::EMPTY
            },
            transient_storage_journal: TransientStorageJournalDiagnostics {
                incomplete: Some(StorageJournalIncompleteness::CollectorUnavailable),
                ..TransientStorageJournalDiagnostics::EMPTY
            },
            access_list_construction: AccessListConstructionDiagnostics {
                incomplete: Some(AccessListConstructionIncompleteness::MissingCapture),
                ..AccessListConstructionDiagnostics::EMPTY
            },
        }
    }

    #[cfg(test)]
    pub(in crate::zk_gas) const fn from_parts_for_test(
        model_id: &'static str,
        status: ZkGasShadowStatus,
    ) -> Self {
        Self {
            model_id,
            status,
            storage_journal: StorageJournalDiagnostics::EMPTY,
            transient_storage_journal: TransientStorageJournalDiagnostics::EMPTY,
            access_list_construction: AccessListConstructionDiagnostics::EMPTY,
        }
    }
}

impl ZkGasBlockSnapshot {
    /// Evaluates the committed execution, signed payload and resource prefix for Shadow mode.
    ///
    /// This never claims to be a complete block quote. Exact MPT selectors are unavailable, so a
    /// successful evaluation is explicitly tagged as incomplete and cannot drive admission.
    pub fn shadow_report(&self) -> ZkGasShadowReport {
        let status = match self.diagnostic {
            Some(error) => ZkGasShadowStatus::Diagnostic(ZkGasShadowDiagnostic::Collector(error)),
            None if self.storage_journal.incomplete.is_some() => ZkGasShadowStatus::Diagnostic(
                ZkGasShadowDiagnostic::StorageJournal(self.storage_journal.incomplete.unwrap()),
            ),
            None if self.transient_storage_journal.incomplete.is_some() => {
                ZkGasShadowStatus::Diagnostic(ZkGasShadowDiagnostic::StorageJournal(
                    self.transient_storage_journal.incomplete.unwrap(),
                ))
            }
            None => match self.storage_journal_work_units().and_then(|storage_journal_work_units| {
                let access_list_construction_pairs = self.access_list_construction_pairs()?;
                let access_list_construction_other_pairs =
                    self.access_list_construction_other_pairs()?;
                ZkGasModel.price_block_without_mpt(&KnownBlockPricingInput {
                    started_non_anchor_transactions: self.started_non_anchor_transactions,
                    native_value_transfers: self.native_value_transfers,
                    authorization_validations: self.authorization_validations,
                    access_list_items: self.access_list_items,
                    access_list_construction_pairs,
                    access_list_construction_other_pairs,
                    access_list_address_pairs: self.access_list_address_pairs()?,
                    code_preparation: self.code_preparation,
                    storage_journal_work_units,
                    operation_routes: &self.operation_routes,
                    resources: self.resources,
                    transaction_payload: self.transaction_payload,
                })
            }) {
                Ok(known) => ZkGasShadowStatus::Incomplete {
                    known,
                    missing: ZkGasIncompleteness::ExactBlockResourcesAndMptSelectorsUnavailable,
                },
                Err(error) => ZkGasShadowStatus::Diagnostic(ZkGasShadowDiagnostic::Pricing(error)),
            },
        };
        ZkGasShadowReport {
            model_id: RUNTIME_SPEC_SHA256,
            status,
            storage_journal: self.storage_journal,
            transient_storage_journal: self.transient_storage_journal,
            access_list_construction: self.access_list_construction,
        }
    }
}
