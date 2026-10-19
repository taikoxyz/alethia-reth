//! Pure fixed-point evaluator for the accepted schema-9 zk gas model.

mod accumulator;
mod generated;
mod mode;
mod model;
mod types;

pub use accumulator::{
    StateAccess, TransactionMetadata, ZkGasAccumulator, ZkGasAccumulatorError, ZkGasBlockSnapshot,
};
pub use generated::{FIXED_POINT_SCALE, RUNTIME_SPEC_SHA256};
pub use mode::{
    ZkGasCompletionError, ZkGasIncompleteness, ZkGasKnownBlockBreakdown, ZkGasMode,
    ZkGasShadowDiagnostic, ZkGasShadowReport, ZkGasShadowStatus,
};
pub use model::ZkGasModel;
pub use types::{
    AccessClass, BlockPricingInput, CalldataAccessClass, MemorySelectors, MptSelectors,
    OperationModelInput, OperationPricingInput, OperationRoute, OperationRoutePricingInput,
    PricingBreakdown, ProposalPricingInput, ResourceSelectors, StorageBranch, ValueClass,
    ZkGasError,
};
