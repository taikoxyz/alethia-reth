//! Proof-history storage bootstrap and finalized-window backfill.

use alloy_eips::BlockNumHash;
use eyre::eyre;
use reth::providers::{
    BlockHashReader, BlockNumReader, DBProvider, DatabaseProviderFactory, HeaderProvider,
    StageCheckpointReader,
};
use reth_db::Database;
use reth_optimism_trie::{
    BackfillJob, InitializationJob, OpProofsBackfillStore, OpProofsStorageError, OpProofsStore,
    RethTrieStorageLayout,
    api::{InitialStateStatus, OpProofsInitProvider, OpProofsProviderRO},
    db::MdbxProofsStorage,
};
use reth_storage_api::{
    ChainStateBlockReader, ChangeSetReader, StorageChangeSetReader, StorageSettingsCache,
};
use std::path::Path;
use tracing::info;

/// Rejects a configured proof-history path when it contains legacy V1 data.
///
/// An empty V1 schema may coexist with the V2 schema, but any V1 initialization state or retained
/// bound requires an explicit operator-selected wipe or fresh path. The probe closes every V1
/// handle before returning and never removes the configured directory.
pub(super) fn refuse_legacy_v1_storage(path: &Path) -> eyre::Result<()> {
    let legacy = MdbxProofsStorage::new(path)?;
    let initializer = legacy.initialization_provider()?;
    let anchor = initializer.initial_state_anchor()?;
    drop(initializer);

    let provider = legacy.provider_ro()?;
    let earliest = provider.get_earliest_block();
    let latest = provider.get_latest_block();
    let populated = !matches!(anchor.status, InitialStateStatus::NotStarted) ||
        !matches!(earliest, Err(OpProofsStorageError::NoBlocksFound)) ||
        !matches!(latest, Err(OpProofsStorageError::NoBlocksFound));
    drop(provider);
    drop(legacy);

    if populated {
        return Err(eyre!(
            "legacy proof-history V1 storage at {} contains data; remove the directory or use a fresh path and restart",
            path.display()
        ));
    }

    Ok(())
}

/// Returns the next block proof-history should backfill toward, or `None` when caught up.
///
/// The target is always the node's locally executed on-disk head: canonical notifications only
/// wake the sync loop, they never extend its target, so re-execution can never read a block that
/// is not yet persisted. Deriving the target from the executed head (rather than the last
/// notification) also means a pipeline/staged-sync gap is backfilled even when no live
/// notification arrives, e.g. right after a restart or while the consensus feed is down.
pub(super) fn proof_history_sync_target(latest_stored: u64, executed_head: u64) -> Option<u64> {
    (executed_head > latest_stored).then_some(executed_head)
}

/// Returns the persisted DB block used to label current-state proof-history initialization.
fn proof_history_current_state_anchor<Provider>(provider: &Provider) -> eyre::Result<BlockNumHash>
where
    Provider: BlockNumReader + HeaderProvider,
{
    let best_number = provider.best_block_number()?;
    let best_header = provider
        .sealed_header(best_number)?
        .ok_or_else(|| eyre!("missing persisted header {best_number}"))?;
    Ok(BlockNumHash::new(best_number, best_header.hash()))
}

/// Rejects resuming a partial copy when its persisted source anchor has moved.
///
/// A partial copy contains rows read from the stored anchor's main-DB snapshot. Mixing those rows
/// with a different source head is unsafe, so the error names the proof path and requires an
/// operator-selected wipe or fresh path.
fn validate_in_progress_anchor<Storage>(
    storage: &Storage,
    expected: BlockNumHash,
    storage_path: &Path,
) -> eyre::Result<()>
where
    Storage: OpProofsStore,
{
    let initializer = storage.initialization_provider()?;
    let anchor = initializer.initial_state_anchor()?;
    if matches!(anchor.status, InitialStateStatus::InProgress) && anchor.block != Some(expected) {
        return Err(eyre!(
            "in-progress proof-history initialization at {} targets {:?}, but the persisted source head is {:?}; wipe that proof-history directory or use a fresh path and restart initialization",
            storage_path.display(),
            anchor.block,
            expected,
        ));
    }
    Ok(())
}

/// Copies current state with upstream initialization from one pinned main-DB snapshot.
///
/// The persisted head, its sealed hash, and trie layout are all read before the provider is
/// consumed into its transaction. The returned anchor is therefore the exact source state copied
/// into proof storage; a partial copy at any other anchor is rejected before new rows are written.
fn initialize_from_pinned_provider<Provider, Storage>(
    db_provider: Provider,
    storage: Storage,
    storage_path: &Path,
) -> eyre::Result<BlockNumHash>
where
    Provider: BlockNumReader + DBProvider + HeaderProvider + StorageSettingsCache,
    Provider::Tx: Sync,
    Storage: OpProofsStore + Send,
{
    let anchor = proof_history_current_state_anchor(&db_provider)?;
    let layout = if db_provider.cached_storage_settings().is_v2() {
        RethTrieStorageLayout::Packed
    } else {
        RethTrieStorageLayout::Legacy
    };
    validate_in_progress_anchor(&storage, anchor, storage_path)?;
    info!(
        target: "reth::taiko::proof_history",
        best_number = anchor.number,
        best_hash = ?anchor.hash,
        "initializing proof-history storage from current persisted state"
    );
    InitializationJob::new(storage, db_provider.into_tx(), layout)
        .run(anchor.number, anchor.hash)?;
    Ok(anchor)
}

/// Prepares proof storage at the exact persisted execution head.
///
/// Fresh storage is initialized, a same-anchor partial copy resumes, and completed storage is left
/// unchanged by upstream. A partial copy whose source moved fails with path-specific recovery
/// guidance instead of mixing rows from multiple main-DB states.
pub(super) fn initialize_proof_history_storage<Provider, Storage>(
    provider: &Provider,
    storage: Storage,
    storage_path: &Path,
) -> eyre::Result<()>
where
    Provider: DatabaseProviderFactory,
    Provider::Provider: BlockNumReader + DBProvider + HeaderProvider + StorageSettingsCache,
    <Provider::DB as Database>::TX: Sync,
    Storage: OpProofsStore + Send,
{
    let anchor =
        initialize_from_pinned_provider(provider.database_provider_ro()?, storage, storage_path)?;
    info!(
        target: "reth::taiko::proof_history",
        best_number = anchor.number,
        best_hash = ?anchor.hash,
        "proof-history storage initialized"
    );

    Ok(())
}

/// Verifies that a stored proof-history block still matches the pinned canonical database.
fn validate_canonical_stored_block<Provider>(
    provider: &Provider,
    block: BlockNumHash,
    label: &'static str,
    storage_path: &Path,
) -> eyre::Result<()>
where
    Provider: HeaderProvider,
{
    let canonical = provider.sealed_header(block.number)?;
    if canonical.as_ref().map(|header| header.hash()) != Some(block.hash) {
        return Err(eyre!(
            "proof-history {label} block {:?} is not canonical in the persisted database; wipe proof-history storage at {} or use a fresh path and restart initialization",
            block,
            storage_path.display(),
        ));
    }
    Ok(())
}

/// Initializes at the executed head and backfills to the persisted finalized-window target.
///
/// Returns `false` without mutating an uninitialized store while finalized state is unavailable or
/// execution is below `finalized.saturating_sub(window)`. Once initialization is possible, the
/// source snapshot is pinned through the upstream copy. A fresh snapshot then validates the stored
/// anchor and latest block before upstream backfill resumes from its committed earliest block.
pub(super) fn initialize_finalized_window_proof_history_storage<Provider, Storage>(
    provider: &Provider,
    storage: Storage,
    storage_path: &Path,
    window: u64,
) -> eyre::Result<bool>
where
    Provider: DatabaseProviderFactory,
    Provider::Provider: BlockHashReader
        + BlockNumReader
        + ChainStateBlockReader
        + ChangeSetReader
        + DBProvider
        + HeaderProvider
        + StageCheckpointReader
        + StorageChangeSetReader
        + StorageSettingsCache
        + Send,
    <Provider::DB as Database>::TX: Sync,
    Storage: OpProofsBackfillStore + Clone + Send,
{
    let db_provider = provider.database_provider_ro()?;
    let executed_head = db_provider.best_block_number()?;
    let Some(finalized) = db_provider.last_finalized_block_number()? else {
        return Ok(false);
    };
    let target_earliest = finalized.saturating_sub(window);
    if executed_head < target_earliest {
        return Ok(false);
    }

    initialize_from_pinned_provider(db_provider, storage.clone(), storage_path)?;

    let initializer = storage.initialization_provider()?;
    let anchor = initializer.initial_state_anchor()?;
    drop(initializer);
    if !matches!(anchor.status, InitialStateStatus::Completed) {
        return Err(eyre!("proof-history initialization did not complete"));
    }
    let anchor = anchor.block.ok_or_else(|| eyre!("completed proof-history anchor is missing"))?;
    let latest = storage.provider_ro()?.get_latest_block()?;

    let db_provider = provider.database_provider_ro()?;
    validate_canonical_stored_block(&db_provider, anchor, "initialization anchor", storage_path)?;
    validate_canonical_stored_block(&db_provider, latest, "latest", storage_path)?;
    let Some(finalized) = db_provider.last_finalized_block_number()? else {
        return Ok(false);
    };
    let target_earliest = finalized.saturating_sub(window);
    if db_provider.best_block_number()? < target_earliest {
        return Ok(false);
    }
    BackfillJob::new(db_provider, storage).run(target_earliest)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_eips::NumHash;
    use alloy_primitives::B256;
    use reth_evm::{ConfigureEvm, execute::Executor};
    use reth_evm_ethereum::EthEvmConfig;
    use reth_optimism_trie::{
        OpProofsStorage, OpProofsStorageError,
        api::OpProofsInitProvider,
        backfill::BackfillJob,
        db::{MdbxProofsStorage, MdbxProofsStorageV2, ProofWindow, ProofWindowKey, Tables},
    };
    use reth_primitives_traits::RecoveredBlock;
    use reth_provider::{
        BlockWriter, ChainStateBlockWriter, ExecutionOutcome, HashedPostStateProvider,
        LatestStateProviderRef, ProviderFactory, StateRootProvider,
        test_utils::{MockNodeTypesWithDB, create_test_provider_factory_with_chain_spec},
    };
    use reth_revm::database::StateProviderDatabase;
    use std::{path::PathBuf, sync::Arc};

    /// Paris-activated mainnet fixture used for real provider initialization and backfill tests.
    fn initialization_test_chain_spec() -> Arc<reth_chainspec::ChainSpec> {
        use reth_chainspec::{ChainSpecBuilder, MAINNET};

        Arc::new(
            ChainSpecBuilder::default()
                .chain(MAINNET.chain)
                .genesis(MAINNET.genesis.clone())
                .paris_activated()
                .build(),
        )
    }

    /// Builds and persists an empty block whose state root equals its executed post-state.
    fn append_empty_block(
        factory: &ProviderFactory<MockNodeTypesWithDB>,
        chain_spec: &Arc<reth_chainspec::ChainSpec>,
        number: u64,
        parent_hash: B256,
    ) -> BlockNumHash {
        use reth_ethereum_primitives::{Block, BlockBody, Receipt};

        let mut block = RecoveredBlock::new_unhashed(
            Block {
                header: Header { parent_hash, number, ..Default::default() },
                body: BlockBody::default(),
            },
            Vec::new(),
        );
        let execution_output = {
            let provider = factory.provider().expect("state provider opens");
            let database = StateProviderDatabase::new(LatestStateProviderRef::new(&provider));
            EthEvmConfig::ethereum(chain_spec.clone())
                .batch_executor(database)
                .execute(&block)
                .expect("empty block executes")
        };
        let hashed_state = {
            let provider = factory.provider().expect("state provider opens");
            let latest = LatestStateProviderRef::new(&provider);
            let hashed_state = latest.hashed_post_state(&execution_output.state);
            let state_root =
                latest.state_root(hashed_state.clone()).expect("empty block state root computes");
            block.set_state_root(state_root);
            hashed_state
        };
        let outcome = ExecutionOutcome::<Receipt> {
            bundle: execution_output.state.clone(),
            receipts: vec![execution_output.receipts.clone()],
            first_block: number,
            requests: vec![execution_output.requests.clone()],
        };
        let block_hash = block.hash();
        let provider = factory.provider_rw().expect("write provider opens");
        provider
            .append_blocks_with_state(vec![block], &outcome, hashed_state.into_sorted())
            .expect("empty block persists");
        provider.commit().expect("empty block commit succeeds");

        BlockNumHash::new(number, block_hash)
    }

    /// Creates a real persisted chain through `height`, returning every canonical num-hash.
    fn initialization_provider_with_blocks(
        height: u64,
    ) -> (ProviderFactory<MockNodeTypesWithDB>, Vec<BlockNumHash>) {
        let chain_spec = initialization_test_chain_spec();
        let factory = create_test_provider_factory_with_chain_spec(chain_spec.clone());
        reth_db_common::init::init_genesis(&factory).expect("genesis initializes");

        let mut blocks = vec![BlockNumHash::new(0, chain_spec.genesis_hash())];
        for number in 1..=height {
            blocks.push(append_empty_block(
                &factory,
                &chain_spec,
                number,
                blocks.last().expect("genesis exists").hash,
            ));
        }

        (factory, blocks)
    }

    /// Creates raw V2 proof storage and keeps its configured path available to diagnostics.
    fn initialization_v2_storage() -> (Arc<MdbxProofsStorageV2>, PathBuf) {
        let path = tempfile::tempdir().expect("temporary proof path").keep();
        let storage = Arc::new(MdbxProofsStorageV2::new(&path).expect("V2 storage opens"));
        (storage, path)
    }

    /// Persists a finalized height in the real provider database.
    fn persist_finalized(factory: &ProviderFactory<MockNodeTypesWithDB>, finalized: u64) {
        let provider = factory.provider_rw().expect("write provider opens");
        provider.save_finalized_block_number(finalized).expect("finalized height persists");
        provider.commit().expect("finalized height commits");
    }

    #[test]
    fn fresh_v2_initialization_records_exact_persisted_head() {
        let (factory, blocks) = initialization_provider_with_blocks(2);
        let expected = *blocks.last().expect("persisted head exists");
        let (storage, path) = initialization_v2_storage();

        initialize_proof_history_storage(&factory, storage.clone(), &path)
            .expect("fresh V2 storage initializes");

        let anchor = storage
            .initialization_provider()
            .expect("initialization provider opens")
            .initial_state_anchor()
            .expect("initialization anchor reads");
        assert!(matches!(anchor.status, InitialStateStatus::Completed));
        assert_eq!(anchor.block, Some(expected));
        let window = storage
            .provider_ro()
            .expect("read provider opens")
            .get_proof_window()
            .expect("proof window exists");
        assert_eq!(window.earliest, expected);
        assert_eq!(window.latest, expected);
    }

    #[test]
    fn interrupted_v2_initialization_resumes_at_same_source_anchor() {
        let (factory, blocks) = initialization_provider_with_blocks(1);
        let expected = *blocks.last().expect("persisted head exists");
        let (storage, path) = initialization_v2_storage();
        let initializer = storage.initialization_provider().expect("initializer opens");
        initializer.set_initial_state_anchor(expected).expect("anchor starts");
        initializer.commit().expect("in-progress anchor commits");

        initialize_proof_history_storage(&factory, storage.clone(), &path)
            .expect("same source anchor resumes");

        let anchor = storage
            .initialization_provider()
            .expect("initializer opens")
            .initial_state_anchor()
            .expect("anchor reads");
        assert!(matches!(anchor.status, InitialStateStatus::Completed));
        assert_eq!(anchor.block, Some(expected));
    }

    #[test]
    fn interrupted_v2_initialization_refuses_moved_source_anchor() {
        let (factory, blocks) = initialization_provider_with_blocks(1);
        let persisted = *blocks.last().expect("persisted head exists");
        let (storage, path) = initialization_v2_storage();
        let stale_anchor =
            BlockNumHash::new(persisted.number.saturating_sub(1), B256::repeat_byte(0x44));
        let initializer = storage.initialization_provider().expect("initializer opens");
        initializer.set_initial_state_anchor(stale_anchor).expect("stale anchor starts");
        initializer.commit().expect("stale in-progress anchor commits");

        let error = initialize_proof_history_storage(&factory, storage, &path)
            .expect_err("moved source anchor must be refused")
            .to_string();

        assert!(error.contains(&path.display().to_string()), "missing path in {error:?}");
        assert!(error.contains("wipe"), "missing wipe guidance in {error:?}");
        assert!(error.contains("fresh path"), "missing fresh-path guidance in {error:?}");
    }

    #[test]
    fn finalized_window_waits_without_persisted_finality() {
        let (factory, _) = initialization_provider_with_blocks(1);
        let (storage, path) = initialization_v2_storage();

        let prepared =
            initialize_finalized_window_proof_history_storage(&factory, storage.clone(), &path, 2)
                .expect("missing finality is a wait state");

        assert!(!prepared);
        let anchor = storage
            .initialization_provider()
            .expect("initializer opens")
            .initial_state_anchor()
            .expect("anchor reads");
        assert!(matches!(anchor.status, InitialStateStatus::NotStarted));
    }

    #[test]
    fn finalized_window_waits_when_execution_is_below_target() {
        let (factory, _) = initialization_provider_with_blocks(1);
        persist_finalized(&factory, 4);
        let (storage, path) = initialization_v2_storage();

        let prepared =
            initialize_finalized_window_proof_history_storage(&factory, storage.clone(), &path, 2)
                .expect("execution below target is a wait state");

        assert!(!prepared);
        let anchor = storage
            .initialization_provider()
            .expect("initializer opens")
            .initial_state_anchor()
            .expect("anchor reads");
        assert!(matches!(anchor.status, InitialStateStatus::NotStarted));
    }

    #[test]
    fn finalized_window_initializes_at_executed_head_and_backfills_to_target() {
        let (factory, blocks) = initialization_provider_with_blocks(5);
        persist_finalized(&factory, 4);
        let expected_latest = blocks[5];
        let expected_earliest = blocks[2];
        let (storage, path) = initialization_v2_storage();

        let prepared =
            initialize_finalized_window_proof_history_storage(&factory, storage.clone(), &path, 2)
                .expect("finalized window prepares");

        assert!(prepared);
        let window = storage
            .provider_ro()
            .expect("read provider opens")
            .get_proof_window()
            .expect("proof window exists");
        assert_eq!(window.earliest, expected_earliest);
        assert_eq!(window.latest, expected_latest);
    }

    #[test]
    fn interrupted_backfill_resumes_from_committed_earliest() {
        let (factory, blocks) = initialization_provider_with_blocks(5);
        persist_finalized(&factory, 4);
        let (storage, path) = initialization_v2_storage();
        initialize_proof_history_storage(&factory, storage.clone(), &path)
            .expect("current-state initialization succeeds");
        BackfillJob::new(
            factory.database_provider_ro().expect("read provider opens"),
            storage.clone(),
        )
        .with_batch_size(1)
        .run(3)
        .expect("first committed backfill segment succeeds");
        assert_eq!(
            storage
                .provider_ro()
                .expect("read provider opens")
                .get_earliest_block()
                .expect("earliest exists"),
            NumHash::new(3, blocks[3].hash),
        );

        let prepared =
            initialize_finalized_window_proof_history_storage(&factory, storage.clone(), &path, 2)
                .expect("backfill resumes");

        assert!(prepared);
        let window = storage
            .provider_ro()
            .expect("read provider opens")
            .get_proof_window()
            .expect("proof window exists");
        assert_eq!(window.earliest, blocks[2]);
        assert_eq!(window.latest, blocks[5]);
    }

    #[test]
    fn proof_history_backfill_waits_when_executed_head_has_no_next_parent_state() {
        // Nothing is executed locally beyond the stored anchor, so there is nothing to backfill
        // regardless of how far ahead the notified canonical tip is.
        assert_eq!(proof_history_sync_target(0, 0), None);
    }

    #[test]
    fn proof_history_sync_target_tracks_executed_head_when_notification_is_stale() {
        // Incident: no live notification arrived after a stall, but the node pipeline-synced
        // ahead. Proof-history must still backfill up to the executed head.
        assert_eq!(proof_history_sync_target(8_108_771, 8_110_008), Some(8_110_008));
    }

    #[test]
    fn proof_history_sync_target_waits_when_caught_up_to_executed_head() {
        assert_eq!(proof_history_sync_target(8_110_008, 8_110_008), None);
    }

    #[test]
    fn proof_history_sync_target_backfills_to_executed_head() {
        assert_eq!(proof_history_sync_target(100, 150), Some(150));
    }

    #[test]
    fn proof_history_sync_target_reports_none_when_executed_head_regressed_below_stored() {
        // Reorg/unwind rolled the on-disk executed head back below what proof-history already
        // stored. There is nothing to backfill (`None`), but this is a divergence, not healthy
        // idle: the sync loop logs it because the notification-driven reorg handlers that would
        // unwind `latest_stored` only run on live notifications.
        assert_eq!(proof_history_sync_target(200, 150), None);
    }

    /// Writes selected V1 proof-window rows into a fresh proof-history MDBX database.
    fn write_legacy_mdbx_layout(path: &Path, rows: &[(ProofWindowKey, BlockNumHash)]) {
        use reth_db::{
            Database,
            mdbx::{DatabaseArguments, init_db_for},
            transaction::{DbTx, DbTxMut},
        };

        let env = init_db_for::<_, Tables>(path, DatabaseArguments::default())
            .expect("raw proofs database opens");
        let tx = env.tx_mut().expect("write transaction opens");
        for (key, block) in rows {
            tx.put::<ProofWindow>(*key, (*block).into()).expect("legacy row writes");
        }
        tx.commit().expect("legacy layout commits");
    }

    /// MDBX proofs storage opened over a directory prepared by [`write_legacy_mdbx_layout`].
    fn legacy_mdbx_storage(path: &Path) -> OpProofsStorage<Arc<MdbxProofsStorage>> {
        Arc::new(MdbxProofsStorage::new(path).expect("mdbx storage opens")).into()
    }

    fn assert_v1_refusal(error: &eyre::Report, path: &Path) {
        let message = error.to_string();
        let configured_path = path.display().to_string();
        assert!(message.contains(&configured_path), "missing {configured_path:?} in {message:?}");
        assert!(
            message.contains("remove the directory or use a fresh path and restart"),
            "missing operator instruction in {message:?}"
        );
    }

    #[test]
    fn empty_v1_storage_allows_v2_cutover() {
        let dir = tempfile::tempdir().unwrap();
        drop(legacy_mdbx_storage(dir.path()));

        refuse_legacy_v1_storage(dir.path()).expect("empty V1 storage allows V2 cutover");

        let storage = MdbxProofsStorageV2::new(dir.path()).expect("V2 storage opens");
        let provider = storage.provider_ro().expect("V2 read provider opens");
        assert!(matches!(provider.get_earliest_block(), Err(OpProofsStorageError::NoBlocksFound)));
        assert!(matches!(provider.get_latest_block(), Err(OpProofsStorageError::NoBlocksFound)));
    }

    #[test]
    fn completed_v1_storage_is_refused_with_configured_path() {
        let dir = tempfile::tempdir().unwrap();
        let storage = legacy_mdbx_storage(dir.path());
        let initializer = storage.initialization_provider().expect("initialization provider");
        initializer
            .set_initial_state_anchor(BlockNumHash::new(42, B256::with_last_byte(7)))
            .expect("set initial state anchor");
        initializer.commit_initial_state().expect("complete initial state");
        initializer.commit().expect("commit completed V1 storage");
        drop(storage);

        let error = refuse_legacy_v1_storage(dir.path()).unwrap_err();

        assert_v1_refusal(&error, dir.path());
    }

    #[test]
    fn in_progress_v1_storage_is_refused_with_configured_path() {
        let dir = tempfile::tempdir().unwrap();
        let storage = legacy_mdbx_storage(dir.path());
        let initializer = storage.initialization_provider().expect("initialization provider");
        initializer
            .set_initial_state_anchor(BlockNumHash::new(42, B256::with_last_byte(7)))
            .expect("set initial state anchor");
        initializer.commit().expect("commit in-progress V1 storage");
        drop(storage);

        let error = refuse_legacy_v1_storage(dir.path()).unwrap_err();

        assert_v1_refusal(&error, dir.path());
    }

    #[test]
    fn v1_storage_with_any_window_bound_is_refused() {
        for bound in [ProofWindowKey::EarliestBlock, ProofWindowKey::LatestBlock] {
            let dir = tempfile::tempdir().unwrap();
            write_legacy_mdbx_layout(
                dir.path(),
                &[(bound, BlockNumHash::new(42, B256::with_last_byte(7)))],
            );

            let error = refuse_legacy_v1_storage(dir.path()).unwrap_err();

            assert_v1_refusal(&error, dir.path());
        }
    }

    #[test]
    fn v1_refusal_does_not_delete_storage() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = BlockNumHash::new(42, B256::with_last_byte(7));
        let storage = legacy_mdbx_storage(dir.path());
        let initializer = storage.initialization_provider().expect("initialization provider");
        initializer.set_initial_state_anchor(anchor).expect("set initial state anchor");
        initializer.commit_initial_state().expect("complete initial state");
        initializer.commit().expect("commit completed V1 storage");
        drop(storage);

        let error = refuse_legacy_v1_storage(dir.path()).unwrap_err();
        assert_v1_refusal(&error, dir.path());

        let storage = legacy_mdbx_storage(dir.path());
        let initializer = storage.initialization_provider().expect("initialization provider");
        let stored_anchor = initializer.initial_state_anchor().expect("read initial state anchor");
        assert!(matches!(stored_anchor.status, InitialStateStatus::Completed));
        assert_eq!(stored_anchor.block, Some(anchor));
        drop(initializer);
        let provider = storage.provider_ro().expect("read provider opens");
        assert_eq!(provider.get_earliest_block().unwrap(), anchor);
        assert_eq!(provider.get_latest_block().unwrap(), anchor);
    }
}
