#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
//! Live execution-extension that writes canonical block state into the proofs sidecar.

// The following crates are declared as dependencies in `Cargo.toml` because they will
// be consumed by the ExEx run loop that is ported in a subsequent task. They are pulled
// in here with `as _` to silence `unused_crate_dependencies` until that task lands.
use alloy_consensus as _;
use alloy_eips as _;
use futures_util as _;
use reth_execution_types as _;
use reth_provider as _;
use reth_trie as _;
use tokio as _;
use tracing as _;

use alethia_reth_proofs_trie::{ProofsProviderRO, ProofsStorage, ProofsStore};
use reth_exex::ExExContext;
use reth_node_api::FullNodeComponents;
use std::time::Duration;

/// Safety threshold for maximum blocks to prune automatically on startup.
/// If the required prune exceeds this, the node will error out and require manual pruning.
/// Default is 1000 blocks.
const MAX_PRUNE_BLOCKS_STARTUP: u64 = 1000;

/// How many blocks to process in a single batch before yielding. Default is 50 blocks.
const SYNC_BLOCKS_BATCH_SIZE: usize = 50;

/// How close to tip before we process blocks in real-time vs batch. Default is 1024 blocks.
const REAL_TIME_BLOCKS_THRESHOLD: u64 = 1024;

/// How long to sleep when sync task is caught up. Default is 5 seconds.
const SYNC_IDLE_SLEEP_SECS: u64 = 5;

/// Default proofs history window: 72 hours of blocks at 1s block time (Taiko default).
const DEFAULT_PROOFS_HISTORY_WINDOW: u64 = 259_200;

/// Default interval between proof-storage prune runs. Default is 15 seconds.
const DEFAULT_PRUNE_INTERVAL: Duration = Duration::from_secs(15);

/// Default verification interval: disabled.
const DEFAULT_VERIFICATION_INTERVAL: u64 = 0;

// Mark deferred constants as intentionally unused until the run loop is ported in Task 13.
#[allow(dead_code)]
const _: () = {
    let _ = SYNC_BLOCKS_BATCH_SIZE;
    let _ = REAL_TIME_BLOCKS_THRESHOLD;
    let _ = SYNC_IDLE_SLEEP_SECS;
};

/// Builder for [`ProofsExEx`].
#[derive(Debug)]
pub struct ProofsExExBuilder<Node, Storage>
where
    Node: FullNodeComponents,
{
    /// The ExEx context containing the node related utilities e.g. provider, notifications,
    /// events.
    ctx: ExExContext<Node>,
    /// The proofs sidecar storage.
    storage: ProofsStorage<Storage>,
    /// The window (in blocks) of history retained by the proofs sidecar.
    proofs_history_window: u64,
    /// Interval between proof-storage prune runs.
    proofs_history_prune_interval: Duration,
    /// Verification interval (see [`ProofsExEx::verification_interval`]).
    verification_interval: u64,
}

impl<Node, Storage> ProofsExExBuilder<Node, Storage>
where
    Node: FullNodeComponents,
{
    /// Create a new builder with required parameters and defaults.
    pub const fn new(ctx: ExExContext<Node>, storage: ProofsStorage<Storage>) -> Self {
        Self {
            ctx,
            storage,
            proofs_history_window: DEFAULT_PROOFS_HISTORY_WINDOW,
            proofs_history_prune_interval: DEFAULT_PRUNE_INTERVAL,
            verification_interval: DEFAULT_VERIFICATION_INTERVAL,
        }
    }

    /// Sets the window (in blocks) of history retained by the proofs sidecar.
    pub const fn with_proofs_history_window(mut self, window: u64) -> Self {
        self.proofs_history_window = window;
        self
    }

    /// Sets the interval between proof-storage prune runs.
    pub const fn with_proofs_history_prune_interval(mut self, interval: Duration) -> Self {
        self.proofs_history_prune_interval = interval;
        self
    }

    /// Sets the verification interval.
    pub const fn with_verification_interval(mut self, interval: u64) -> Self {
        self.verification_interval = interval;
        self
    }

    /// Builds the [`ProofsExEx`].
    pub fn build(self) -> ProofsExEx<Node, Storage> {
        ProofsExEx {
            ctx: self.ctx,
            storage: self.storage,
            proofs_history_window: self.proofs_history_window,
            proofs_history_prune_interval: self.proofs_history_prune_interval,
            verification_interval: self.verification_interval,
        }
    }
}

/// Taiko proofs ExEx — processes canonical blocks and tracks state changes within the
/// configured proofs-history window.
///
/// Saves and serves trie nodes so that historical proof RPCs can be answered quickly.
/// This struct handles the process of saving the current state, new blocks as they are
/// added, and serving proof RPCs based on the saved data.
#[derive(Debug)]
pub struct ProofsExEx<Node, Storage>
where
    Node: FullNodeComponents,
{
    /// The ExEx context containing the node related utilities e.g. provider, notifications,
    /// events.
    #[allow(dead_code)]
    ctx: ExExContext<Node>,
    /// The proofs sidecar storage.
    storage: ProofsStorage<Storage>,
    /// The window (in blocks) of history retained by the proofs sidecar. Received as a CLI arg.
    proofs_history_window: u64,
    /// Interval between proof-storage prune runs.
    #[allow(dead_code)]
    proofs_history_prune_interval: Duration,
    /// Verification interval: perform full block execution every N blocks for data integrity.
    /// If 0, verification is disabled (always use fast path when available). If 1, verification
    /// is always enabled (always execute blocks).
    #[allow(dead_code)]
    verification_interval: u64,
}

impl<Node, Storage> ProofsExEx<Node, Storage>
where
    Node: FullNodeComponents,
{
    /// Create a new [`ProofsExEx`] instance with default settings.
    pub fn new(ctx: ExExContext<Node>, storage: ProofsStorage<Storage>) -> Self {
        ProofsExExBuilder::new(ctx, storage).build()
    }

    /// Create a new builder for [`ProofsExEx`].
    pub const fn builder(
        ctx: ExExContext<Node>,
        storage: ProofsStorage<Storage>,
    ) -> ProofsExExBuilder<Node, Storage> {
        ProofsExExBuilder::new(ctx, storage)
    }
}

impl<Node, Storage> ProofsExEx<Node, Storage>
where
    Node: FullNodeComponents,
    Storage: ProofsStore + Clone + 'static,
{
    /// Ensure the proofs sidecar storage is initialized and within the configured window.
    ///
    /// Returns an error if the storage has not been seeded yet (prompting the operator to
    /// run `alethia-reth proofs init`), or if the gap between the configured retention
    /// window and the stored history is wider than [`MAX_PRUNE_BLOCKS_STARTUP`] — in which
    /// case the operator is asked to prune manually before starting the node.
    pub fn ensure_initialized(&self) -> eyre::Result<()> {
        // Check if proofs storage is initialized.
        let provider_ro = self.storage.provider_ro()?;
        let earliest_block_number = match provider_ro.get_earliest_block_number()? {
            Some((n, _)) => n,
            None => {
                return Err(eyre::eyre!(
                    "Proofs storage not initialized. Please run 'alethia-reth proofs init --proofs-history.storage-path <PATH>' first."
                ));
            }
        };

        let latest_block_number: u64 = match provider_ro.get_latest_block_number()? {
            Some((n, _)) => n,
            None => {
                return Err(eyre::eyre!(
                    "Proofs storage not initialized. Please run 'alethia-reth proofs init --proofs-history.storage-path <PATH>' first."
                ));
            }
        };

        // Check if we have accumulated too much history for the configured window.
        // If the gap between what we have and what we want to keep is too large, the auto-pruner
        // will stall the node.
        let target_earliest = latest_block_number.saturating_sub(self.proofs_history_window);
        if target_earliest > earliest_block_number {
            let blocks_to_prune = target_earliest - earliest_block_number;
            if blocks_to_prune > MAX_PRUNE_BLOCKS_STARTUP {
                return Err(eyre::eyre!(
                    "Configuration requires pruning {} blocks, which exceeds the safety threshold of {}. \
                     Huge prune operations can stall the node. \
                     Please run 'alethia-reth proofs prune' manually before starting the node.",
                    blocks_to_prune,
                    MAX_PRUNE_BLOCKS_STARTUP
                ));
            }
        }

        // Need to update the earliest block metric on startup as this is not called frequently
        // and can show outdated info.
        self.storage.metrics().block_metrics().earliest_number.set(earliest_block_number as f64);

        Ok(())
    }
}
