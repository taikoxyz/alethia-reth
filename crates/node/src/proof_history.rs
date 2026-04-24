//! Proof-history sidecar configuration and ExEx wiring for Taiko nodes.

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
    OpProofStoragePrunerTask, OpProofsStorage, OpProofsStore,
    api::{InitialStateAnchor, InitialStateStatus, OpProofsInitProvider, OpProofsProviderRO},
    db::{HashedStorageKey, MdbxProofsStorage, StorageTrieKey},
    live::LiveTrieCollector,
};
use reth_primitives_traits::Account;
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_storage_api::StorageSettingsCache;
use reth_trie_common::{
    BranchNodeCompact, HashedPostStateSorted, Nibbles, SortedTrieData, StoredNibbles,
    updates::TrieUpdatesSorted,
};
use reth_trie_db::{LegacyKeyAdapter, PackedKeyAdapter, StorageTrieEntryLike, TrieTableAdapter};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{sync::watch, task, time, time::sleep};
use tracing::{debug, error, info};

/// Default proof-history retention window in blocks.
const DEFAULT_PROOF_HISTORY_WINDOW: u64 = 1_296_000;

/// Default interval between proof-history prune passes.
const DEFAULT_PROOF_HISTORY_PRUNE_INTERVAL: Duration = Duration::from_secs(15);

/// Default interval, in blocks, between proof-history consistency checks.
const DEFAULT_PROOF_HISTORY_VERIFICATION_INTERVAL: u64 = 0;

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

/// Shared storage type used by proof-history indexing and debug RPC overrides.
pub type ProofHistoryStorage = OpProofsStorage<Arc<MdbxProofsStorage>>;

/// Result returned by proof-history installation with the updated node builder and optional handle.
pub type ProofHistoryInstallResult<T, CB, AO> = eyre::Result<(
    WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>,
    Option<ProofHistoryHandle>,
)>;

/// Handle returned when proof-history indexing is installed.
#[derive(Debug, Clone)]
pub struct ProofHistoryHandle {
    /// Shared storage handle used by both the ExEx and optional RPC replacement.
    storage: ProofHistoryStorage,
}

impl ProofHistoryHandle {
    /// Creates a proof-history handle from initialized shared storage.
    fn new(storage: ProofHistoryStorage) -> Self {
        Self { storage }
    }

    /// Returns a clone of the storage handle for components that need proof-history access.
    pub fn storage(&self) -> ProofHistoryStorage {
        self.storage.clone()
    }
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
    <T::Provider as DatabaseProviderFactory>::Provider: StorageSettingsCache,
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
    let handle = ProofHistoryHandle::new(storage.clone());
    let storage_for_exex = storage.clone();
    let storage_for_init = Arc::clone(&mdbx);

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
                initialize_proof_history_storage(exex_context.provider(), storage_for_init)?;
                Ok(ProofHistoryExEx::new(
                    exex_context,
                    storage_for_exex,
                    config.window,
                    config.prune_interval,
                    config.verification_interval,
                )
                .run())
            }),
        Some(handle),
    ))
}

/// Returns whether proof-history storage needs an initial current-state snapshot.
fn proof_history_storage_needs_initialization<Storage>(storage: &Storage) -> eyre::Result<bool>
where
    Storage: OpProofsStore,
{
    let provider_ro = storage.provider_ro()?;
    Ok(provider_ro.get_earliest_block_number()?.is_none() ||
        provider_ro.get_latest_block_number()?.is_none())
}

/// Safety threshold for automatic proof-history pruning on startup.
const PROOF_HISTORY_MAX_STARTUP_PRUNE_BLOCKS: u64 = 1000;

/// Number of blocks the proof-history sync task executes in one batch.
const PROOF_HISTORY_SYNC_BATCH_SIZE: usize = 50;

/// Distance from canonical tip where proof-history can process notification data directly.
const PROOF_HISTORY_REAL_TIME_BLOCKS_THRESHOLD: u64 = 1024;

/// Delay used when proof-history has no locally executable backfill work.
const PROOF_HISTORY_SYNC_IDLE_SLEEP: Duration = Duration::from_secs(5);

/// Returns the highest block proof-history may backfill without running ahead of execution.
fn proof_history_backfill_target(
    latest_stored: u64,
    requested_target: u64,
    executed_head: u64,
) -> Option<u64> {
    let target = requested_target.min(executed_head);
    (target > latest_stored).then_some(target)
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
    /// Number of recent blocks retained in proof-history storage.
    proofs_history_window: u64,
    /// Wall-clock interval between proof-history prune passes.
    proofs_history_prune_interval: Duration,
    /// Block interval between full execution verification checks.
    verification_interval: u64,
}

impl<Node, Storage> ProofHistoryExEx<Node, Storage>
where
    Node: FullNodeComponents,
{
    /// Creates a proof-history ExEx with Taiko backfill guards.
    const fn new(
        ctx: ExExContext<Node>,
        storage: OpProofsStorage<Storage>,
        proofs_history_window: u64,
        proofs_history_prune_interval: Duration,
        verification_interval: u64,
    ) -> Self {
        Self {
            ctx,
            storage,
            proofs_history_window,
            proofs_history_prune_interval,
            verification_interval,
        }
    }
}

impl<Node, Storage, Primitives> ProofHistoryExEx<Node, Storage>
where
    Node: FullNodeComponents<Types: NodeTypes<Primitives = Primitives>>,
    Primitives: NodePrimitives,
    Storage: OpProofsStore + Clone + 'static,
{
    /// Runs proof-history indexing until the node shuts down.
    async fn run(mut self) -> eyre::Result<()> {
        self.ensure_initialized()?;
        let sync_target_tx = self.spawn_sync_task();

        let prune_task = OpProofStoragePrunerTask::new(
            self.storage.clone(),
            self.ctx.provider().clone(),
            self.proofs_history_window,
            self.proofs_history_prune_interval,
        );
        self.ctx
            .task_executor()
            .spawn_with_graceful_shutdown_signal(|signal| Box::pin(prune_task.run(signal)));

        let collector = LiveTrieCollector::new(
            self.ctx.evm_config().clone(),
            self.ctx.provider().clone(),
            &self.storage,
        );

        while let Some(notification) = self.ctx.notifications.try_next().await? {
            self.handle_notification(notification, &collector, &sync_target_tx)?;
        }

        Ok(())
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

        let target_earliest = latest_block_number.saturating_sub(self.proofs_history_window);
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

    /// Spawns the guarded proof-history backfill task.
    fn spawn_sync_task(&self) -> watch::Sender<u64> {
        let (sync_target_tx, sync_target_rx) = watch::channel(0u64);
        let task_storage = self.storage.clone();
        let task_provider = self.ctx.provider().clone();
        let task_evm_config = self.ctx.evm_config().clone();

        self.ctx.task_executor().spawn_critical_task(
            "taiko::proof_history::sync_loop",
            async move {
                let storage = task_storage.clone();
                let collector =
                    LiveTrieCollector::new(task_evm_config, task_provider.clone(), &storage);
                Self::sync_loop(sync_target_rx, task_storage, task_provider, &collector).await;
            },
        );

        sync_target_tx
    }

    /// Backfills proof-history only through blocks the node has locally executed.
    async fn sync_loop(
        mut sync_target_rx: watch::Receiver<u64>,
        storage: OpProofsStorage<Storage>,
        provider: Node::Provider,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
    ) {
        debug!(target: "reth::taiko::proof_history", "starting proof-history sync loop");

        loop {
            let requested_target = *sync_target_rx.borrow_and_update();
            let latest = match storage.provider_ro().and_then(|p| p.get_latest_block_number()) {
                Ok(Some((number, _))) => number,
                Ok(None) => {
                    error!(target: "reth::taiko::proof_history", "proof-history sync loop found no stored blocks");
                    time::sleep(PROOF_HISTORY_SYNC_IDLE_SLEEP).await;
                    continue;
                }
                Err(error) => {
                    error!(target: "reth::taiko::proof_history", ?error, "failed to read proof-history latest block");
                    time::sleep(PROOF_HISTORY_SYNC_IDLE_SLEEP).await;
                    continue;
                }
            };

            let executed_head = match provider.best_block_number() {
                Ok(number) => number,
                Err(error) => {
                    error!(target: "reth::taiko::proof_history", ?error, "failed to read executed head for proof-history sync");
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
                time::sleep(PROOF_HISTORY_SYNC_IDLE_SLEEP).await;
                continue;
            };

            if let Err(error) = Self::process_batch(
                latest,
                target,
                &provider,
                collector,
                PROOF_HISTORY_SYNC_BATCH_SIZE,
            ) {
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
    fn handle_notification(
        &self,
        notification: ExExNotification<Primitives>,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
        sync_target_tx: &watch::Sender<u64>,
    ) -> eyre::Result<()> {
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

        if let Some(committed_chain) = notification.committed_chain() {
            self.ctx.events.send(ExExEvent::FinishedHeight(committed_chain.tip().num_hash()))?;
        }

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
        let should_verify = self.verification_interval > 0 &&
            block_number.is_multiple_of(self.verification_interval);

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

/// Initializes empty proof-history storage from the node's current canonical state.
fn initialize_proof_history_storage<Provider, Storage>(
    provider: &Provider,
    storage: Storage,
) -> eyre::Result<()>
where
    Provider: BlockNumReader + DatabaseProviderFactory,
    Provider::Provider: StorageSettingsCache,
    <Provider::DB as Database>::TX: Sync,
    Storage: OpProofsStore + Send,
{
    if !proof_history_storage_needs_initialization(&storage)? {
        return Ok(());
    }

    let chain_info = provider.chain_info()?;
    info!(
        target: "reth::taiko::proof_history",
        best_number = chain_info.best_number,
        best_hash = ?chain_info.best_hash,
        "initializing proof-history storage from current canonical state"
    );

    let db_provider = provider.database_provider_ro()?.disable_long_read_transaction_safety();
    let storage_v2 = db_provider.cached_storage_settings().is_v2();
    let db_tx = db_provider.into_tx();
    let init_job = ProofHistoryInitializationJob::new(storage, db_tx);
    if storage_v2 {
        init_job
            .run_with_adapter::<PackedKeyAdapter>(chain_info.best_number, chain_info.best_hash)?;
    } else {
        init_job
            .run_with_adapter::<LegacyKeyAdapter>(chain_info.best_number, chain_info.best_hash)?;
    }

    info!(
        target: "reth::taiko::proof_history",
        best_number = chain_info.best_number,
        best_hash = ?chain_info.best_hash,
        "proof-history storage initialized"
    );

    Ok(())
}

/// Batch size used while copying the current node state into proof-history storage.
const PROOF_HISTORY_INITIALIZATION_BATCH_SIZE: usize = 100_000;

/// Copies the node's current trie and hashed-state tables into proof-history storage.
#[derive(Debug)]
struct ProofHistoryInitializationJob<Tx, Storage> {
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
    const fn new(storage: Storage, tx: Tx) -> Self {
        Self { tx, storage }
    }

    /// Runs initialization using the trie key encoding selected by the node's storage settings.
    fn run_with_adapter<Adapter>(&self, best_number: u64, best_hash: B256) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        let init_provider = self.storage.initialization_provider()?;
        let anchor = init_provider.initial_state_anchor()?;

        match anchor.status {
            InitialStateStatus::Completed => return Ok(()),
            InitialStateStatus::NotStarted => {
                init_provider
                    .set_initial_state_anchor(BlockNumHash::new(best_number, best_hash))?;
                OpProofsInitProvider::commit(init_provider)?;
            }
            InitialStateStatus::InProgress => {
                self.validate_anchor_block(&anchor, best_number, best_hash)?;
                drop(init_provider);
            }
        }

        self.initialize_hashed_accounts(anchor.latest_hashed_account_key)?;
        self.initialize_hashed_storages(anchor.latest_hashed_storage_key)?;
        self.initialize_storage_trie::<Adapter>(anchor.latest_storage_trie_key)?;
        self.initialize_account_trie::<Adapter>(anchor.latest_account_trie_key)?;

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
        let mut cursor = self.tx.cursor_read::<tables::HashedAccounts>()?;

        if let Some(latest_key) = start_key {
            cursor.seek(latest_key)?.filter(|(key, _)| *key == latest_key).ok_or_else(|| {
                eyre!("proof-history initialization resume key missing in HashedAccounts")
            })?;
        }

        let mut batch = Vec::with_capacity(PROOF_HISTORY_INITIALIZATION_BATCH_SIZE);
        while let Some((hashed_address, account)) = cursor.next()? {
            batch.push((hashed_address, Some(account)));
            if batch.len() >= PROOF_HISTORY_INITIALIZATION_BATCH_SIZE {
                self.store_hashed_accounts(std::mem::take(&mut batch))?;
            }
        }
        self.store_hashed_accounts(batch)
    }

    /// Copies hashed storage leaves from the node database into proof-history storage.
    fn initialize_hashed_storages(&self, start_key: Option<HashedStorageKey>) -> eyre::Result<()> {
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
        let mut cursor = self.tx.cursor_read::<Adapter::AccountTrieTable>()?;

        if let Some(latest_key) = start_key {
            let adapter_key = Adapter::AccountKey::from(latest_key.0);
            cursor.seek(adapter_key.clone())?.filter(|(key, _)| *key == adapter_key).ok_or_else(
                || eyre!("proof-history initialization resume key missing in account trie"),
            )?;
        }

        let mut batch = Vec::with_capacity(PROOF_HISTORY_INITIALIZATION_BATCH_SIZE);
        while let Some((path, branch)) = cursor.next()? {
            batch.push((Adapter::account_key_to_nibbles(&path), Some(branch)));
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
            if current_address.is_some_and(|address| address != hashed_address) ||
                current_nodes.len() >= PROOF_HISTORY_INITIALIZATION_BATCH_SIZE
            {
                let address = current_address
                    .expect("storage trie batch is only populated after address is set");
                self.store_storage_branches(address, std::mem::take(&mut current_nodes))?;
            }

            let (subkey, branch) = entry.into_parts();
            current_address = Some(hashed_address);
            current_nodes.push((Adapter::subkey_to_nibbles(&subkey), Some(branch)));
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
    ctx.modules.replace_configured(eth_ext.into_rpc())?;

    let debug_ext = TaikoDebugWitnessExt::new(
        ctx.node().provider().clone(),
        ctx.registry.eth_api().clone(),
        storage,
        ctx.node().task_executor().clone(),
    );
    ctx.modules.replace_configured(debug_ext.into_rpc())?;
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
    use alloy_primitives::{B256, U256};
    use reth::providers::{DBProvider, DatabaseProviderFactory};
    use reth_db::{
        tables::{self, PackedStoragesTrie},
        transaction::DbTxMut,
    };
    use reth_optimism_trie::{
        InMemoryProofsStorage, OpProofsStore,
        api::{OpProofsInitProvider, OpProofsProviderRw},
        db::StorageTrieKey,
    };
    use reth_primitives_traits::StorageEntry;
    use reth_provider::test_utils::create_test_provider_factory;
    use reth_storage_api::{StorageSettings, StorageSettingsCache};
    use reth_trie_common::{
        BranchNodeCompact, Nibbles, PackedStorageTrieEntry, PackedStoredNibblesSubKey,
        StoredNibbles, TrieMask,
    };
    use reth_trie_db::PackedKeyAdapter;

    #[test]
    fn disabled_config_has_no_storage_path() {
        let config = ProofHistoryConfig::disabled();

        assert!(!config.enabled);
        assert!(config.storage_path.is_none());
        assert!(config.required_storage_path().is_err());
    }

    #[test]
    fn enabled_config_requires_storage_path() {
        let config = ProofHistoryConfig { enabled: true, ..ProofHistoryConfig::disabled() };

        assert!(config.required_storage_path().is_err());
    }

    #[test]
    fn proof_history_storage_initialization_check_tracks_empty_storage() {
        let storage: OpProofsStorage<InMemoryProofsStorage> =
            InMemoryProofsStorage::default().into();

        assert!(proof_history_storage_needs_initialization(&storage).unwrap());

        let provider_rw = storage.provider_rw().expect("provider_rw");
        provider_rw.set_earliest_block_number(0, B256::ZERO).expect("set earliest block");
        provider_rw.commit().expect("commit");

        assert!(!proof_history_storage_needs_initialization(&storage).unwrap());
    }

    #[test]
    fn proof_history_backfill_waits_when_executed_head_has_no_next_parent_state() {
        assert_eq!(proof_history_backfill_target(0, 350_000, 0), None);
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
