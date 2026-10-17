//! Runtime mode and typed shadow-report contracts for zk gas V2.

use super::{
    RUNTIME_SPEC_SHA256, ResourceSelectors, ZkGasV2AccumulatorError, ZkGasV2BlockSnapshot,
    ZkGasV2Error, ZkGasV2Model, model::KnownBlockPricingInput,
};
use core::fmt;

/// Priced block components observable from EVM-local hooks before block-end reconciliation.
///
/// The resource term is only the EVM-local subset: pre-block system calls and validation can load
/// additional state directly into the block database cache. There is deliberately no exact
/// block-end resource term, MPT field, or complete-block total in this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZkGasV2KnownBlockBreakdown {
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
    /// EVM-local subtotal in fixed-point units, excluding exact block-end resources and MPT work.
    evm_local_known_subtotal_scaled: i128,
    /// Ceiling of the EVM-local subtotal, excluding exact block-end resources and MPT work.
    evm_local_known_subtotal_pgu: u128,
}

impl ZkGasV2KnownBlockBreakdown {
    /// Constructs the immutable report inside the sealed evaluator boundary.
    pub(super) const fn new(
        fixed_scaled: i128,
        operations_scaled: i128,
        feature_error_scaled: i128,
        evm_local_resources: ResourceSelectors,
        evm_local_resources_scaled: i128,
        evm_local_known_subtotal_scaled: i128,
        evm_local_known_subtotal_pgu: u128,
    ) -> Self {
        Self {
            fixed_scaled,
            operations_scaled,
            feature_error_scaled,
            evm_local_resources,
            evm_local_resources_scaled,
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

    /// EVM-local subtotal excluding exact block-end resources and MPT work.
    pub const fn evm_local_known_subtotal_scaled(self) -> i128 {
        self.evm_local_known_subtotal_scaled
    }

    /// Ceiling of the EVM-local subtotal excluding exact block-end resources and MPT work.
    pub const fn evm_local_known_subtotal_pgu(self) -> u128 {
        self.evm_local_known_subtotal_pgu
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
            known_subtotal_scaled,
            known_subtotal_pgu,
        )
    }
}

/// Runtime collection mode for the accepted zk gas V2 model.
///
/// Admission is intentionally absent: the production state-root pipeline does not yet expose the
/// exact absolute candidate-prefix MPT selector snapshot required for a complete quote. That
/// snapshot describes the canonically aggregated post-state and replaces the prior prefix snapshot;
/// it is not an additive per-transaction delta.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ZkGasV2Mode {
    /// Do not collect or price V2 selectors.
    #[default]
    Disabled,
    /// Collect execution/resource selectors and publish a non-enforcing diagnostic report.
    Shadow,
}

impl ZkGasV2Mode {
    /// Modes accepted by runtime configuration.
    pub const ALL: [Self; 2] = [Self::Disabled, Self::Shadow];
}

/// Exact selector group that prevents a shadow subtotal from being a complete block quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZkGasV2Incompleteness {
    /// Exact block-end resources and the absolute block/prefix MPT snapshot are unavailable.
    ExactBlockResourcesAndMptSelectorsUnavailable,
}

/// Typed failure that prevents publication of even the known-component subtotal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZkGasV2ShadowDiagnostic {
    /// Shadow mode was requested but no selector collector was installed.
    CollectorUnavailable,
    /// Selector collection retained a non-fatal diagnostic from included execution.
    Collector(ZkGasV2AccumulatorError),
    /// The pure model rejected the committed execution/resource selectors.
    Pricing(ZkGasV2Error),
}

/// Typed failure while completing a Shadow report with exact external block resources and MPT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZkGasV2CompletionError {
    /// The report did not identify the sealed runtime model.
    ModelIdentity,
    /// The report named an incompleteness that this completion input cannot satisfy.
    UnexpectedIncompleteness(ZkGasV2Incompleteness),
    /// The sealed known-component fields were internally inconsistent.
    InconsistentKnownBreakdown(&'static str),
    /// An alleged exact resource dimension was below its sealed EVM-local subset.
    ExactResourcesBelowEvmLocal(&'static str),
    /// Shadow execution reported a collector or pricing diagnostic.
    ShadowDiagnostic(ZkGasV2ShadowDiagnostic),
    /// Checked model validation or arithmetic rejected the exact completion input.
    Pricing(ZkGasV2Error),
}

impl From<ZkGasV2Error> for ZkGasV2CompletionError {
    fn from(error: ZkGasV2Error) -> Self {
        Self::Pricing(error)
    }
}

impl fmt::Display for ZkGasV2CompletionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelIdentity => formatter.write_str("zk gas V2 runtime model identity differs"),
            Self::UnexpectedIncompleteness(missing) => {
                write!(formatter, "unexpected zk gas V2 incompleteness: {missing:?}")
            }
            Self::InconsistentKnownBreakdown(field) => {
                write!(formatter, "inconsistent zk gas V2 known breakdown: {field}")
            }
            Self::ExactResourcesBelowEvmLocal(field) => {
                write!(formatter, "exact zk gas V2 resource is below EVM-local {field}")
            }
            Self::ShadowDiagnostic(diagnostic) => {
                write!(formatter, "zk gas V2 shadow diagnostic: {diagnostic:?}")
            }
            Self::Pricing(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ZkGasV2CompletionError {}

/// Status of one block-local shadow evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZkGasV2ShadowStatus {
    /// EVM-local execution/resource components were priced, but the result is incomplete.
    Incomplete {
        /// Non-enforcing fixed, operation, and EVM-local resource subtotal only.
        known: ZkGasV2KnownBlockBreakdown,
        /// Exact selector group that remains unavailable.
        missing: ZkGasV2Incompleteness,
    },
    /// A typed collector or pricing diagnostic prevented a trustworthy subtotal.
    Diagnostic(ZkGasV2ShadowDiagnostic),
}

impl ZkGasV2ShadowStatus {
    /// Returns a bounded-cardinality label suitable for logs and metrics.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Incomplete { .. } => "incomplete_missing_block_resources_and_mpt",
            Self::Diagnostic(ZkGasV2ShadowDiagnostic::CollectorUnavailable) => {
                "collector_unavailable"
            }
            Self::Diagnostic(ZkGasV2ShadowDiagnostic::Collector(
                ZkGasV2AccumulatorError::Pricing(ZkGasV2Error::UnknownRoute(_)),
            )) => "collector_unknown_route",
            Self::Diagnostic(ZkGasV2ShadowDiagnostic::Collector(_)) => "collector_error",
            Self::Diagnostic(ZkGasV2ShadowDiagnostic::Pricing(ZkGasV2Error::UnknownRoute(_))) => {
                "pricing_unknown_route"
            }
            Self::Diagnostic(ZkGasV2ShadowDiagnostic::Pricing(_)) => "pricing_error",
        }
    }
}

/// Block-local non-enforcing output published by Shadow mode.
///
/// External callers can inspect reports but cannot forge one from public fields.
///
/// ```compile_fail
/// use alethia_reth_evm::zk_gas::v2::{RUNTIME_SPEC_SHA256, ZkGasV2ShadowDiagnostic,
///     ZkGasV2ShadowReport, ZkGasV2ShadowStatus};
/// let _ = ZkGasV2ShadowReport {
///     model_id: RUNTIME_SPEC_SHA256,
///     status: ZkGasV2ShadowStatus::Diagnostic(ZkGasV2ShadowDiagnostic::CollectorUnavailable),
/// };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZkGasV2ShadowReport {
    /// Content-addressed runtime model identifier.
    model_id: &'static str,
    /// Incomplete known-component subtotal or a typed diagnostic.
    status: ZkGasV2ShadowStatus,
}

impl ZkGasV2ShadowReport {
    /// Constructs one immutable report inside the sealed evaluator boundary.
    pub(super) const fn from_status(status: ZkGasV2ShadowStatus) -> Self {
        Self {
            model_id: RUNTIME_SPEC_SHA256,
            status,
        }
    }

    /// Returns the content-addressed runtime model identifier.
    pub const fn model_id(self) -> &'static str {
        self.model_id
    }

    /// Returns the typed non-enforcing report status.
    pub const fn status(self) -> ZkGasV2ShadowStatus {
        self.status
    }

    /// Returns a typed report for an unexpectedly missing Shadow collector.
    pub const fn collector_unavailable() -> Self {
        Self {
            model_id: RUNTIME_SPEC_SHA256,
            status: ZkGasV2ShadowStatus::Diagnostic(ZkGasV2ShadowDiagnostic::CollectorUnavailable),
        }
    }

    #[cfg(test)]
    pub(in crate::zk_gas) const fn from_parts_for_test(
        model_id: &'static str,
        status: ZkGasV2ShadowStatus,
    ) -> Self {
        Self { model_id, status }
    }
}

impl ZkGasV2BlockSnapshot {
    /// Evaluates the committed execution/resource prefix for Shadow mode.
    ///
    /// This never claims to be a complete block quote. Exact MPT selectors are unavailable, so a
    /// successful evaluation is explicitly tagged as incomplete and cannot drive admission.
    pub fn shadow_report(&self) -> ZkGasV2ShadowReport {
        let status = match self.diagnostic {
            Some(error) => {
                ZkGasV2ShadowStatus::Diagnostic(ZkGasV2ShadowDiagnostic::Collector(error))
            }
            None => match ZkGasV2Model.price_block_without_mpt(&KnownBlockPricingInput {
                started_non_anchor_transactions: self.started_non_anchor_transactions,
                native_value_transfers: self.native_value_transfers,
                operation_routes: &self.operation_routes,
                resources: self.resources,
            }) {
                Ok(known) => ZkGasV2ShadowStatus::Incomplete {
                    known,
                    missing:
                        ZkGasV2Incompleteness::ExactBlockResourcesAndMptSelectorsUnavailable,
                },
                Err(error) => {
                    ZkGasV2ShadowStatus::Diagnostic(ZkGasV2ShadowDiagnostic::Pricing(error))
                }
            },
        };
        ZkGasV2ShadowReport::from_status(status)
    }
}
