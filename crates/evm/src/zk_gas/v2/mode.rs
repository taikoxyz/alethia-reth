//! Runtime mode and typed shadow-report contracts for zk gas V2.

use super::{
    RUNTIME_SPEC_SHA256, ZkGasV2AccumulatorError, ZkGasV2BlockSnapshot, ZkGasV2Error, ZkGasV2Model,
    model::KnownBlockPricingInput,
};
use core::fmt;

/// Priced block components that are observable without the production MPT interface.
///
/// There is deliberately no MPT field and no complete-block total in this type. The subtotal is
/// useful for Shadow analysis only and cannot be interpreted as an MPT measurement of zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZkGasV2KnownBlockBreakdown {
    /// Block fixed-cost contribution in fixed-point units.
    pub fixed_scaled: i128,
    /// Block operation contribution in fixed-point units.
    pub operations_scaled: i128,
    /// Explicit feature-error contribution included in `operations_scaled`.
    pub feature_error_scaled: i128,
    /// Block unique-resource contribution in fixed-point units.
    pub resources_scaled: i128,
    /// Known-component subtotal in fixed-point units, excluding all MPT work.
    pub known_subtotal_scaled: i128,
    /// Ceiling of the known-component subtotal in PGU, excluding all MPT work.
    pub known_subtotal_pgu: u128,
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
    /// The exact absolute block/prefix MPT snapshot is unavailable from the state-root pipeline.
    ExactMptSelectorsUnavailable,
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

/// Typed failure while completing a Shadow report with an exact external MPT snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZkGasV2CompletionError {
    /// The report did not identify the sealed runtime model.
    ModelIdentity,
    /// The report named an incompleteness that this completion input cannot satisfy.
    UnexpectedIncompleteness(ZkGasV2Incompleteness),
    /// The public known-component fields were internally inconsistent.
    InconsistentKnownBreakdown(&'static str),
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
    /// Known execution/resource components were priced, but the result is not a complete quote.
    Incomplete {
        /// Non-enforcing subtotal containing fixed, operation, and resource components only.
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
            Self::Incomplete { .. } => "incomplete_missing_mpt",
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZkGasV2ShadowReport {
    /// Content-addressed runtime model identifier.
    pub model_id: &'static str,
    /// Incomplete known-component subtotal or a typed diagnostic.
    pub status: ZkGasV2ShadowStatus,
}

impl ZkGasV2ShadowReport {
    /// Returns a typed report for an unexpectedly missing Shadow collector.
    pub const fn collector_unavailable() -> Self {
        Self {
            model_id: RUNTIME_SPEC_SHA256,
            status: ZkGasV2ShadowStatus::Diagnostic(ZkGasV2ShadowDiagnostic::CollectorUnavailable),
        }
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
                    missing: ZkGasV2Incompleteness::ExactMptSelectorsUnavailable,
                },
                Err(error) => {
                    ZkGasV2ShadowStatus::Diagnostic(ZkGasV2ShadowDiagnostic::Pricing(error))
                }
            },
        };
        ZkGasV2ShadowReport { model_id: RUNTIME_SPEC_SHA256, status }
    }
}
