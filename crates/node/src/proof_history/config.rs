//! Proof-history configuration.

use eyre::eyre;
use std::{path::PathBuf, time::Duration};

/// Default proof-history retention window in blocks.
const DEFAULT_PROOF_HISTORY_WINDOW: u64 = 1_296_000;

/// Default interval between proof-history prune passes.
const DEFAULT_PROOF_HISTORY_PRUNE_INTERVAL: Duration = Duration::from_secs(15);

/// Default interval, in blocks, between proof-history consistency checks.
const DEFAULT_PROOF_HISTORY_VERIFICATION_INTERVAL: u64 = 1024;

/// Configuration for the optional proof-history execution extension.
#[derive(Debug, Clone)]
pub struct ProofHistoryConfig {
    /// Whether proof-history indexing is installed on the node.
    pub enabled: bool,
    /// Filesystem path for the MDBX proof-history database.
    pub storage_path: Option<PathBuf>,
    /// Number of recent blocks retained in proof-history storage.
    pub window: u64,
    /// Whether empty proof-history storage waits until the finalized retention window starts.
    pub backfill_window_only: bool,
    /// Wall-clock interval between proof-history prune passes.
    pub prune_interval: Duration,
    /// Block interval between proof-history consistency checks; zero disables verification.
    pub verification_interval: u64,
}

impl ProofHistoryConfig {
    /// Returns a disabled proof-history configuration with production defaults.
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            storage_path: None,
            window: DEFAULT_PROOF_HISTORY_WINDOW,
            backfill_window_only: false,
            prune_interval: DEFAULT_PROOF_HISTORY_PRUNE_INTERVAL,
            verification_interval: DEFAULT_PROOF_HISTORY_VERIFICATION_INTERVAL,
        }
    }

    /// Returns the configured storage path or an error if proof history requires one.
    pub fn required_storage_path(&self) -> eyre::Result<&PathBuf> {
        self.storage_path.as_ref().ok_or_else(|| {
            eyre!("proof-history storage path is required when proof history is enabled")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_config_has_no_storage_path() {
        let config = ProofHistoryConfig::disabled();

        assert!(!config.enabled);
        assert!(!config.backfill_window_only);
        assert!(config.storage_path.is_none());
        assert!(config.required_storage_path().is_err());
    }

    #[test]
    fn enabled_config_requires_storage_path() {
        let config = ProofHistoryConfig {
            enabled: true,
            backfill_window_only: true,
            ..ProofHistoryConfig::disabled()
        };

        assert!(config.required_storage_path().is_err());
        assert!(config.backfill_window_only);
    }
}
