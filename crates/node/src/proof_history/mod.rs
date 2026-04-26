//! Proof-history sidecar configuration and ExEx wiring for Taiko nodes.

mod config;
mod storage_init;

pub use config::ProofHistoryConfig;
use storage_init::{
    DelayedProofHistoryStart, HistoricalInitMetadata, PROOF_HISTORY_MAX_STARTUP_PRUNE_BLOCKS,
    ProofHistoryInitializationAction, delayed_proof_history_start, finalized_block_number,
    initialize_historical_proof_history_storage, initialize_proof_history_storage,
    proof_history_backfill_target, proof_history_historical_init_metadata_path,
    proof_history_storage_needs_initialization, remove_historical_init_metadata,
    validate_historical_init_metadata_file, write_historical_init_metadata,
};

use crate::TaikoNode;
use alethia_reth_rpc::{
    debug::{TaikoDebugWitnessApiServer, TaikoDebugWitnessExt},
    eth::proofs::{TaikoEthProofApiServer, TaikoEthProofExt},
};
use alloy_consensus::BlockHeader;
use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, U256};
use eyre::{WrapErr, eyre};
use futures_util::TryStreamExt;
use reth::{
    providers::{
        BlockNumReader, BlockReader, DBProvider, DatabaseProviderFactory, HeaderProvider,
        TransactionVariant,
    },
    tasks::TaskExecutor,
};
use reth_db::{
    Database, DatabaseError,
    cursor::{DbCursorRO, DbDupCursorRO},
    database_metrics::DatabaseMetrics,
    table::{DupSort, Table},
    tables,
    transaction::DbTx,
};
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_node_api::{FullNodeComponents, NodeAddOns, NodePrimitives, NodeTypes};
use reth_node_builder::{
    NodeAdapter, NodeBuilderWithComponents, NodeComponentsBuilder, WithLaunchContext,
    rpc::{RethRpcAddOns, RpcContext},
};
use reth_optimism_trie::{
    OpProofStoragePruner, OpProofsStorage, OpProofsStore,
    api::{InitialStateAnchor, InitialStateStatus, OpProofsInitProvider, OpProofsProviderRO},
    db::{HashedStorageKey, MdbxProofsStorage, StorageTrieKey},
    live::LiveTrieCollector,
};
use reth_primitives_traits::Account;
use reth_rpc_builder::RethRpcModule;
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_storage_api::{
    ChainStateBlockReader, ChangeSetReader, StorageChangeSetReader, StorageSettingsCache,
};
use reth_trie::StateRoot;
use reth_trie_common::{
    BranchNodeCompact, HashedPostStateSorted, Nibbles, SortedTrieData, StoredNibbles,
    updates::{StorageTrieUpdatesSorted, TrieUpdatesSorted},
};
use reth_trie_db::{
    DatabaseHashedCursorFactory, DatabaseStateRoot, DatabaseTrieCursorFactory,
    StorageTrieEntryLike, TrieTableAdapter,
};
use std::{
    panic,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Mutex, watch},
    task,
    time::{self, MissedTickBehavior, sleep},
};
use tracing::{debug, error, info};

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

/// Shared storage type used by proof-history indexing and debug RPC overrides.
pub type ProofHistoryStorage = OpProofsStorage<Arc<MdbxProofsStorage>>;

/// Result returned by proof-history installation with the updated node builder and optional
/// storage.
pub type ProofHistoryInstallResult<T, CB, AO> = eyre::Result<(
    WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>,
    Option<ProofHistoryStorage>,
)>;

/// Installs the proof-history ExEx and proof database metrics task on a Taiko node builder.
pub fn install_proof_history<T, CB, AO>(
    node_builder: WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>,
    config: ProofHistoryConfig,
) -> ProofHistoryInstallResult<T, CB, AO>
where
    T: reth_node_api::FullNodeTypes<Types = TaikoNode>,
    CB: NodeComponentsBuilder<T>,
    AO: NodeAddOns<NodeAdapter<T, CB::Components>> + RethRpcAddOns<NodeAdapter<T, CB::Components>>,
    AO::EthApi: FullEthApi + Send + Sync + 'static,
    T::Provider: BlockNumReader + DatabaseProviderFactory,
    <T::Provider as DatabaseProviderFactory>::Provider: BlockNumReader
        + ChainStateBlockReader
        + ChangeSetReader
        + DBProvider
        + HeaderProvider
        + StorageChangeSetReader
        + StorageSettingsCache,
    <T::DB as Database>::TX: Sync,
{
    if !config.enabled {
        return Ok((node_builder, None));
    }

    let storage_path = config.required_storage_path()?.clone();
    let mdbx =
        Arc::new(MdbxProofsStorage::new(&storage_path).wrap_err_with(|| {
            format!("failed to create proof-history MDBX at {storage_path:?}")
        })?);
    let storage: ProofHistoryStorage = Arc::clone(&mdbx).into();
    let storage_for_exex = storage.clone();
    let storage_for_init = Arc::clone(&mdbx);
    let historical_init_metadata_path = proof_history_historical_init_metadata_path(&storage_path);

    Ok((
        node_builder
            .on_node_started(move |node| {
                spawn_proofs_db_metrics(
                    node.task_executor,
                    mdbx,
                    node.config.metrics.push_gateway_interval,
                );
                Ok(())
            })
            .install_exex("proofs-history", async move |exex_context| {
                Ok(ProofHistoryExEx::new(
                    exex_context,
                    storage_for_exex,
                    storage_for_init,
                    ProofHistoryExExConfig {
                        proofs_history_window: config.window,
                        proofs_history_prune_interval: config.prune_interval,
                        verification_interval: config.verification_interval,
                        backfill_window_only: config.backfill_window_only,
                        historical_init_metadata_path: Some(historical_init_metadata_path),
                    },
                )
                .run())
            }),
        Some(storage),
    ))
}

/// Number of blocks the proof-history sync task executes in one batch.
const PROOF_HISTORY_SYNC_BATCH_SIZE: usize = 50;

/// Distance from canonical tip where proof-history can process notification data directly.
const PROOF_HISTORY_REAL_TIME_BLOCKS_THRESHOLD: u64 = 1024;

/// Delay used when proof-history has no locally executable backfill work.
const PROOF_HISTORY_SYNC_IDLE_SLEEP: Duration = Duration::from_secs(5);

/// Delay used while waiting for delayed proof-history initialization to become possible.
const PROOF_HISTORY_DELAYED_START_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Number of proof-history blocks pruned in one pruning transaction.
const PROOF_HISTORY_PRUNE_BATCH_SIZE: u64 = 200;

/// Runtime settings passed into the proof-history ExEx.
#[derive(Debug)]
struct ProofHistoryExExConfig {
    /// Number of recent blocks retained in proof-history storage.
    proofs_history_window: u64,
    /// Wall-clock interval between proof-history prune passes.
    proofs_history_prune_interval: Duration,
    /// Block interval between full execution verification checks.
    verification_interval: u64,
    /// Whether empty proof-history storage waits for the finalized retention window.
    backfill_window_only: bool,
    /// Sidecar file that records historical initialization target metadata.
    historical_init_metadata_path: Option<PathBuf>,
}

/// Taiko proof-history ExEx that keeps OP proofs storage behind locally executed state.
#[derive(Debug)]
struct ProofHistoryExEx<Node, Storage>
where
    Node: FullNodeComponents,
{
    /// Reth ExEx context used to receive chain notifications and report finished heights.
    ctx: ExExContext<Node>,
    /// Proof-history storage populated by the extension.
    storage: OpProofsStorage<Storage>,
    /// Raw proof-history storage handle used for the initial current-state snapshot.
    init_storage: Storage,
    /// Runtime settings that govern proof-history retention and startup behavior.
    config: ProofHistoryExExConfig,
    /// Whether a delayed-start miss has already been reported for this ExEx run.
    missed_start_logged: AtomicBool,
    /// Serializes proof-history writers across live notifications, background sync, and pruning.
    write_lock: Arc<Mutex<()>>,
}

impl<Node, Storage> ProofHistoryExEx<Node, Storage>
where
    Node: FullNodeComponents,
{
    /// Creates a proof-history ExEx with Taiko backfill guards.
    fn new(
        ctx: ExExContext<Node>,
        storage: OpProofsStorage<Storage>,
        init_storage: Storage,
        config: ProofHistoryExExConfig,
    ) -> Self {
        Self {
            ctx,
            storage,
            init_storage,
            config,
            missed_start_logged: AtomicBool::new(false),
            write_lock: Arc::new(Mutex::new(())),
        }
    }
}

impl<Node, Storage, Primitives> ProofHistoryExEx<Node, Storage>
where
    Node: FullNodeComponents<Types: NodeTypes<Primitives = Primitives>>,
    Node::Provider: BlockNumReader + DatabaseProviderFactory,
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
    async fn run(mut self) -> eyre::Result<()> {
        let mut sync_target_tx = self.try_start()?;
        let mut retry_interval = time::interval(PROOF_HISTORY_DELAYED_START_RETRY_INTERVAL);
        retry_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let collector = LiveTrieCollector::new(
            self.ctx.evm_config().clone(),
            self.ctx.provider().clone(),
            &self.storage,
        );

        loop {
            tokio::select! {
                maybe_notification = self.ctx.notifications.try_next() => {
                    let Some(notification) = maybe_notification? else {
                        break;
                    };

                    if sync_target_tx.is_none() {
                        sync_target_tx = self.try_start()?;
                        if sync_target_tx.is_none() {
                            self.acknowledge_notification(&notification)?;
                            continue;
                        }
                    }

                    self.handle_notification(
                        notification,
                        &collector,
                        sync_target_tx.as_ref().expect("initialized proof-history ExEx has a sync target"),
                    )
                    .await?;
                }
                _ = retry_interval.tick(), if sync_target_tx.is_none() => {
                    sync_target_tx = self.try_start()?;
                }
            }
        }

        Ok(())
    }

    /// Initializes storage if possible and spawns the sync and pruner tasks on first success.
    fn try_start(&self) -> eyre::Result<Option<watch::Sender<u64>>> {
        if !self.initialize_or_wait()? {
            return Ok(None);
        }
        let initial_sync_target = self.ctx.provider().best_block_number()?;
        let sync_target_tx = self.spawn_sync_task(initial_sync_target);
        self.spawn_pruner_task();
        Ok(Some(sync_target_tx))
    }

    /// Initializes proof-history storage immediately or waits for the finalized window.
    fn initialize_or_wait(&self) -> eyre::Result<bool> {
        if proof_history_storage_needs_initialization(&self.storage)? {
            let action = if self.config.backfill_window_only {
                self.finalized_window_initialization_action()?
            } else {
                ProofHistoryInitializationAction::CurrentState
            };

            match action {
                ProofHistoryInitializationAction::Wait => return Ok(false),
                ProofHistoryInitializationAction::CurrentState => initialize_proof_history_storage(
                    self.ctx.provider(),
                    self.init_storage.clone(),
                )?,
                ProofHistoryInitializationAction::HistoricalWindow {
                    start_block,
                    target_block,
                } => initialize_historical_proof_history_storage(
                    self.ctx.provider(),
                    self.init_storage.clone(),
                    self.config.historical_init_metadata_path.as_deref(),
                    start_block,
                    target_block,
                )?,
            }
        }
        self.ensure_initialized()?;
        Ok(true)
    }

    /// Returns how empty storage should initialize for a finalized proof-history window.
    fn finalized_window_initialization_action(
        &self,
    ) -> eyre::Result<ProofHistoryInitializationAction> {
        let finalized_block = finalized_block_number(self.ctx.provider())?;
        let executed_head = self.ctx.provider().best_block_number()?;

        match delayed_proof_history_start(
            finalized_block,
            executed_head,
            self.config.proofs_history_window,
        ) {
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
                if self.missed_start_logged.swap(true, Ordering::Relaxed) {
                    debug!(
                        target: "reth::taiko::proof_history",
                        ?finalized_block,
                        executed_head,
                        start_block,
                        "waiting for proof-history window start to match local execution"
                    );
                } else {
                    info!(
                        target: "reth::taiko::proof_history",
                        ?finalized_block,
                        executed_head,
                        start_block,
                        "empty proof-history storage missed the finalized window start; building historical proof-history anchor"
                    );
                }
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
        let earliest_block_number = provider_ro
            .get_earliest_block_number()?
            .ok_or_else(|| eyre!("proof-history storage is not initialized"))?
            .0;
        let latest_block_number = provider_ro
            .get_latest_block_number()?
            .ok_or_else(|| eyre!("proof-history storage is not initialized"))?
            .0;

        let target_earliest = latest_block_number.saturating_sub(self.config.proofs_history_window);
        if target_earliest > earliest_block_number {
            let blocks_to_prune = target_earliest - earliest_block_number;
            if blocks_to_prune > PROOF_HISTORY_MAX_STARTUP_PRUNE_BLOCKS {
                return Err(eyre!(
                    "configuration requires pruning {} proof-history blocks, which exceeds the safety threshold of {}",
                    blocks_to_prune,
                    PROOF_HISTORY_MAX_STARTUP_PRUNE_BLOCKS
                ));
            }
        }

        Ok(())
    }

    /// Acknowledges committed ExEx notifications without proof-history processing.
    fn acknowledge_notification(
        &self,
        notification: &ExExNotification<Primitives>,
    ) -> eyre::Result<()> {
        if let Some(committed_chain) = notification.committed_chain() {
            self.ctx.events.send(ExExEvent::FinishedHeight(committed_chain.tip().num_hash()))?;
        }

        Ok(())
    }

    /// Spawns the periodic proof-history pruning task.
    fn spawn_pruner_task(&self) {
        let pruner = Arc::new(OpProofStoragePruner::new(
            self.storage.clone(),
            self.ctx.provider().clone(),
            self.config.proofs_history_window,
            PROOF_HISTORY_PRUNE_BATCH_SIZE,
        ));
        let prune_interval = self.config.proofs_history_prune_interval;
        let retention_window = self.config.proofs_history_window;
        let write_lock = self.write_lock.clone();

        self.ctx
            .task_executor()
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
                                    result = &mut prune_task => {
                                        if let Err(error) = blocking_join_result(result, "proof-history pruner worker") {
                                            error!(
                                                target: "reth::taiko::proof_history",
                                                ?error,
                                                "proof-history pruner task failed to join blocking worker"
                                            );
                                        }
                                    }
                                    _ = &mut signal => {
                                        // `spawn_blocking` workers cannot be aborted, so wait for
                                        // the prune to finish to avoid tearing down a write txn
                                        // mid-flight. A deeper fix would need a cancel-aware
                                        // pruner API or smaller prune chunks.
                                        info!(
                                            target: "reth::taiko::proof_history",
                                            "shutdown requested while proof-history prune is running; waiting for prune to finish"
                                        );
                                        if let Err(error) = blocking_join_result(prune_task.await, "proof-history pruner worker") {
                                            error!(
                                                target: "reth::taiko::proof_history",
                                                ?error,
                                                "proof-history pruner task failed to join blocking worker during shutdown"
                                            );
                                        }
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

    /// Spawns the guarded proof-history backfill task.
    fn spawn_sync_task(&self, initial_sync_target: u64) -> watch::Sender<u64> {
        let (sync_target_tx, sync_target_rx) = watch::channel(initial_sync_target);
        let task_storage = self.storage.clone();
        let task_provider = self.ctx.provider().clone();
        let task_evm_config = self.ctx.evm_config().clone();
        let task_write_lock = self.write_lock.clone();

        self.ctx.task_executor().spawn_critical_task(
            "taiko::proof_history::sync_loop",
            async move {
                Self::sync_loop(
                    sync_target_rx,
                    task_storage,
                    task_provider,
                    task_evm_config,
                    task_write_lock,
                )
                .await;
            },
        );

        sync_target_tx
    }

    /// Backfills proof-history only through blocks the node has locally executed.
    async fn sync_loop(
        mut sync_target_rx: watch::Receiver<u64>,
        storage: OpProofsStorage<Storage>,
        provider: Node::Provider,
        evm_config: Node::Evm,
        write_lock: Arc<Mutex<()>>,
    ) {
        debug!(target: "reth::taiko::proof_history", "starting proof-history sync loop");

        loop {
            let requested_target = *sync_target_rx.borrow_and_update();
            let write_guard = write_lock.lock().await;
            let latest = match storage.provider_ro().and_then(|p| p.get_latest_block_number()) {
                Ok(Some((number, _))) => number,
                Ok(None) => {
                    error!(target: "reth::taiko::proof_history", "proof-history sync loop found no stored blocks; stopping sync loop");
                    return;
                }
                Err(error) => {
                    error!(target: "reth::taiko::proof_history", ?error, "failed to read proof-history latest block");
                    drop(write_guard);
                    time::sleep(PROOF_HISTORY_SYNC_IDLE_SLEEP).await;
                    continue;
                }
            };

            if latest >= requested_target {
                drop(write_guard);
                if sync_target_rx.changed().await.is_err() {
                    debug!(
                        target: "reth::taiko::proof_history",
                        "proof-history sync target sender dropped; stopping sync loop"
                    );
                    return;
                }
                continue;
            }

            let executed_head = match provider.best_block_number() {
                Ok(number) => number,
                Err(error) => {
                    error!(target: "reth::taiko::proof_history", ?error, "failed to read executed head for proof-history sync");
                    drop(write_guard);
                    time::sleep(PROOF_HISTORY_SYNC_IDLE_SLEEP).await;
                    continue;
                }
            };

            let Some(target) =
                proof_history_backfill_target(latest, requested_target, executed_head)
            else {
                debug!(
                    target: "reth::taiko::proof_history",
                    latest,
                    requested_target,
                    executed_head,
                    "proof-history sync waiting for local execution"
                );
                drop(write_guard);
                time::sleep(PROOF_HISTORY_SYNC_IDLE_SLEEP).await;
                continue;
            };

            let batch_provider = provider.clone();
            let batch_storage = storage.clone();
            let batch_evm_config = evm_config.clone();
            // Each block write commits independently; if this batch fails part-way through, the
            // next loop rereads `latest` and resumes after the last committed block.
            let batch_task = task::spawn_blocking(move || {
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
            let batch_result = blocking_join_result(batch_task.await, "proof-history batch worker")
                .and_then(|result| result);
            drop(write_guard);

            if let Err(error) = batch_result {
                error!(target: "reth::taiko::proof_history", ?error, "proof-history batch processing failed");
                time::sleep(PROOF_HISTORY_SYNC_IDLE_SLEEP).await;
            }

            task::yield_now().await;
        }
    }

    /// Processes a bounded batch of canonical blocks into proof-history storage.
    fn process_batch(
        start: u64,
        target: u64,
        provider: &Node::Provider,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
        batch_size: usize,
    ) -> eyre::Result<()> {
        let end = (start + batch_size as u64).min(target);
        debug!(target: "reth::taiko::proof_history", start, end, "processing proof-history batch");

        for block_num in (start + 1)..=end {
            let block = provider
                .recovered_block(block_num.into(), TransactionVariant::NoHash)?
                .ok_or_else(|| eyre!("missing block {block_num}"))?;
            collector.execute_and_store_block_updates(&block)?;
        }

        Ok(())
    }

    /// Handles an ExEx notification and advances proof-history storage or its backfill target.
    async fn handle_notification(
        &self,
        notification: ExExNotification<Primitives>,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
        sync_target_tx: &watch::Sender<u64>,
    ) -> eyre::Result<()> {
        let _write_guard = self.write_lock.lock().await;
        let latest_stored = self
            .storage
            .provider_ro()?
            .get_latest_block_number()?
            .ok_or_else(|| eyre!("no blocks stored in proof-history storage"))?
            .0;

        match &notification {
            ExExNotification::ChainCommitted { new } => {
                self.handle_chain_committed(new.clone(), latest_stored, collector, sync_target_tx)?
            }
            ExExNotification::ChainReorged { old, new } => {
                self.handle_chain_reorged(old.clone(), new.clone(), latest_stored, collector)?
            }
            ExExNotification::ChainReverted { old } => {
                self.handle_chain_reverted(old.clone(), latest_stored, collector)?
            }
        }

        self.acknowledge_notification(&notification)?;

        Ok(())
    }

    /// Handles a canonical chain commit notification.
    fn handle_chain_committed(
        &self,
        new: Arc<Chain<Primitives>>,
        latest_stored: u64,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
        sync_target_tx: &watch::Sender<u64>,
    ) -> eyre::Result<()> {
        if new.tip().number() <= latest_stored {
            return Ok(());
        }

        let best_block = self.ctx.provider().best_block_number()?;
        let is_sequential = new.tip().number() == latest_stored + 1;
        let is_near_tip = best_block.saturating_sub(new.tip().number()) <
            PROOF_HISTORY_REAL_TIME_BLOCKS_THRESHOLD;

        if is_sequential && is_near_tip {
            for block_number in latest_stored.saturating_add(1)..=new.tip().number() {
                self.process_block(block_number, &new, collector)?;
            }
        } else {
            sync_target_tx.send(new.tip().number())?;
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

        if let Some(block) = chain.blocks().get(&block_number) &&
            let Some((trie_updates, hashed_state)) = chain.trie_data_at(block_number).map(|d| {
                let SortedTrieData { hashed_state, trie_updates } = d.get();
                (trie_updates, hashed_state)
            }) &&
            !should_verify
        {
            collector.store_block_updates(
                block.block_with_parent(),
                (**trie_updates).clone(),
                (**hashed_state).clone(),
            )?;
            return Ok(());
        }

        let block = self
            .ctx
            .provider()
            .recovered_block(block_number.into(), TransactionVariant::NoHash)?
            .ok_or_else(|| eyre!("missing block {block_number} in provider"))?;
        collector.execute_and_store_block_updates(&block)?;
        Ok(())
    }

    /// Handles a canonical chain reorg notification.
    fn handle_chain_reorged(
        &self,
        old: Arc<Chain<Primitives>>,
        new: Arc<Chain<Primitives>>,
        latest_stored: u64,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
    ) -> eyre::Result<()> {
        if old.first().number() > latest_stored {
            return Ok(());
        }

        let mut block_updates: Vec<(
            BlockWithParent,
            Arc<TrieUpdatesSorted>,
            Arc<HashedPostStateSorted>,
        )> = Vec::with_capacity(new.len());

        for block_number in new.blocks().keys() {
            if old.fork_block() != new.fork_block() {
                return Err(eyre!(
                    "proof-history fork blocks do not match: old={:?}, new={:?}",
                    old.fork_block(),
                    new.fork_block()
                ));
            }

            if let Some(block) = new.blocks().get(block_number) &&
                let Some(trie_data) = new.trie_data_at(*block_number)
            {
                let SortedTrieData { hashed_state, trie_updates } = trie_data.get();
                block_updates.push((
                    block.block_with_parent(),
                    trie_updates.clone(),
                    hashed_state.clone(),
                ));
                continue;
            }

            self.process_block(*block_number, &new, collector)?;
        }

        if !block_updates.is_empty() {
            collector.unwind_and_store_block_updates(block_updates)?;
        }

        Ok(())
    }

    /// Handles a canonical chain revert notification.
    fn handle_chain_reverted(
        &self,
        old: Arc<Chain<Primitives>>,
        latest_stored: u64,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
    ) -> eyre::Result<()> {
        if old.first().number() > latest_stored {
            return Ok(());
        }

        collector.unwind_history(old.first().block_with_parent())?;
        Ok(())
    }
}

/// Batch size used while copying the current node state into proof-history storage.
const PROOF_HISTORY_INITIALIZATION_BATCH_SIZE: usize = 100_000;

/// Historical overlay used to rewrite the current node state into an older anchor state.
#[derive(Debug)]
struct HistoricalAnchorOverlay {
    /// Account and storage leaf changes that rewind current state to the anchor.
    hashed_state: HashedPostStateSorted,
    /// Account and storage branch changes that rewind current tries to the anchor.
    trie_updates: TrieUpdatesSorted,
}

impl HistoricalAnchorOverlay {
    /// Returns whether a current hashed account row must be replaced by the overlay.
    fn has_account_update(&self, hashed_address: &B256) -> bool {
        self.hashed_state.accounts.binary_search_by_key(hashed_address, |(key, _)| *key).is_ok()
    }

    /// Returns whether a current hashed storage row must be replaced by the overlay.
    fn has_storage_update(&self, hashed_address: &B256, hashed_slot: &B256) -> bool {
        self.hashed_state.storages.get(hashed_address).is_some_and(|storage| {
            storage.is_wiped() ||
                storage
                    .storage_slots_ref()
                    .binary_search_by_key(hashed_slot, |(key, _)| *key)
                    .is_ok()
        })
    }

    /// Returns whether a current account trie branch must be replaced by the overlay.
    fn has_account_trie_update(&self, path: &Nibbles) -> bool {
        self.trie_updates.account_nodes_ref().binary_search_by_key(path, |(key, _)| *key).is_ok()
    }

    /// Returns whether a current storage trie branch must be replaced by the overlay.
    fn has_storage_trie_update(&self, hashed_address: &B256, path: &Nibbles) -> bool {
        self.trie_updates
            .storage_tries_ref()
            .get(hashed_address)
            .is_some_and(|storage| storage_trie_has_path_update(storage, path))
    }
}

/// Returns whether a sorted storage trie overlay replaces a given path.
fn storage_trie_has_path_update(storage: &StorageTrieUpdatesSorted, path: &Nibbles) -> bool {
    storage.is_deleted() ||
        storage.storage_nodes_ref().binary_search_by_key(path, |(key, _)| *key).is_ok()
}

/// Copies the node's current trie and hashed-state tables into proof-history storage.
#[derive(Debug)]
pub(super) struct ProofHistoryInitializationJob<Tx, Storage> {
    /// Read-only transaction opened against the node's primary database.
    tx: Tx,
    /// Destination proof-history storage initialized by this job.
    storage: Storage,
}

impl<Tx, Storage> ProofHistoryInitializationJob<Tx, Storage>
where
    Tx: DbTx + Sync,
    Storage: OpProofsStore + Send,
{
    /// Creates a proof-history initialization job over a source database transaction.
    pub(super) const fn new(storage: Storage, tx: Tx) -> Self {
        Self { tx, storage }
    }

    /// Runs initialization using the trie key encoding selected by the node's storage settings.
    pub(super) fn run_with_adapter<Adapter>(
        &self,
        best_number: u64,
        best_hash: B256,
    ) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        let Some(anchor) = self.prepare_anchor(BlockNumHash::new(best_number, best_hash))? else {
            return Ok(());
        };

        self.initialize_hashed_accounts(anchor.latest_hashed_account_key)?;
        self.initialize_hashed_storages(anchor.latest_hashed_storage_key)?;
        self.initialize_storage_trie::<Adapter>(anchor.latest_storage_trie_key)?;
        self.initialize_account_trie::<Adapter>(anchor.latest_account_trie_key)?;

        self.commit_initial_state()
    }

    /// Runs initialization after rewriting current node tables into a historical anchor state.
    pub(super) fn run_historical_with_adapter<Adapter>(
        &self,
        block: BlockNumHash,
        target_block: BlockNumHash,
        state_root: B256,
        hashed_state: HashedPostStateSorted,
        metadata_path: Option<&Path>,
    ) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        let (computed_root, trie_updates) = <StateRoot<
            DatabaseTrieCursorFactory<&Tx, Adapter>,
            DatabaseHashedCursorFactory<&Tx>,
        > as DatabaseStateRoot<'_, Tx>>::overlay_root_with_updates(
            &self.tx, &hashed_state
        )?;

        if computed_root != state_root {
            return Err(eyre!(
                "historical proof-history anchor state root mismatch at block {}: computed={computed_root:?} expected={state_root:?}",
                block.number
            ));
        }

        let overlay =
            HistoricalAnchorOverlay { hashed_state, trie_updates: trie_updates.into_sorted() };

        let metadata = HistoricalInitMetadata { start_block: block, target_block };
        let Some(anchor) = self.prepare_historical_anchor(block, metadata_path, metadata)? else {
            return Ok(());
        };

        self.initialize_hashed_accounts_with_overlay(anchor.latest_hashed_account_key, &overlay)?;
        self.initialize_hashed_storages_with_overlay(anchor.latest_hashed_storage_key, &overlay)?;
        self.initialize_storage_trie_with_overlay::<Adapter>(
            anchor.latest_storage_trie_key,
            &overlay,
        )?;
        self.initialize_account_trie_with_overlay::<Adapter>(
            anchor.latest_account_trie_key,
            &overlay,
        )?;

        self.commit_initial_state()?;
        if let Some(path) = metadata_path &&
            let Err(error) = remove_historical_init_metadata(path)
        {
            error!(
                target: "reth::taiko::proof_history",
                ?error,
                metadata_path = ?path,
                "failed to remove historical proof-history initialization metadata"
            );
        }
        Ok(())
    }

    /// Loads or initializes the proof-history anchor for `block`, returning `None` when
    /// initialization is already completed.
    fn prepare_anchor(&self, block: BlockNumHash) -> eyre::Result<Option<InitialStateAnchor>> {
        let init_provider = self.storage.initialization_provider()?;
        let anchor = init_provider.initial_state_anchor()?;

        match anchor.status {
            InitialStateStatus::Completed => Ok(None),
            InitialStateStatus::NotStarted => {
                init_provider.set_initial_state_anchor(block)?;
                OpProofsInitProvider::commit(init_provider)?;
                Ok(Some(anchor))
            }
            InitialStateStatus::InProgress => {
                self.validate_anchor_block(&anchor, block.number, block.hash)?;
                drop(init_provider);
                Ok(Some(anchor))
            }
        }
    }

    /// Loads or initializes a historical proof-history anchor and validates the resume target.
    fn prepare_historical_anchor(
        &self,
        block: BlockNumHash,
        metadata_path: Option<&Path>,
        expected_metadata: HistoricalInitMetadata,
    ) -> eyre::Result<Option<InitialStateAnchor>> {
        let init_provider = self.storage.initialization_provider()?;
        let anchor = init_provider.initial_state_anchor()?;

        match anchor.status {
            InitialStateStatus::Completed => Ok(None),
            InitialStateStatus::NotStarted => {
                if let Some(path) = metadata_path {
                    write_historical_init_metadata(path, expected_metadata)?;
                }
                init_provider.set_initial_state_anchor(block)?;
                OpProofsInitProvider::commit(init_provider)?;
                Ok(Some(anchor))
            }
            InitialStateStatus::InProgress => {
                self.validate_anchor_block(&anchor, block.number, block.hash)?;
                self.validate_historical_init_metadata(metadata_path, expected_metadata)?;
                drop(init_provider);
                Ok(Some(anchor))
            }
        }
    }

    /// Validates that historical initialization is resuming the same source state.
    fn validate_historical_init_metadata(
        &self,
        metadata_path: Option<&Path>,
        expected_metadata: HistoricalInitMetadata,
    ) -> eyre::Result<()> {
        validate_historical_init_metadata_file(metadata_path, expected_metadata)
    }

    /// Marks proof-history initialization as completed and commits.
    fn commit_initial_state(&self) -> eyre::Result<()> {
        let init_provider = self.storage.initialization_provider()?;
        init_provider.commit_initial_state()?;
        OpProofsInitProvider::commit(init_provider)?;
        Ok(())
    }

    /// Validates that a resumed initialization still targets the same source block.
    fn validate_anchor_block(
        &self,
        anchor: &InitialStateAnchor,
        best_number: u64,
        best_hash: B256,
    ) -> eyre::Result<()> {
        let Some(block) = anchor.block else {
            return Err(eyre!("proof-history initialization anchor is missing its source block"));
        };

        if block.number != best_number || block.hash != best_hash {
            return Err(eyre!(
                "proof-history initialization anchor mismatch: stored=({:?}, {:?}) current=({:?}, {:?})",
                block.number,
                block.hash,
                best_number,
                best_hash
            ));
        }

        Ok(())
    }

    /// Copies hashed account leaves from the node database into proof-history storage.
    fn initialize_hashed_accounts(&self, start_key: Option<B256>) -> eyre::Result<()> {
        self.initialize_hashed_accounts_filtered(start_key, |_| true)
    }

    /// Copies historical hashed account leaves, replacing current rows with overlay rows.
    fn initialize_hashed_accounts_with_overlay(
        &self,
        start_key: Option<B256>,
        overlay: &HistoricalAnchorOverlay,
    ) -> eyre::Result<()> {
        self.initialize_hashed_accounts_filtered(start_key, |hashed_address| {
            !overlay.has_account_update(hashed_address)
        })?;
        self.store_hashed_accounts(overlay.hashed_state.accounts.clone())
    }

    /// Copies hashed account leaves that satisfy `include`.
    fn initialize_hashed_accounts_filtered(
        &self,
        start_key: Option<B256>,
        include: impl Fn(&B256) -> bool,
    ) -> eyre::Result<()> {
        let mut cursor = self.tx.cursor_read::<tables::HashedAccounts>()?;

        if let Some(latest_key) = start_key {
            cursor.seek(latest_key)?.filter(|(key, _)| *key == latest_key).ok_or_else(|| {
                eyre!("proof-history initialization resume key missing in HashedAccounts")
            })?;
        }

        let mut batch = Vec::with_capacity(PROOF_HISTORY_INITIALIZATION_BATCH_SIZE);
        while let Some((hashed_address, account)) = cursor.next()? {
            if !include(&hashed_address) {
                continue;
            }

            batch.push((hashed_address, Some(account)));
            if batch.len() >= PROOF_HISTORY_INITIALIZATION_BATCH_SIZE {
                self.store_hashed_accounts(std::mem::take(&mut batch))?;
            }
        }
        self.store_hashed_accounts(batch)
    }

    /// Copies hashed storage leaves from the node database into proof-history storage.
    fn initialize_hashed_storages(&self, start_key: Option<HashedStorageKey>) -> eyre::Result<()> {
        self.initialize_hashed_storages_filtered(start_key, |_, _| true)
    }

    /// Copies historical hashed storage leaves, replacing current rows with overlay rows.
    fn initialize_hashed_storages_with_overlay(
        &self,
        start_key: Option<HashedStorageKey>,
        overlay: &HistoricalAnchorOverlay,
    ) -> eyre::Result<()> {
        self.initialize_hashed_storages_filtered(start_key, |hashed_address, hashed_slot| {
            !overlay.has_storage_update(hashed_address, hashed_slot)
        })?;

        for (hashed_address, storage) in &overlay.hashed_state.storages {
            self.store_hashed_storages(*hashed_address, storage.storage_slots_ref().to_vec())?;
        }

        Ok(())
    }

    /// Copies hashed storage leaves that satisfy `include`.
    fn initialize_hashed_storages_filtered(
        &self,
        start_key: Option<HashedStorageKey>,
        include: impl Fn(&B256, &B256) -> bool,
    ) -> eyre::Result<()> {
        let mut cursor = self.tx.cursor_dup_read::<tables::HashedStorages>()?;

        if let Some(latest_key) = start_key {
            cursor
                .seek_by_key_subkey(latest_key.hashed_address, latest_key.hashed_storage_key)?
                .filter(|storage| storage.key == latest_key.hashed_storage_key)
                .ok_or_else(|| {
                    eyre!("proof-history initialization resume key missing in HashedStorages")
                })?;
        }

        let mut current_address = None;
        let mut current_slots = Vec::with_capacity(PROOF_HISTORY_INITIALIZATION_BATCH_SIZE);
        while let Some((hashed_address, storage)) =
            next_dup_entry::<tables::HashedStorages, _>(&mut cursor)?
        {
            if !include(&hashed_address, &storage.key) {
                continue;
            }

            if current_address.is_some_and(|address| address != hashed_address) ||
                current_slots.len() >= PROOF_HISTORY_INITIALIZATION_BATCH_SIZE
            {
                let address =
                    current_address.expect("storage batch is only populated after address is set");
                self.store_hashed_storages(address, std::mem::take(&mut current_slots))?;
            }

            current_address = Some(hashed_address);
            current_slots.push((storage.key, storage.value));
        }

        if let Some(address) = current_address {
            self.store_hashed_storages(address, current_slots)?;
        }

        Ok(())
    }

    /// Copies account trie branches from the node database into proof-history storage.
    fn initialize_account_trie<Adapter>(&self, start_key: Option<StoredNibbles>) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        self.initialize_account_trie_filtered::<Adapter>(start_key, |_| true)
    }

    /// Copies historical account trie branches, replacing current rows with overlay rows.
    fn initialize_account_trie_with_overlay<Adapter>(
        &self,
        start_key: Option<StoredNibbles>,
        overlay: &HistoricalAnchorOverlay,
    ) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        self.initialize_account_trie_filtered::<Adapter>(start_key, |path| {
            !overlay.has_account_trie_update(path)
        })?;
        self.store_account_branches(overlay.trie_updates.account_nodes_ref().to_vec())
    }

    /// Copies account trie branches that satisfy `include`.
    fn initialize_account_trie_filtered<Adapter>(
        &self,
        start_key: Option<StoredNibbles>,
        include: impl Fn(&Nibbles) -> bool,
    ) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        let mut cursor = self.tx.cursor_read::<Adapter::AccountTrieTable>()?;

        if let Some(latest_key) = start_key {
            let adapter_key = Adapter::AccountKey::from(latest_key.0);
            cursor.seek(adapter_key.clone())?.filter(|(key, _)| *key == adapter_key).ok_or_else(
                || eyre!("proof-history initialization resume key missing in account trie"),
            )?;
        }

        let mut batch = Vec::with_capacity(PROOF_HISTORY_INITIALIZATION_BATCH_SIZE);
        while let Some((path, branch)) = cursor.next()? {
            let nibbles = Adapter::account_key_to_nibbles(&path);
            if !include(&nibbles) {
                continue;
            }

            batch.push((nibbles, Some(branch)));
            if batch.len() >= PROOF_HISTORY_INITIALIZATION_BATCH_SIZE {
                self.store_account_branches(std::mem::take(&mut batch))?;
            }
        }
        self.store_account_branches(batch)
    }

    /// Copies storage trie branches from the node database into proof-history storage.
    fn initialize_storage_trie<Adapter>(
        &self,
        start_key: Option<StorageTrieKey>,
    ) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        self.initialize_storage_trie_filtered::<Adapter>(start_key, |_, _| true)
    }

    /// Copies historical storage trie branches, replacing current rows with overlay rows.
    fn initialize_storage_trie_with_overlay<Adapter>(
        &self,
        start_key: Option<StorageTrieKey>,
        overlay: &HistoricalAnchorOverlay,
    ) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        self.initialize_storage_trie_filtered::<Adapter>(start_key, |hashed_address, path| {
            !overlay.has_storage_trie_update(hashed_address, path)
        })?;

        for (hashed_address, storage) in overlay.trie_updates.storage_tries_ref() {
            self.store_storage_branches(*hashed_address, storage.storage_nodes_ref().to_vec())?;
        }

        Ok(())
    }

    /// Copies storage trie branches that satisfy `include`.
    fn initialize_storage_trie_filtered<Adapter>(
        &self,
        start_key: Option<StorageTrieKey>,
        include: impl Fn(&B256, &Nibbles) -> bool,
    ) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        let mut cursor = self.tx.cursor_dup_read::<Adapter::StorageTrieTable>()?;

        if let Some(latest_key) = start_key {
            let adapter_subkey = Adapter::StorageSubKey::from(latest_key.path.0);
            cursor
                .seek_by_key_subkey(latest_key.hashed_address, adapter_subkey.clone())?
                .filter(|entry| *entry.nibbles() == adapter_subkey)
                .ok_or_else(|| {
                    eyre!("proof-history initialization resume key missing in storage trie")
                })?;
        }

        let mut current_address = None;
        let mut current_nodes = Vec::with_capacity(PROOF_HISTORY_INITIALIZATION_BATCH_SIZE);
        while let Some((hashed_address, entry)) =
            next_dup_entry::<Adapter::StorageTrieTable, _>(&mut cursor)?
        {
            let (subkey, branch) = entry.into_parts();
            let nibbles = Adapter::subkey_to_nibbles(&subkey);
            if !include(&hashed_address, &nibbles) {
                continue;
            }

            if current_address.is_some_and(|address| address != hashed_address) ||
                current_nodes.len() >= PROOF_HISTORY_INITIALIZATION_BATCH_SIZE
            {
                let address = current_address
                    .expect("storage trie batch is only populated after address is set");
                self.store_storage_branches(address, std::mem::take(&mut current_nodes))?;
            }

            current_address = Some(hashed_address);
            current_nodes.push((nibbles, Some(branch)));
        }

        if let Some(address) = current_address {
            self.store_storage_branches(address, current_nodes)?;
        }

        Ok(())
    }

    /// Stores a batch of hashed account leaves and commits the proof-history transaction.
    fn store_hashed_accounts(&self, batch: Vec<(B256, Option<Account>)>) -> eyre::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        let provider = self.storage.initialization_provider()?;
        provider.store_hashed_accounts(batch)?;
        OpProofsInitProvider::commit(provider)?;
        Ok(())
    }

    /// Stores a batch of hashed storage leaves for one account and commits it.
    fn store_hashed_storages(
        &self,
        hashed_address: B256,
        batch: Vec<(B256, U256)>,
    ) -> eyre::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        let provider = self.storage.initialization_provider()?;
        provider.store_hashed_storages(hashed_address, batch)?;
        OpProofsInitProvider::commit(provider)?;
        Ok(())
    }

    /// Stores a batch of account trie branches and commits the proof-history transaction.
    fn store_account_branches(
        &self,
        batch: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> eyre::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        let provider = self.storage.initialization_provider()?;
        provider.store_account_branches(batch)?;
        OpProofsInitProvider::commit(provider)?;
        Ok(())
    }

    /// Stores a batch of storage trie branches for one account and commits it.
    fn store_storage_branches(
        &self,
        hashed_address: B256,
        batch: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> eyre::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        let provider = self.storage.initialization_provider()?;
        provider.store_storage_branches(hashed_address, batch)?;
        OpProofsInitProvider::commit(provider)?;
        Ok(())
    }
}

/// Result type returned while iterating duplicate-sorted source tables.
type DupEntryResult<T> = Result<Option<(<T as Table>::Key, <T as Table>::Value)>, DatabaseError>;

/// Returns the next duplicate-sorted table entry, advancing to the next key when needed.
fn next_dup_entry<T, C>(cursor: &mut C) -> DupEntryResult<T>
where
    T: DupSort,
    C: DbCursorRO<T> + DbDupCursorRO<T>,
{
    if let Some(entry) = cursor.next_dup()? {
        return Ok(Some(entry));
    }

    let Some((next_key, _)) = cursor.next_no_dup()? else {
        return Ok(None);
    };

    cursor.seek(next_key)
}

/// Installs proof-history backed replacements for configured `eth_` and `debug_` RPC methods.
pub fn install_proof_history_rpc<Node, EthApi>(
    ctx: &mut RpcContext<'_, Node, EthApi>,
    storage: ProofHistoryStorage,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    EthApi: FullEthApi + Send + Sync + 'static,
    Node::Provider:
        HeaderProvider<Header = reth::primitives::Header> + Clone + Send + Sync + 'static,
{
    let eth_ext = TaikoEthProofExt::new(ctx.registry.eth_api().clone(), storage.clone());
    ctx.modules.add_or_replace_if_module_configured(RethRpcModule::Eth, eth_ext.into_rpc())?;

    let debug_ext = TaikoDebugWitnessExt::new(
        ctx.node().provider().clone(),
        ctx.registry.eth_api().clone(),
        storage,
    );
    ctx.modules.add_or_replace_if_module_configured(RethRpcModule::Debug, debug_ext.into_rpc())?;
    Ok(())
}

/// Spawns periodic metric collection for the proof-history MDBX database.
fn spawn_proofs_db_metrics(
    executor: TaskExecutor,
    storage: Arc<MdbxProofsStorage>,
    metrics_report_interval: Duration,
) {
    executor.spawn_critical_task("taiko-proofs-storage-metrics", async move {
        info!(
            target: "reth::taiko::proof_history",
            ?metrics_report_interval,
            "starting proof-history metrics task"
        );

        loop {
            sleep(metrics_report_interval).await;
            storage.report_metrics();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, U256, map::B256Map};
    use reth::providers::{DBProvider, DatabaseProviderFactory};
    use reth_db::{
        tables::{self, PackedStoragesTrie},
        transaction::DbTxMut,
    };
    use reth_optimism_trie::{
        InMemoryProofsStorage, OpProofsStore, api::OpProofsInitProvider, db::StorageTrieKey,
    };
    use reth_primitives_traits::StorageEntry;
    use reth_provider::test_utils::create_test_provider_factory;
    use reth_storage_api::{StorageSettings, StorageSettingsCache};
    use reth_trie_common::{
        BranchNodeCompact, HashedStorageSorted, Nibbles, PackedStorageTrieEntry,
        PackedStoredNibblesSubKey, StoredNibbles, TrieMask, updates::StorageTrieUpdatesSorted,
    };
    use reth_trie_db::PackedKeyAdapter;

    #[test]
    fn historical_anchor_overlay_tracks_replaced_rows() {
        let account = B256::with_last_byte(1);
        let storage = B256::with_last_byte(2);
        let wiped_storage = B256::with_last_byte(3);
        let slot = B256::with_last_byte(4);
        let account_path = Nibbles::from_nibbles([0x01]);
        let storage_path = Nibbles::from_nibbles([0x02]);
        let untouched_path = Nibbles::from_nibbles([0x03]);
        let branch = BranchNodeCompact::new(
            TrieMask::new(0b0011),
            TrieMask::new(0b0011),
            TrieMask::new(0),
            Vec::new(),
            None,
        );

        let overlay = HistoricalAnchorOverlay {
            hashed_state: HashedPostStateSorted::new(
                vec![(account, None)],
                B256Map::from_iter([
                    (
                        storage,
                        HashedStorageSorted {
                            storage_slots: vec![(slot, U256::ZERO)],
                            wiped: false,
                        },
                    ),
                    (wiped_storage, HashedStorageSorted { storage_slots: Vec::new(), wiped: true }),
                ]),
            ),
            trie_updates: TrieUpdatesSorted::new(
                vec![(account_path.clone(), None)],
                B256Map::from_iter([
                    (
                        storage,
                        StorageTrieUpdatesSorted {
                            is_deleted: false,
                            storage_nodes: vec![(storage_path.clone(), Some(branch))],
                        },
                    ),
                    (
                        wiped_storage,
                        StorageTrieUpdatesSorted { is_deleted: true, storage_nodes: Vec::new() },
                    ),
                ]),
            ),
        };

        assert!(overlay.has_account_update(&account));
        assert!(overlay.has_storage_update(&storage, &slot));
        assert!(overlay.has_storage_update(&wiped_storage, &B256::with_last_byte(99)));
        assert!(overlay.has_account_trie_update(&account_path));
        assert!(overlay.has_storage_trie_update(&storage, &storage_path));
        assert!(overlay.has_storage_trie_update(&wiped_storage, &untouched_path));
        assert!(!overlay.has_account_update(&B256::with_last_byte(99)));
        assert!(!overlay.has_storage_trie_update(&storage, &untouched_path));
    }

    #[test]
    fn proof_history_initialization_reads_storage_v2_packed_trie_entries() {
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(StorageSettings::v2());

        let hashed_address = B256::with_last_byte(1);
        let hashed_slot = B256::with_last_byte(2);
        let trie_path = Nibbles::from_nibbles([0x0a, 0x0b]);
        let branch = BranchNodeCompact::new(
            TrieMask::new(0b0011),
            TrieMask::new(0b0011),
            TrieMask::new(0),
            Vec::new(),
            None,
        );

        let provider_rw = factory.database_provider_rw().unwrap();
        {
            let tx = provider_rw.tx_ref();
            tx.put::<tables::HashedStorages>(
                hashed_address,
                StorageEntry { key: hashed_slot, value: U256::from(1) },
            )
            .unwrap();
            tx.put::<PackedStoragesTrie>(
                hashed_address,
                PackedStorageTrieEntry {
                    nibbles: PackedStoredNibblesSubKey::from(trie_path),
                    node: branch,
                },
            )
            .unwrap();
        }
        provider_rw.commit().unwrap();

        let source_provider = factory.database_provider_ro().unwrap();
        let storage: OpProofsStorage<InMemoryProofsStorage> =
            InMemoryProofsStorage::default().into();

        ProofHistoryInitializationJob::new(storage.clone(), source_provider.into_tx())
            .run_with_adapter::<PackedKeyAdapter>(0, B256::ZERO)
            .unwrap();

        let anchor = storage.initialization_provider().unwrap().initial_state_anchor().unwrap();
        assert_eq!(
            anchor.latest_storage_trie_key,
            Some(StorageTrieKey::new(hashed_address, StoredNibbles::from(trie_path)))
        );
        assert_eq!(anchor.latest_hashed_storage_key.unwrap().hashed_storage_key, hashed_slot);
    }
}
