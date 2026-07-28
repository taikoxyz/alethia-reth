/// Pruning errors and per-run output records.
mod error;
pub use error::{OpProofStoragePrunerResult, PrunerError, PrunerOutput};

/// Proof-window pruning algorithm and batch orchestration.
mod pruner;
pub use pruner::OpProofStoragePruner;

#[cfg(feature = "metrics")]
/// Metrics emitted for completed pruning passes.
mod metrics;

/// Periodic asynchronous driver for the proof-history pruner.
mod task;
pub use task::OpProofStoragePrunerTask;
