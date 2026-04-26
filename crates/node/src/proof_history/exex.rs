//! Proof-history execution extension: notification handling, sync loop, pruner task.

use super::storage_init::{
    DelayedProofHistoryStart, PROOF_HISTORY_MAX_STARTUP_PRUNE_BLOCKS,
    ProofHistoryInitializationAction, delayed_proof_history_start, finalized_block_number,
    initialize_historical_proof_history_storage, initialize_proof_history_storage,
    proof_history_backfill_target, proof_history_storage_needs_initialization,
};
use alloy_consensus::BlockHeader;
use alloy_eips::eip1898::BlockWithParent;
use eyre::eyre;
use futures_util::TryStreamExt;
use reth::providers::{
    BlockNumReader, BlockReader, DBProvider, DatabaseProviderFactory, HeaderProvider,
    TransactionVariant,
};
use reth_db::Database;
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_node_api::{FullNodeComponents, NodePrimitives, NodeTypes};
use reth_optimism_trie::{
    OpProofStoragePruner, OpProofsStorage, OpProofsStore, api::OpProofsProviderRO,
    live::LiveTrieCollector,
};
use reth_storage_api::{
    ChainStateBlockReader, ChangeSetReader, StorageChangeSetReader, StorageSettingsCache,
};
use reth_trie_common::{HashedPostStateSorted, SortedTrieData, updates::TrieUpdatesSorted};
use std::{
    panic,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Mutex, watch},
    task,
    time::{self, MissedTickBehavior},
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
pub(super) struct ProofHistoryExExConfig {
    /// Number of recent blocks retained in proof-history storage.
    pub(super) proofs_history_window: u64,
    /// Wall-clock interval between proof-history prune passes.
    pub(super) proofs_history_prune_interval: Duration,
    /// Block interval between full execution verification checks.
    pub(super) verification_interval: u64,
    /// Whether empty proof-history storage waits for the finalized retention window.
    pub(super) backfill_window_only: bool,
    /// Sidecar file that records historical initialization target metadata.
    pub(super) historical_init_metadata_path: Option<PathBuf>,
}

/// Taiko proof-history ExEx that keeps OP proofs storage behind locally executed state.
#[derive(Debug)]
pub(super) struct ProofHistoryExEx<Node, Storage>
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
    pub(super) fn new(
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
    pub(super) async fn run(mut self) -> eyre::Result<()> {
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
