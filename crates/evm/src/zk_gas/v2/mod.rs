//! Pure fixed-point evaluator for the accepted schema-9 zk gas model.

mod accumulator;
mod generated;
mod mode;
mod model;
mod types;

pub use accumulator::{
    StateAccess, TransactionMetadata, ZkGasV2Accumulator, ZkGasV2AccumulatorError,
    ZkGasV2BlockSnapshot,
};
pub use generated::{FIXED_POINT_SCALE, RUNTIME_SPEC_SHA256};
pub use mode::{
    ZkGasV2CompletionError, ZkGasV2Incompleteness, ZkGasV2KnownBlockBreakdown, ZkGasV2Mode,
    ZkGasV2ShadowDiagnostic, ZkGasV2ShadowReport, ZkGasV2ShadowStatus,
};
pub use model::ZkGasV2Model;
pub use types::{
    AccessClass, BlockPricingInput, CalldataAccessClass, MemorySelectors, MptSelectors,
    OperationModelInput, OperationPricingInput, OperationRoute, OperationRoutePricingInput,
    PricingBreakdown, ProposalPricingInput, ResourceSelectors, StorageBranch, ValueClass,
    ZkGasV2Error,
};
