//! Proof-history sidecar configuration and ExEx wiring for Taiko nodes.

use crate::TaikoNode;
use alethia_reth_rpc::{
    debug::{TaikoDebugWitnessApiServer, TaikoDebugWitnessExt},
    eth::proofs::{TaikoEthProofApiServer, TaikoEthProofExt},
};
use alloy_eips::BlockNumHash;
use alloy_primitives::{B256, U256};
use eyre::{WrapErr, eyre};
use reth::{
    providers::{BlockNumReader, DBProvider, DatabaseProviderFactory, HeaderProvider},
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
use reth_node_api::{FullNodeComponents, NodeAddOns};
use reth_node_builder::{
    NodeAdapter, NodeBuilderWithComponents, NodeComponentsBuilder, WithLaunchContext,
    rpc::{RethRpcAddOns, RpcContext},
};
use reth_optimism_exex::OpProofsExEx;
use reth_optimism_trie::{
    OpProofsStorage, OpProofsStore,
    api::{InitialStateAnchor, InitialStateStatus, OpProofsInitProvider, OpProofsProviderRO},
    db::{HashedStorageKey, MdbxProofsStorage, StorageTrieKey},
};
use reth_primitives_traits::Account;
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_storage_api::StorageSettingsCache;
use reth_trie_common::{BranchNodeCompact, Nibbles, StoredNibbles};
use reth_trie_db::{LegacyKeyAdapter, PackedKeyAdapter, StorageTrieEntryLike, TrieTableAdapter};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::info;

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
                Ok(OpProofsExEx::builder(exex_context, storage_for_exex)
                    .with_proofs_history_window(config.window)
                    .with_proofs_history_prune_interval(config.prune_interval)
                    .with_verification_interval(config.verification_interval)
                    .build()
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
