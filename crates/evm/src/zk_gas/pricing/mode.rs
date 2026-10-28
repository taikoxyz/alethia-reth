//! Runtime mode and typed shadow-report contracts for zk gas pricing.

use super::{
    RUNTIME_SPEC_SHA256, ResourceSelectors, TransactionPayloadSelectors, ZkGasAccumulatorError,
    ZkGasBlockSnapshot, ZkGasError, ZkGasModel, model::KnownBlockPricingInput,
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
    /// Block operation contribution in fixed-point units.
    operations_scaled: i128,
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
        operations_scaled: i128,
        feature_error_scaled: i128,
        evm_local_resources: ResourceSelectors,
        evm_local_resources_scaled: i128,
        transaction_payload: TransactionPayloadSelectors,
        transaction_payload_scaled: i128,
        evm_local_known_subtotal_scaled: i128,
        evm_local_known_subtotal_pgu: u128,
    ) -> Self {
        Self {
            fixed_scaled,
            operations_scaled,
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

    /// Block operation contribution in fixed-point units.
    pub const fn operations_scaled(self) -> i128 {
        self.operations_scaled
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
            operations_scaled,
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
}

impl ZkGasShadowReport {
    /// Constructs one immutable report inside the sealed evaluator boundary.
    pub(super) const fn from_status(status: ZkGasShadowStatus) -> Self {
        Self { model_id: RUNTIME_SPEC_SHA256, status }
    }

    /// Returns the content-addressed runtime model identifier.
    pub const fn model_id(self) -> &'static str {
        self.model_id
    }

    /// Returns the typed non-enforcing report status.
    pub const fn status(self) -> ZkGasShadowStatus {
        self.status
    }

    /// Returns a typed report for an unexpectedly missing Shadow collector.
    pub const fn collector_unavailable() -> Self {
        Self {
            model_id: RUNTIME_SPEC_SHA256,
            status: ZkGasShadowStatus::Diagnostic(ZkGasShadowDiagnostic::CollectorUnavailable),
        }
    }

    #[cfg(test)]
    pub(in crate::zk_gas) const fn from_parts_for_test(
        model_id: &'static str,
        status: ZkGasShadowStatus,
    ) -> Self {
        Self { model_id, status }
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
            None => match ZkGasModel.price_block_without_mpt(&KnownBlockPricingInput {
                started_non_anchor_transactions: self.started_non_anchor_transactions,
                native_value_transfers: self.native_value_transfers,
                operation_routes: &self.operation_routes,
                resources: self.resources,
                transaction_payload: self.transaction_payload,
            }) {
                Ok(known) => ZkGasShadowStatus::Incomplete {
                    known,
                    missing: ZkGasIncompleteness::ExactBlockResourcesAndMptSelectorsUnavailable,
                },
                Err(error) => ZkGasShadowStatus::Diagnostic(ZkGasShadowDiagnostic::Pricing(error)),
            },
        };
        ZkGasShadowReport::from_status(status)
    }
}
