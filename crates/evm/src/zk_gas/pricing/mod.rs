//! Pure fixed-point evaluator for the accepted schema-9 zk gas model.

mod access_list_construction;
mod account_cache;
mod account_journal;
mod accumulator;
mod generated;
mod mode;
mod model;
mod storage_journal;
mod storage_transition;
mod transient_journal;
mod types;

pub use access_list_construction::{
    AccessListConstructionDiagnostics, AccessListConstructionIncompleteness,
};
pub use account_cache::{AccountCacheDiagnostics, AccountCacheIncompleteness};
pub use account_journal::{AccountJournalDiagnostics, AccountJournalIncompleteness};
pub use accumulator::{
    StateAccess, TransactionMetadata, ZkGasAccumulator, ZkGasAccumulatorError,
    ZkGasAttemptedBlockSnapshot, ZkGasBlockSnapshot,
};
pub use generated::{FIXED_POINT_SCALE, RUNTIME_SPEC_SHA256};
pub use mode::{
    ZkGasCompletionError, ZkGasIncompleteness, ZkGasKnownBlockBreakdown, ZkGasMode,
    ZkGasShadowDiagnostic, ZkGasShadowReport, ZkGasShadowStatus,
};
pub use model::ZkGasModel;
pub use storage_journal::{StorageJournalDiagnostics, StorageJournalIncompleteness};
pub use storage_transition::{StorageTransitionDiagnostics, StorageTransitionIncompleteness};
pub use transient_journal::TransientStorageJournalDiagnostics;
pub use types::{
    AccessClass, BlockPricingInput, CalldataAccessClass, CodePreparationSelectors, MemorySelectors,
    MptSelectors, OperationModelInput, OperationPricingInput, OperationRoute,
    OperationRoutePricingInput, PricingBreakdown, ProposalPricingInput, ResourceSelectors,
    StorageBranch, TransactionPayloadSelectors, ValueClass, ZkGasError,
};
