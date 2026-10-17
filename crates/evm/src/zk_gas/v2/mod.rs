//! Pure fixed-point evaluator for the accepted schema-9 zk gas model.

mod generated;
mod model;
mod types;

pub use generated::{FIXED_POINT_SCALE, RUNTIME_SPEC_SHA256};
pub use model::ZkGasV2Model;
pub use types::{
    AccessClass, BlockPricingInput, CalldataAccessClass, MemorySelectors, MptSelectors,
    OperationModelInput, OperationPricingInput, OperationRoute, OperationRoutePricingInput,
    PricingBreakdown, ProposalPricingInput, ResourceSelectors, StorageBranch, ValueClass,
    ZkGasV2Error,
};
