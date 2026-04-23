//! CLI flags for the historical-proofs sidecar.

use std::{path::PathBuf, time::Duration};

use clap::Args;

/// Configuration for the historical-proofs sidecar. All flags are optional;
/// the sidecar is enabled by setting `--proofs-history`.
#[derive(Debug, Clone, Args)]
pub struct ProofsHistoryArgs {
    /// Enable the historical-proofs sidecar.
    #[arg(long = "proofs-history", env = "RETH_PROOFS_HISTORY")]
    pub enabled: bool,

    /// Path to the sidecar MDBX environment. Required when the sidecar is enabled.
    #[arg(long = "proofs-history.storage-path", env = "RETH_PROOFS_HISTORY_STORAGE_PATH")]
    pub storage_path: Option<PathBuf>,

    /// Retention window in blocks. Default is 259_200 (72 hours at 1s block time).
    #[arg(
        long = "proofs-history.window",
        env = "RETH_PROOFS_HISTORY_WINDOW",
        default_value_t = 259_200
    )]
    pub window: u64,

    /// Interval between prune runs.
    #[arg(
        long = "proofs-history.prune-interval",
        env = "RETH_PROOFS_HISTORY_PRUNE_INTERVAL",
        value_parser = humantime::parse_duration,
        default_value = "15s"
    )]
    pub prune_interval: Duration,

    /// Maximum blocks processed per prune batch.
    #[arg(
        long = "proofs-history.prune-batch-size",
        env = "RETH_PROOFS_HISTORY_PRUNE_BATCH_SIZE",
        default_value_t = 10_000
    )]
    pub prune_batch_size: u64,

    /// Full block re-execution integrity-check interval (0 = disabled).
    #[arg(
        long = "proofs-history.verification-interval",
        env = "RETH_PROOFS_HISTORY_VERIFICATION_INTERVAL",
        default_value_t = 0
    )]
    pub verification_interval: u64,
}

impl ProofsHistoryArgs {
    /// Validate flag combinations. Run at startup before any allocation.
    pub fn validate(&self) -> eyre::Result<()> {
        if self.enabled && self.storage_path.is_none() {
            eyre::bail!(
                "--proofs-history.storage-path is required when --proofs-history is enabled"
            );
        }
        if self.enabled && self.window == 0 {
            eyre::bail!("--proofs-history.window must be greater than 0");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_always_valid() {
        let args = ProofsHistoryArgs {
            enabled: false,
            storage_path: None,
            window: 0,
            prune_interval: Duration::from_secs(15),
            prune_batch_size: 0,
            verification_interval: 0,
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn enabled_requires_storage_path() {
        let args = ProofsHistoryArgs {
            enabled: true,
            storage_path: None,
            window: 1,
            prune_interval: Duration::from_secs(15),
            prune_batch_size: 10_000,
            verification_interval: 0,
        };
        let err = args.validate().unwrap_err().to_string();
        assert!(err.contains("storage-path"));
    }

    #[test]
    fn enabled_rejects_zero_window() {
        let args = ProofsHistoryArgs {
            enabled: true,
            storage_path: Some("/tmp/p".into()),
            window: 0,
            prune_interval: Duration::from_secs(15),
            prune_batch_size: 10_000,
            verification_interval: 0,
        };
        let err = args.validate().unwrap_err().to_string();
        assert!(err.contains("window"));
    }
}
