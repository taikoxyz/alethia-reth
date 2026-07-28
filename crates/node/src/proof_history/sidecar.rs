//! Proof-history sidecar: notification handling, sync loop, pruner task.

use super::{
    config::ProofHistoryConfig,
    live::LiveTrieCollector,
    opt_block,
    storage_init::{
        DelayedProofHistoryStart, ProofHistoryInitializationAction, delayed_proof_history_start,
        finalized_block_number, initialize_historical_proof_history_storage,
        initialize_proof_history_storage, proof_history_historical_init_metadata_path,
        proof_history_storage_needs_initialization, proof_history_sync_target,
        read_historical_init_metadata,
    },
};
use alethia_reth_rpc::proof_state::ProofHistoryReadiness;
use alloy_consensus::BlockHeader;
use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use alloy_primitives::B256;
use eyre::eyre;
use reth::{
    providers::{
        BlockHashReader, BlockNumReader, BlockReader, CanonStateNotification,
        CanonStateSubscriptions, DBProvider, DatabaseProviderFactory, HeaderProvider,
        TransactionVariant,
    },
    tasks::{TaskExecutor, shutdown::GracefulShutdown},
};
use reth_db::Database;
use reth_execution_types::Chain;
use reth_node_api::{FullNodeComponents, NodePrimitives, NodeTypes};
use reth_optimism_trie::{
    OpProofStoragePruner, OpProofsStorage, OpProofsStorageError, OpProofsStore,
    api::{OpProofsProviderRO, OpProofsProviderRw},
};
use reth_storage_api::{
    ChainStateBlockReader, ChangeSetReader, StorageChangeSetReader, StorageSettingsCache,
};
use reth_trie_common::{HashedPostStateSorted, SortedTrieData, updates::TrieUpdatesSorted};
use std::{panic, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, Notify, broadcast},
    task,
    time::{self, MissedTickBehavior},
};
use tracing::{debug, error, info, warn};

/// Converts blocking-task join failures into errors while preserving panics as panics.
fn blocking_join_result<T>(
    result: Result<T, task::JoinError>,
    task_name: &'static str,
) -> eyre::Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if error.is_panic() => panic::resume_unwind(error.into_panic()),
        Err(error) => Err(eyre!("{task_name} failed to join: {error}")),
    }
}

/// Logs a join failure from the proof-history pruner worker, preserving panics.
fn log_prune_join_result(result: Result<(), task::JoinError>) {
    if let Err(error) = blocking_join_result(result, "proof-history pruner worker") {
        error!(
            target: "reth::taiko::proof_history",
            ?error,
            "proof-history pruner task failed to join blocking worker"
        );
    }
}

/// Number of blocks the proof-history sync task executes in one batch.
const PROOF_HISTORY_SYNC_BATCH_SIZE: usize = 50;

/// Distance from canonical tip where proof-history can process notification data directly.
const PROOF_HISTORY_REAL_TIME_BLOCKS_THRESHOLD: u64 = 1024;

/// Delay used when proof-history has no locally executable backfill work.
const PROOF_HISTORY_SYNC_IDLE_SLEEP: Duration = Duration::from_secs(5);

/// Delay used while waiting for delayed proof-history initialization to become possible.
const PROOF_HISTORY_DELAYED_START_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Delay between polls of the node's executed head while proof-history is caught up, so a
/// staged-sync gap is backfilled even when no live canonical notification arrives.
const PROOF_HISTORY_HEAD_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Number of proof-history blocks pruned in one pruning transaction.
const PROOF_HISTORY_PRUNE_BATCH_SIZE: u64 = 200;

/// Startup reconciliation action for existing proof-history storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProofHistoryStartupAction {
    /// Proof-history storage has no complete retained range and needs the normal initialization
    /// path.
    Uninitialized,
    /// Stored proof-history bounds match canonical state and can be served as-is.
    Ready,
    /// Stored proof-history must be unwound to its retained earliest block before syncing forward.
    UnwindToEarliest {
        /// Earliest retained proof-history block that still matches canonical state.
        earliest: BlockNumHash,
    },
    /// Canonical chain has not yet reached the stored earliest block; reconciliation must retry
    /// once the chain database catches up (e.g. a chain re-sync that kept proof-history storage).
    WaitForCanonicalEarliest {
        /// Earliest retained proof-history block number missing from the canonical chain.
        earliest: u64,
    },
    /// Canonical chain is still behind the stored latest block; reconciliation must retry once
    /// the node catches back up (e.g. right after a restart where the persisted head lags the
    /// previously indexed in-memory tip), and only then compare hashes to decide between serving
    /// as-is and unwinding.
    WaitForCanonicalLatest {
        /// Latest retained proof-history block number.
        latest: u64,
        /// Canonical chain height observed at reconciliation time.
        canonical_best: u64,
    },
}

/// Determines how proof-history storage should be reconciled against canonical block hashes.
pub(super) fn proof_history_startup_action(
    earliest: Option<(u64, B256)>,
    latest: Option<(u64, B256)>,
    canonical_best: u64,
    canonical_earliest_hash: Option<B256>,
    canonical_latest_hash: Option<B256>,
) -> eyre::Result<ProofHistoryStartupAction> {
    let (Some((earliest_number, earliest_hash)), Some((latest_number, latest_hash))) =
        (earliest, latest)
    else {
        return Ok(ProofHistoryStartupAction::Uninitialized);
    };

    if earliest_number > latest_number {
        return Err(eyre!(
            "proof-history storage has earliest block {earliest_number} after latest block {latest_number}"
        ));
    }

    // No canonical header at the earliest height yet: the chain database is (re-)syncing and has
    // not reached the retained range. Failing here would crash the node before it could ever sync
    // past this point, so wait instead; a *mismatching* hash stays a hard error below.
    let Some(canonical_earliest) = canonical_earliest_hash else {
        return Ok(ProofHistoryStartupAction::WaitForCanonicalEarliest {
            earliest: earliest_number,
        });
    };
    if canonical_earliest != earliest_hash {
        return Err(eyre!(
            "proof-history earliest stored block {earliest_number} hash {earliest_hash:?} is not canonical; wipe proof-history storage and restart initialization"
        ));
    }

    // The stored head runs ahead of the canonical chain. This is the normal aftermath of an
    // ungraceful restart: live indexing follows in-memory commits, which outpace the persisted
    // head by up to the engine persistence threshold. Wait for canonical state to catch back up
    // and only then decide between `Ready` and an unwind — discarding the retained window here
    // would trade a few seconds of catch-up for days of re-execution.
    if latest_number > canonical_best {
        return Ok(ProofHistoryStartupAction::WaitForCanonicalLatest {
            latest: latest_number,
            canonical_best,
        });
    }

    let Some(canonical_latest) = canonical_latest_hash else {
        return Err(eyre!(
            "canonical chain has no canonical hash for stored proof-history latest block {latest_number} at or below canonical best {canonical_best}"
        ));
    };

    if canonical_latest == latest_hash {
        return Ok(ProofHistoryStartupAction::Ready);
    }

    // The canonical chain reached the stored height with a different block: real divergence
    // (a reorg happened while the sidecar was down). Rewind to the validated earliest anchor.
    Ok(ProofHistoryStartupAction::UnwindToEarliest {
        earliest: BlockNumHash::new(earliest_number, earliest_hash),
    })
}

/// Taiko proof-history sidecar that keeps OP proofs storage behind locally executed state.
#[derive(Debug)]
pub(super) struct ProofHistorySidecar<Node, Storage>
where
    Node: FullNodeComponents,
{
    /// Canonical provider used for state notifications and block reads.
    provider: Node::Provider,
    /// EVM configuration used to execute blocks for proof-history updates.
    evm_config: Node::Evm,
    /// Task executor used to spawn critical proof-history workers.
    task_executor: TaskExecutor,
    /// Proof-history storage populated by the extension.
    storage: OpProofsStorage<Storage>,
    /// Raw proof-history storage handle used for the initial current-state snapshot.
    init_storage: Storage,
    /// Runtime settings that govern proof-history retention and startup behavior.
    config: ProofHistoryConfig,
    /// Sidecar file that records historical initialization target metadata.
    historical_init_metadata_path: Option<PathBuf>,
    /// Readiness flag consumed by the RPC layer; set only while storage is reconciled.
    readiness: ProofHistoryReadiness,
    /// Serializes proof-history writers across live notifications, background sync, and pruning.
    write_lock: Arc<Mutex<()>>,
}

/// Returns whether a committed chain starting at `first_block` leaves no gap above the stored
/// proof-history head, so its blocks can be consumed directly from the notification.
const fn committed_chain_is_contiguous(first_block: u64, latest_stored: u64) -> bool {
    first_block <= latest_stored.saturating_add(1)
}

/// Ensures a canonical reorg or revert does not replace the retained proof-history anchor.
fn ensure_canonical_update_above_earliest(
    update_kind: &'static str,
    earliest: BlockNumHash,
    first_old: BlockNumHash,
) -> eyre::Result<()> {
    if first_old.number <= earliest.number {
        return Err(eyre!(
            "proof-history {update_kind} touches retained earliest block {} hash {:?} with old block {} hash {:?}; wipe proof-history storage and restart initialization",
            earliest.number,
            earliest.hash,
            first_old.number,
            first_old.hash
        ));
    }

    Ok(())
}

impl<Node, Storage> ProofHistorySidecar<Node, Storage>
where
    Node: FullNodeComponents,
{
    /// Creates a proof-history sidecar with Taiko backfill guards.
    pub(super) fn new(
        provider: Node::Provider,
        evm_config: Node::Evm,
        task_executor: TaskExecutor,
        storage: OpProofsStorage<Storage>,
        init_storage: Storage,
        config: ProofHistoryConfig,
        readiness: ProofHistoryReadiness,
    ) -> Self {
        let historical_init_metadata_path =
            config.storage_path.as_deref().map(proof_history_historical_init_metadata_path);
        Self {
            provider,
            evm_config,
            task_executor,
            storage,
            init_storage,
            config,
            historical_init_metadata_path,
            readiness,
            write_lock: Arc::new(Mutex::new(())),
        }
    }
}

impl<Node, Storage, Primitives> ProofHistorySidecar<Node, Storage>
where
    Node: FullNodeComponents<Types: NodeTypes<Primitives = Primitives>>,
    Node::Provider: BlockHashReader
        + BlockNumReader
        + BlockReader
        + CanonStateSubscriptions
        + DatabaseProviderFactory,
    <Node::Provider as DatabaseProviderFactory>::Provider: BlockNumReader
        + ChainStateBlockReader
        + ChangeSetReader
        + DBProvider
        + HeaderProvider
        + StorageChangeSetReader
        + StorageSettingsCache,
    <Node::Provider as DatabaseProviderFactory>::DB: Database,
    <<Node::Provider as DatabaseProviderFactory>::DB as Database>::TX: Sync,
    Primitives: NodePrimitives,
    Storage: OpProofsStore + Clone + Send + 'static,
{
    /// Runs proof-history indexing until the node shuts down.
    pub(super) async fn run(self, mut shutdown: GracefulShutdown) -> eyre::Result<()> {
        let collector =
            LiveTrieCollector::new(self.evm_config.clone(), self.provider.clone(), &self.storage);
        let mut notifications = self.provider.subscribe_to_canonical_state();
        let mut sync_wake = self.try_start().await?;
        let mut retry_interval = time::interval(PROOF_HISTORY_DELAYED_START_RETRY_INTERVAL);
        retry_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                notification = notifications.recv() => {
                    let notification = match notification {
                        Ok(notification) => notification,
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                target: "reth::taiko::proof_history",
                                skipped,
                                "proof-history sidecar lagged canonical notifications; reconciling storage"
                            );
                            // Replace the lagged receiver before reconciliation. The old
                            // receiver's retained suffix is no longer useful after storage is
                            // reconciled, and the fresh receiver buffers commits published while
                            // recovery brings storage back in line with canonical state.
                            notifications = self.provider.subscribe_to_canonical_state();
                            if let Some(wake) = sync_wake.as_ref() {
                                self.recover_from_lag(wake).await?;
                            } else {
                                sync_wake = self.try_start().await?;
                            }
                            continue;
                        }
                    };
                    if sync_wake.is_none() {
                        sync_wake = self.try_start().await?;
                    }

                    let Some(wake) = sync_wake.as_ref() else {
                        continue;
                    };

                    self.handle_notification(notification, &collector, wake).await?;
                }
                _ = &mut shutdown => break,
                _ = retry_interval.tick(), if sync_wake.is_none() => {
                    sync_wake = self.try_start().await?;
                }
            }
        }

        Ok(())
    }

    /// Reconciles storage if possible and spawns the sync and pruner tasks on first success.
    async fn try_start(&self) -> eyre::Result<Option<Arc<Notify>>> {
        if !self.reconcile_or_wait().await? {
            return Ok(None);
        }
        // Storage bounds are now validated against canonical hashes: allow the RPC layer to
        // serve from proof-history. Workers only extend storage consistently from here on.
        self.readiness.set_ready();
        let sync_wake = self.spawn_sync_task();
        self.spawn_pruner_task();
        Ok(Some(sync_wake))
    }

    /// Reconciles proof-history storage after missing canonical notifications.
    async fn recover_from_lag(&self, sync_wake: &Notify) -> eyre::Result<()> {
        let _write_guard = self.write_lock.lock().await;
        // Notifications were missed, so the stored bounds are unvalidated until reconciliation
        // succeeds; stop serving proof-history state in the meantime.
        self.readiness.set_not_ready();
        if !self.reconcile_or_wait().await? {
            return Err(eyre!(
                "proof-history reconciliation cannot proceed while sync workers are running \
                 (canonical state moved backwards during a notification lag); restart the node to \
                 recover"
            ));
        }
        self.readiness.set_ready();
        sync_wake.notify_one();
        Ok(())
    }

    /// Reconciles current proof-history bounds against the canonical database.
    async fn reconcile_or_wait(&self) -> eyre::Result<bool> {
        match self.startup_action()? {
            ProofHistoryStartupAction::Uninitialized => self.initialize_or_wait().await,
            ProofHistoryStartupAction::Ready => {
                self.ensure_initialized()?;
                Ok(true)
            }
            ProofHistoryStartupAction::UnwindToEarliest { earliest } => {
                self.unwind_to_earliest(earliest).await?;
                self.ensure_initialized()?;
                Ok(true)
            }
            ProofHistoryStartupAction::WaitForCanonicalEarliest { earliest } => {
                // Common during a chain re-sync that kept proof-history storage: stay quiet at
                // debug level, this state can last for days and resolves on its own.
                debug!(
                    target: "reth::taiko::proof_history",
                    earliest,
                    "canonical chain has not reached the proof-history earliest block; waiting for sync"
                );
                Ok(false)
            }
            ProofHistoryStartupAction::WaitForCanonicalLatest { latest, canonical_best } => {
                // Expected briefly after an ungraceful restart while the driver re-derives the
                // gap; warn so a *stuck* wait (chain unwound for good) stays visible.
                warn!(
                    target: "reth::taiko::proof_history",
                    latest,
                    canonical_best,
                    "canonical chain is behind the stored proof-history head; waiting for the node to catch up before reconciling"
                );
                Ok(false)
            }
        }
    }

    /// Computes the reconciliation action for the current proof-history storage bounds.
    fn startup_action(&self) -> eyre::Result<ProofHistoryStartupAction> {
        let provider_ro = self.storage.provider_ro()?;
        let earliest = opt_block(provider_ro.get_earliest_block())?;
        let latest = opt_block(provider_ro.get_latest_block())?;
        let canonical_best = self.provider.best_block_number()?;
        let canonical_earliest_hash =
            earliest.map(|(number, _)| self.provider.block_hash(number)).transpose()?.flatten();
        let canonical_latest_hash = latest
            .filter(|(number, _)| *number <= canonical_best)
            .map(|(number, _)| self.provider.block_hash(number))
            .transpose()?
            .flatten();

        proof_history_startup_action(
            earliest,
            latest,
            canonical_best,
            canonical_earliest_hash,
            canonical_latest_hash,
        )
    }

    /// Unwinds proof-history storage so its latest retained block is the canonical earliest block.
    async fn unwind_to_earliest(&self, earliest: BlockNumHash) -> eyre::Result<()> {
        let latest = opt_block(self.storage.provider_ro()?.get_latest_block())?
            .ok_or_else(|| eyre!("no latest proof-history block to unwind"))?
            .0;
        if latest <= earliest.number {
            return Ok(());
        }

        info!(
            target: "reth::taiko::proof_history",
            latest,
            earliest = earliest.number,
            "unwinding proof-history storage to retained canonical earliest block"
        );

        let unwind_block_number = earliest
            .number
            .checked_add(1)
            .ok_or_else(|| eyre!("cannot unwind proof-history beyond u64::MAX block"))?;
        let unwind_block_hash = self
            .provider
            .block_hash(unwind_block_number)?
            .ok_or_else(|| eyre!("missing proof-history unwind block {unwind_block_number}"))?;
        let unwind_to = BlockWithParent::new(
            earliest.hash,
            BlockNumHash::new(unwind_block_number, unwind_block_hash),
        );

        let storage = self.storage.clone();
        let unwind_task = task::spawn_blocking(move || -> Result<(), OpProofsStorageError> {
            let provider_rw = storage.provider_rw()?;
            provider_rw.unwind_history(unwind_to)?;
            provider_rw.commit()
        });
        blocking_join_result(unwind_task.await, "proof-history unwind worker")??;
        Ok(())
    }

    /// Initializes proof-history storage immediately or waits for the finalized window.
    async fn initialize_or_wait(&self) -> eyre::Result<bool> {
        if proof_history_storage_needs_initialization(&self.storage)? {
            let action = if let Some(resume) = self.historical_init_resume_action()? {
                resume
            } else if self.config.backfill_window_only {
                self.finalized_window_initialization_action()?
            } else {
                ProofHistoryInitializationAction::CurrentState
            };

            match action {
                ProofHistoryInitializationAction::Wait => return Ok(false),
                ProofHistoryInitializationAction::CurrentState => {
                    let provider = self.provider.clone();
                    let storage = self.init_storage.clone();
                    let init_task = task::spawn_blocking(move || {
                        initialize_proof_history_storage(&provider, storage)
                    });
                    blocking_join_result(init_task.await, "proof-history init worker")??;
                }
                ProofHistoryInitializationAction::HistoricalWindow {
                    start_block,
                    target_block,
                } => {
                    let provider = self.provider.clone();
                    let storage = self.init_storage.clone();
                    let metadata_path = self.historical_init_metadata_path.clone();
                    let init_task = task::spawn_blocking(move || {
                        initialize_historical_proof_history_storage(
                            &provider,
                            storage,
                            metadata_path.as_deref(),
                            start_block,
                            target_block,
                        )
                    });
                    blocking_join_result(init_task.await, "proof-history historical init worker")??;
                }
            }
        }
        self.ensure_initialized()?;
        Ok(true)
    }

    /// Returns the initialization action that resumes an interrupted historical initialization.
    ///
    /// The recorded metadata pins the anchor start block of the interrupted attempt; the target is
    /// recomputed from the current on-disk executed head because the reverse-changeset overlay is
    /// rebuilt against the current tables (and re-verified against the anchor state root), so an
    /// interrupted initialization survives node restarts on a live chain.
    fn historical_init_resume_action(
        &self,
    ) -> eyre::Result<Option<ProofHistoryInitializationAction>> {
        let Some(path) = self.historical_init_metadata_path.as_deref() else {
            return Ok(None);
        };
        let Some(metadata) = read_historical_init_metadata(path)? else {
            return Ok(None);
        };

        let executed_head = self.provider.database_provider_ro()?.best_block_number()?;
        info!(
            target: "reth::taiko::proof_history",
            start_block = metadata.start_block.number,
            executed_head,
            "resuming interrupted historical proof-history initialization from recorded metadata"
        );
        Ok(Some(ProofHistoryInitializationAction::HistoricalWindow {
            start_block: metadata.start_block.number,
            target_block: executed_head,
        }))
    }

    /// Returns how empty storage should initialize for a finalized proof-history window.
    fn finalized_window_initialization_action(
        &self,
    ) -> eyre::Result<ProofHistoryInitializationAction> {
        let finalized_block = finalized_block_number(&self.provider)?;
        // Use the on-disk best block as `executed_head` so that the historical-init target header
        // and reverse changesets are guaranteed to be persisted. The in-memory canonical tip from
        // `provider().best_block_number()` can outpace disk by up to `engine.persistence-threshold`
        // blocks, which previously caused the historical init to panic on a missing target header.
        let executed_head = self.provider.database_provider_ro()?.best_block_number()?;

        match delayed_proof_history_start(finalized_block, executed_head, self.config.window) {
            DelayedProofHistoryStart::WaitForFinalized => {
                debug!(
                    target: "reth::taiko::proof_history",
                    executed_head,
                    "waiting for finalized head before initializing empty proof-history storage"
                );
                Ok(ProofHistoryInitializationAction::Wait)
            }
            DelayedProofHistoryStart::WaitForExecution { start_block } => {
                debug!(
                    target: "reth::taiko::proof_history",
                    ?finalized_block,
                    executed_head,
                    start_block,
                    "waiting for local execution to reach proof-history window start"
                );
                Ok(ProofHistoryInitializationAction::Wait)
            }
            DelayedProofHistoryStart::MissedStart { start_block } => {
                info!(
                    target: "reth::taiko::proof_history",
                    ?finalized_block,
                    executed_head,
                    start_block,
                    "empty proof-history storage missed the finalized window start; building historical proof-history anchor"
                );
                Ok(ProofHistoryInitializationAction::HistoricalWindow {
                    start_block,
                    target_block: executed_head,
                })
            }
            DelayedProofHistoryStart::Ready { start_block } => {
                info!(
                    target: "reth::taiko::proof_history",
                    ?finalized_block,
                    executed_head,
                    start_block,
                    "initializing empty proof-history storage from finalized window"
                );
                Ok(ProofHistoryInitializationAction::CurrentState)
            }
        }
    }

    /// Verifies the proof-history database is initialized and safe to prune automatically.
    fn ensure_initialized(&self) -> eyre::Result<()> {
        let provider_ro = self.storage.provider_ro()?;
        let earliest_block_number = opt_block(provider_ro.get_earliest_block())?
            .ok_or_else(|| eyre!("proof-history storage is not initialized"))?
            .0;
        let latest_block_number = opt_block(provider_ro.get_latest_block())?
            .ok_or_else(|| eyre!("proof-history storage is not initialized"))?
            .0;

        let target_earliest = latest_block_number.saturating_sub(self.config.window);
        if target_earliest > earliest_block_number {
            let blocks_to_prune = target_earliest - earliest_block_number;
            if blocks_to_prune > self.config.max_startup_prune_blocks {
                return Err(eyre!(
                    "configuration requires pruning {} proof-history blocks, which exceeds the safety threshold of {}; raise --proofs-history.max-startup-prune-blocks or restore the previous --proofs-history.window to proceed",
                    blocks_to_prune,
                    self.config.max_startup_prune_blocks
                ));
            }
        }

        Ok(())
    }

    /// Spawns the periodic proof-history pruning task.
    fn spawn_pruner_task(&self) {
        let pruner = Arc::new(
            OpProofStoragePruner::new(
                self.storage.clone(),
                self.provider.clone(),
                self.config.window,
            )
            .with_batch_size(PROOF_HISTORY_PRUNE_BATCH_SIZE),
        );
        let prune_interval = self.config.prune_interval;
        let retention_window = self.config.window;
        let write_lock = self.write_lock.clone();

        self.task_executor
            .spawn_critical_with_graceful_shutdown_signal(
                "taiko::proof_history::pruner",
                move |mut signal| Box::pin(async move {
                    info!(
                        target: "reth::taiko::proof_history",
                        window = retention_window,
                        interval_secs = prune_interval.as_secs(),
                        "starting proof-history pruner task"
                    );

                    let mut interval = time::interval(prune_interval);
                    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

                    loop {
                        tokio::select! {
                            _ = &mut signal => {
                                info!(target: "reth::taiko::proof_history", "proof-history pruner task stopped");
                                break;
                            }
                            _ = interval.tick() => {
                                let _write_guard = write_lock.lock().await;
                                let pruner = pruner.clone();
                                let mut prune_task = task::spawn_blocking(move || pruner.run());
                                tokio::select! {
                                    result = &mut prune_task => log_prune_join_result(result),
                                    _ = &mut signal => {
                                        // `spawn_blocking` workers cannot be aborted, so wait for
                                        // the prune to finish to avoid tearing down a write txn
                                        // mid-flight. A deeper fix would need a cancel-aware
                                        // pruner API or smaller prune chunks.
                                        info!(
                                            target: "reth::taiko::proof_history",
                                            "shutdown requested while proof-history prune is running; waiting for prune to finish"
                                        );
                                        log_prune_join_result(prune_task.await);
                                        info!(target: "reth::taiko::proof_history", "proof-history pruner task stopped");
                                        break;
                                    }
                                }
                            }
                        }
                    }
                })
            );
    }

    /// Spawns the guarded proof-history backfill task and returns its wake-up handle.
    fn spawn_sync_task(&self) -> Arc<Notify> {
        let sync_wake = Arc::new(Notify::new());
        let task_wake = sync_wake.clone();
        let task_storage = self.storage.clone();
        let task_provider = self.provider.clone();
        let task_evm_config = self.evm_config.clone();
        let task_write_lock = self.write_lock.clone();

        self.task_executor.spawn_critical_with_graceful_shutdown_signal(
            "taiko::proof_history::sync_loop",
            move |shutdown| {
                Box::pin(async move {
                    Self::sync_loop(
                        shutdown,
                        task_wake,
                        task_storage,
                        task_provider,
                        task_evm_config,
                        task_write_lock,
                    )
                    .await;
                })
            },
        );

        sync_wake
    }

    /// Backfills proof-history only through blocks the node has locally executed.
    ///
    /// Live notifications only wake this loop up; the backfill target is always re-derived from
    /// the node's on-disk executed head, so re-execution never reads unpersisted blocks and a
    /// staged-sync gap is caught up even when no notification arrives.
    async fn sync_loop(
        mut shutdown: GracefulShutdown,
        wake: Arc<Notify>,
        storage: OpProofsStorage<Storage>,
        provider: Node::Provider,
        evm_config: Node::Evm,
        write_lock: Arc<Mutex<()>>,
    ) {
        debug!(target: "reth::taiko::proof_history", "starting proof-history sync loop");

        // Whether the current divergence episode (stored head ahead of executed head) has already
        // been warned about, so the 5s idle poll does not re-emit the warning on every tick.
        let mut divergence_logged = false;

        loop {
            let write_guard = write_lock.lock().await;
            let latest = match storage.provider_ro().and_then(|p| p.get_latest_block()) {
                Ok(numhash) => numhash.number,
                Err(OpProofsStorageError::NoBlocksFound) => {
                    error!(target: "reth::taiko::proof_history", "proof-history sync loop found no stored blocks; stopping sync loop");
                    return;
                }
                Err(error) => {
                    error!(target: "reth::taiko::proof_history", ?error, "failed to read proof-history latest block");
                    drop(write_guard);
                    if Self::sleep_or_shutdown(&mut shutdown, PROOF_HISTORY_SYNC_IDLE_SLEEP).await {
                        return;
                    }
                    continue;
                }
            };

            // Track the node's on-disk executed head, not just the last notified canonical tip.
            // Using the on-disk head (rather than the in-memory tip) guarantees the blocks the
            // backfill re-executes are persisted, and lets proof-history catch up across a
            // staged-sync gap even when no live notification arrives.
            let executed_head = match provider
                .database_provider_ro()
                .and_then(|p| p.best_block_number())
            {
                Ok(number) => number,
                Err(error) => {
                    error!(target: "reth::taiko::proof_history", ?error, "failed to read executed head for proof-history sync");
                    drop(write_guard);
                    if Self::sleep_or_shutdown(&mut shutdown, PROOF_HISTORY_SYNC_IDLE_SLEEP).await {
                        return;
                    }
                    continue;
                }
            };

            // Surface divergence: proof-history's stored head sits above the node's executed head.
            // This only arises after the on-disk head regresses (a reorg/unwind) and is normally
            // repaired by the notification-driven reorg/revert handlers — which never run when no
            // live notification arrives, the staged-sync-gap case this loop guards. Warn once per
            // episode (the idle poll would otherwise re-warn every tick) so a stuck/diverged
            // sidecar is observable instead of indistinguishable from healthy idle.
            if latest > executed_head {
                if !divergence_logged {
                    warn!(
                        target: "reth::taiko::proof_history",
                        latest,
                        executed_head,
                        "proof-history stored head is ahead of the node's executed head; awaiting a canonical notification to reconcile"
                    );
                    divergence_logged = true;
                }
            } else {
                divergence_logged = false;
            }

            let Some(target) = proof_history_sync_target(latest, executed_head) else {
                // Caught up to the locally executed head. Wake on the next live notification (fast
                // path) or after a poll delay, so a staged-sync gap is still picked up with no
                // notifications.
                drop(write_guard);
                tokio::select! {
                    _ = &mut shutdown => {
                        info!(target: "reth::taiko::proof_history", "proof-history sync loop stopped");
                        return;
                    }
                    _ = wake.notified() => {}
                    _ = time::sleep(PROOF_HISTORY_HEAD_POLL_INTERVAL) => {}
                }
                continue;
            };

            let batch_provider = provider.clone();
            let batch_storage = storage.clone();
            let batch_evm_config = evm_config.clone();
            // Each block write commits independently; if this batch fails part-way through, the
            // next loop rereads `latest` and resumes after the last committed block.
            let mut batch_task = task::spawn_blocking(move || {
                let collector_storage = batch_storage.clone();
                let collector = LiveTrieCollector::new(
                    batch_evm_config,
                    batch_provider.clone(),
                    &collector_storage,
                );
                Self::process_batch(
                    latest,
                    target,
                    &batch_provider,
                    &collector,
                    PROOF_HISTORY_SYNC_BATCH_SIZE,
                )
            });
            let batch_result = tokio::select! {
                result = &mut batch_task => {
                    blocking_join_result(result, "proof-history batch worker")
                        .and_then(|result| result)
                }
                _ = &mut shutdown => {
                    // `spawn_blocking` workers cannot be aborted; wait for the in-flight batch so
                    // its per-block commits finish cleanly before stopping.
                    info!(
                        target: "reth::taiko::proof_history",
                        "shutdown requested while proof-history backfill batch is running; waiting for batch to finish"
                    );
                    let result = blocking_join_result(batch_task.await, "proof-history batch worker")
                        .and_then(|result| result);
                    drop(write_guard);
                    if let Err(error) = result {
                        error!(target: "reth::taiko::proof_history", ?error, "proof-history batch processing failed");
                    }
                    info!(target: "reth::taiko::proof_history", "proof-history sync loop stopped");
                    return;
                }
            };
            drop(write_guard);

            match batch_result {
                Ok(backfilled_to) => {
                    info!(
                        target: "reth::taiko::proof_history",
                        backfilled_to,
                        head = executed_head,
                        "proof-history backfill batch committed"
                    );
                }
                Err(error) => {
                    error!(target: "reth::taiko::proof_history", ?error, "proof-history batch processing failed");
                    if Self::sleep_or_shutdown(&mut shutdown, PROOF_HISTORY_SYNC_IDLE_SLEEP).await {
                        return;
                    }
                }
            }

            task::yield_now().await;
        }
    }

    /// Sleeps for `duration` unless shutdown is requested first; returns whether to stop.
    async fn sleep_or_shutdown(shutdown: &mut GracefulShutdown, duration: Duration) -> bool {
        tokio::select! {
            _ = shutdown => {
                info!(target: "reth::taiko::proof_history", "proof-history sync loop stopped");
                true
            }
            _ = time::sleep(duration) => false,
        }
    }

    /// Processes a bounded batch of canonical blocks into proof-history storage.
    ///
    /// Returns the highest block number processed in this batch.
    fn process_batch(
        start: u64,
        target: u64,
        provider: &Node::Provider,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
        batch_size: usize,
    ) -> eyre::Result<u64> {
        let end = start.saturating_add(batch_size as u64).min(target);
        debug!(target: "reth::taiko::proof_history", start, end, "processing proof-history batch");

        for block_num in (start + 1)..=end {
            let block = provider
                .recovered_block(block_num.into(), TransactionVariant::NoHash)?
                .ok_or_else(|| eyre!("missing block {block_num}"))?;
            collector.execute_and_store_block_updates(&block)?;
        }

        Ok(end)
    }

    /// Handles a canonical notification and advances proof-history storage or wakes the backfill.
    async fn handle_notification(
        &self,
        notification: CanonStateNotification<Primitives>,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
        sync_wake: &Notify,
    ) -> eyre::Result<()> {
        let _write_guard = self.write_lock.lock().await;
        let provider_ro = self.storage.provider_ro()?;
        let earliest_stored = opt_block(provider_ro.get_earliest_block())?
            .ok_or_else(|| eyre!("no earliest proof-history block stored"))?;
        let latest_stored = opt_block(provider_ro.get_latest_block())?
            .ok_or_else(|| eyre!("no latest proof-history block stored"))?
            .0;
        let earliest_stored = BlockNumHash::new(earliest_stored.0, earliest_stored.1);

        match &notification {
            CanonStateNotification::Commit { new } => {
                self.handle_chain_committed(new, latest_stored, collector, sync_wake)?
            }
            // A reorg that replaces the old blocks with nothing is a plain revert.
            CanonStateNotification::Reorg { old, new } if new.is_empty() => {
                self.handle_chain_reverted(old, earliest_stored, latest_stored, collector)?
            }
            CanonStateNotification::Reorg { old, new } => {
                self.handle_chain_reorged(old, new, earliest_stored, latest_stored, collector)?
            }
        }

        Ok(())
    }

    /// Handles a canonical chain commit notification.
    fn handle_chain_committed(
        &self,
        new: &Chain<Primitives>,
        latest_stored: u64,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
        sync_wake: &Notify,
    ) -> eyre::Result<()> {
        if new.tip().number() <= latest_stored {
            return Ok(());
        }

        let best_block = self.provider.best_block_number()?;
        let is_contiguous = committed_chain_is_contiguous(new.first().number(), latest_stored);
        let is_near_tip = best_block.saturating_sub(new.tip().number()) <
            PROOF_HISTORY_REAL_TIME_BLOCKS_THRESHOLD;

        if is_contiguous && is_near_tip {
            for block_number in latest_stored.saturating_add(1)..=new.tip().number() {
                self.process_block(block_number, new, collector)?;
            }
        } else {
            sync_wake.notify_one();
        }

        Ok(())
    }

    /// Processes one block from notification trie data when possible, or by execution otherwise.
    fn process_block(
        &self,
        block_number: u64,
        chain: &Chain<Primitives>,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
    ) -> eyre::Result<()> {
        let should_verify = self.config.verification_interval > 0 &&
            block_number.is_multiple_of(self.config.verification_interval);

        if !should_verify &&
            let Some(block) = chain.blocks().get(&block_number) &&
            let Some(trie_data) = chain.trie_data_at(block_number)
        {
            let SortedTrieData { hashed_state, trie_updates } = &trie_data.get().sorted;
            collector.store_block_updates(
                block.block_with_parent(),
                (**trie_updates).clone(),
                (**hashed_state).clone(),
            )?;
            return Ok(());
        }

        let block = self
            .provider
            .recovered_block(block_number.into(), TransactionVariant::NoHash)?
            .ok_or_else(|| eyre!("missing block {block_number} in provider"))?;
        collector.execute_and_store_block_updates(&block)?;
        Ok(())
    }

    /// Handles a canonical chain reorg notification.
    fn handle_chain_reorged(
        &self,
        old: &Chain<Primitives>,
        new: &Chain<Primitives>,
        earliest_stored: BlockNumHash,
        latest_stored: u64,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
    ) -> eyre::Result<()> {
        if old.first().number() > latest_stored {
            return Ok(());
        }

        ensure_canonical_update_above_earliest(
            "reorg",
            earliest_stored,
            BlockNumHash::new(old.first().number(), old.first().hash()),
        )?;

        if old.fork_block() != new.fork_block() {
            return Err(eyre!(
                "proof-history fork blocks do not match: old={:?}, new={:?}",
                old.fork_block(),
                new.fork_block()
            ));
        }

        let mut block_updates: Vec<(
            BlockWithParent,
            Arc<TrieUpdatesSorted>,
            Arc<HashedPostStateSorted>,
        )> = Vec::with_capacity(new.len());

        for (block_number, block) in new.blocks() {
            let Some(trie_data) = new.trie_data_at(*block_number) else {
                // Missing trie data on at least one new block: fall back to
                // unwinding the old branch first, then re-process all new
                // blocks individually so executions read post-unwind parent
                // state instead of stale old-branch state.
                collector.unwind_history(old.first().block_with_parent())?;
                for block_number in new.blocks().keys() {
                    self.process_block(*block_number, new, collector)?;
                }
                return Ok(());
            };
            let SortedTrieData { hashed_state, trie_updates } = &trie_data.get().sorted;
            block_updates.push((
                block.block_with_parent(),
                trie_updates.clone(),
                hashed_state.clone(),
            ));
        }

        if !block_updates.is_empty() {
            collector.unwind_and_store_block_updates(block_updates)?;
        }

        Ok(())
    }

    /// Handles a canonical chain revert notification.
    fn handle_chain_reverted(
        &self,
        old: &Chain<Primitives>,
        earliest_stored: BlockNumHash,
        latest_stored: u64,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
    ) -> eyre::Result<()> {
        if old.first().number() > latest_stored {
            return Ok(());
        }

        ensure_canonical_update_above_earliest(
            "revert",
            earliest_stored,
            BlockNumHash::new(old.first().number(), old.first().hash()),
        )?;

        collector.unwind_history(old.first().block_with_parent())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProofHistoryStartupAction, committed_chain_is_contiguous,
        ensure_canonical_update_above_earliest, proof_history_startup_action,
    };
    use alloy_eips::BlockNumHash;
    use alloy_primitives::B256;

    fn hash(byte: u8) -> B256 {
        B256::with_last_byte(byte)
    }

    #[test]
    fn committed_chain_contiguity_accepts_next_block_and_overlaps() {
        // The next block extends stored history directly.
        assert!(committed_chain_is_contiguous(101, 100));
        // A commit whose chain starts at or below the stored head (e.g. buffered notifications
        // overlapping blocks the backfill already stored) leaves no gap either; the per-block
        // loop starts above the stored head and consumes only the new suffix.
        assert!(committed_chain_is_contiguous(95, 100));
    }

    #[test]
    fn committed_chain_contiguity_rejects_gaps() {
        assert!(!committed_chain_is_contiguous(102, 100));
    }

    #[test]
    fn committed_chain_contiguity_saturates_at_max_height() {
        assert!(committed_chain_is_contiguous(u64::MAX, u64::MAX));
    }

    #[test]
    fn proof_history_update_guard_rejects_reorg_touching_earliest() {
        let error = ensure_canonical_update_above_earliest(
            "reorg",
            BlockNumHash::new(10, hash(10)),
            BlockNumHash::new(10, hash(11)),
        )
        .expect_err("reorg replacing earliest must fail closed");

        let message = error.to_string();
        assert!(message.contains("proof-history reorg touches retained earliest block 10"));
        assert!(message.contains("wipe proof-history storage"));
    }

    #[test]
    fn proof_history_update_guard_rejects_revert_touching_earliest() {
        let error = ensure_canonical_update_above_earliest(
            "revert",
            BlockNumHash::new(10, hash(10)),
            BlockNumHash::new(10, hash(10)),
        )
        .expect_err("revert unwinding earliest must fail closed");

        let message = error.to_string();
        assert!(message.contains("proof-history revert touches retained earliest block 10"));
        assert!(message.contains("wipe proof-history storage"));
    }

    #[test]
    fn proof_history_update_guard_allows_reorg_above_earliest() {
        ensure_canonical_update_above_earliest(
            "reorg",
            BlockNumHash::new(10, hash(10)),
            BlockNumHash::new(11, hash(11)),
        )
        .expect("reorg after retained earliest should be allowed");
    }

    #[test]
    fn proof_history_startup_action_ready_when_latest_is_canonical() {
        let action = proof_history_startup_action(
            Some((10, hash(10))),
            Some((20, hash(20))),
            20,
            Some(hash(10)),
            Some(hash(20)),
        )
        .expect("canonical latest should be ready");

        assert_eq!(action, ProofHistoryStartupAction::Ready);
    }

    #[test]
    fn proof_history_startup_action_errors_when_latest_canonical_but_earliest_noncanonical() {
        let error = proof_history_startup_action(
            Some((10, hash(11))),
            Some((20, hash(20))),
            20,
            Some(hash(10)),
            Some(hash(20)),
        )
        .expect_err("noncanonical earliest must fail even when latest is canonical");

        assert!(error.to_string().contains("earliest stored block"));
    }

    #[test]
    fn proof_history_startup_action_unwinds_when_latest_mismatches_and_earliest_is_canonical() {
        let action = proof_history_startup_action(
            Some((10, hash(10))),
            Some((20, hash(21))),
            20,
            Some(hash(10)),
            Some(hash(20)),
        )
        .expect("canonical earliest should allow retained-window rewind");

        assert_eq!(
            action,
            ProofHistoryStartupAction::UnwindToEarliest {
                earliest: BlockNumHash::new(10, hash(10))
            }
        );
    }

    #[test]
    fn proof_history_startup_action_waits_when_latest_is_ahead_of_canonical_best() {
        // An ungraceful restart leaves the persisted head a few blocks behind the previously
        // indexed in-memory tip. The driver re-derives the same blocks within seconds, so wait
        // for canonical state to catch up instead of discarding the whole retained window.
        let action = proof_history_startup_action(
            Some((10, hash(10))),
            Some((25, hash(25))),
            20,
            Some(hash(10)),
            None,
        )
        .expect("latest ahead of canonical best should wait for the node to catch up");

        assert_eq!(
            action,
            ProofHistoryStartupAction::WaitForCanonicalLatest { latest: 25, canonical_best: 20 }
        );
    }

    #[test]
    fn proof_history_startup_action_waits_when_canonical_has_not_reached_earliest() {
        // A re-synced chain database (with retained proof-history storage) has no header at the
        // stored earliest height yet. Wait for sync instead of failing: the node could never
        // sync past this point if reconciliation kept crashing it.
        let action =
            proof_history_startup_action(Some((10, hash(10))), Some((20, hash(20))), 5, None, None)
                .expect("canonical chain below stored earliest should wait for sync");

        assert_eq!(action, ProofHistoryStartupAction::WaitForCanonicalEarliest { earliest: 10 });
    }

    #[test]
    fn proof_history_startup_action_errors_when_earliest_is_noncanonical() {
        let error = proof_history_startup_action(
            Some((10, hash(11))),
            Some((20, hash(21))),
            20,
            Some(hash(10)),
            Some(hash(20)),
        )
        .expect_err("noncanonical earliest must not be served");

        let message = error.to_string();
        assert!(message.contains("earliest stored block"));
        assert!(message.contains("wipe proof-history storage"));
    }

    #[test]
    fn proof_history_startup_action_errors_when_latest_hash_is_missing_within_canonical_range() {
        // The stored latest height is within the canonical chain, yet the canonical hash lookup
        // returned nothing: a provider inconsistency that waiting cannot repair. Fail closed.
        let error = proof_history_startup_action(
            Some((10, hash(10))),
            Some((20, hash(20))),
            20,
            Some(hash(10)),
            None,
        )
        .expect_err("missing canonical hash below best must fail closed");

        assert!(error.to_string().contains("no canonical hash"));
    }
}
