//! Window-based pruner for the proofs sidecar storage.

mod error;
pub use error::{ProofsStoragePrunerResult, PrunerError, PrunerOutput};

mod pruner;
pub use pruner::ProofsStoragePruner;

mod metrics;

mod task;
pub use task::ProofsStoragePrunerTask;
