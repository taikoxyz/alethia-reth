//! Proof-history storage bootstrap, window backfill, and window-start state machine.

use alloy_eips::BlockNumHash;
use eyre::{WrapErr, eyre};
use reth::providers::{
    BlockHashReader, BlockNumReader, DBProvider, DatabaseProviderFactory, HeaderProvider,
    StageCheckpointReader,
};
use reth_db::Database;
use reth_optimism_trie::{
    BackfillError, BackfillJob, InitializationJob, OpProofsBackfillStore, OpProofsStore,
    RethTrieStorageLayout,
    api::{InitialStateStatus, OpProofsInitProvider, OpProofsProviderRO, ProofWindowRange},
    db::MdbxProofsStorage,
};
use reth_storage_api::{
    ChainStateBlockReader, ChangeSetReader, StorageChangeSetReader, StorageSettingsCache,
};
use std::{fs, io, path::Path, sync::Arc};
use tracing::{info, warn};

/// Returns whether proof-history storage needs an initial current-state snapshot.
pub(super) fn proof_history_storage_needs_initialization<Storage>(
    storage: &Storage,
) -> eyre::Result<bool>
where
    Storage: OpProofsStore,
{
    let provider_ro = storage.provider_ro()?;
    Ok(super::opt_block(provider_ro.get_earliest_block())?.is_none() ||
        super::opt_block(provider_ro.get_latest_block())?.is_none())
}

/// Opens the proof-history MDBX store at `path`, deleting the store's files and reopening it when
/// the existing store cannot be used.
///
/// The store is rebuilt in two cases:
/// - It holds only removed v1 proof-history data: [`MdbxProofsStorage`] now reads and writes the v2
///   tables, so a v1 directory would otherwise open as an empty store and keep the dead v1 tables
///   forever.
/// - It holds an incomplete current-state initialization (an initial-state anchor but no committed
///   proof window): upstream resumes such a copy only while the anchor still equals the best block,
///   which a live node's moving tip breaks, so a restart would fail on every attempt. Nothing is
///   served before initialization completes, so discarding the copy is safe.
///
/// The wipe deletes only [`PROOF_HISTORY_STORE_FILES`] inside `path` and keeps `path` itself and
/// every other entry, so a mount point or symlink survives and a path shared with other data (such
/// as the reth datadir) loses nothing else. A failure to inspect the store or to delete its files
/// aborts with `path` in the error; a failed deletion may leave some of those files behind, and the
/// next start then either rebuilds again or opens an empty store.
///
/// Must run before any other handle to the store exists.
pub(super) fn open_proof_history_storage(path: &Path) -> eyre::Result<Arc<MdbxProofsStorage>> {
    let storage = open_mdbx(path)?;
    let Some(reason) = rebuild_reason(&storage)
        .wrap_err_with(|| format!("failed to inspect proof-history storage at {path:?}"))?
    else {
        return Ok(storage);
    };

    warn!(
        target: "reth::taiko::proof_history",
        ?path,
        reason,
        "discarding proof-history storage and rebuilding it from scratch"
    );
    drop(storage);
    remove_proof_history_store_files(path).wrap_err_with(|| {
        format!(
            "failed to delete proof-history storage files at {path:?}; delete {} there by hand and restart",
            PROOF_HISTORY_STORE_FILES.join(", ")
        )
    })?;
    open_mdbx(path)
}

/// Returns why `storage` must be discarded and rebuilt, or `None` when it is usable.
fn rebuild_reason(storage: &MdbxProofsStorage) -> eyre::Result<Option<&'static str>> {
    if storage.holds_only_legacy_v1_data()? {
        return Ok(Some("it holds only legacy v1 proof-history data"));
    }
    if proof_history_storage_needs_initialization(storage)? &&
        !matches!(
            storage.initialization_provider()?.initial_state_anchor()?.status,
            InitialStateStatus::NotStarted
        )
    {
        return Ok(Some("it holds an incomplete current-state initialization"));
    }
    Ok(None)
}

/// Files the proof-history store and older alethia releases create inside the storage directory:
/// the MDBX data and lock files, reth's database version file, and the removed historical-init
/// marker.
const PROOF_HISTORY_STORE_FILES: [&str; 4] =
    ["mdbx.dat", "mdbx.lck", "database.version", "taiko-historical-init-target"];

/// Deletes [`PROOF_HISTORY_STORE_FILES`] inside `path`, skipping missing ones and leaving the
/// directory and every other entry in place.
fn remove_proof_history_store_files(path: &Path) -> io::Result<()> {
    for name in PROOF_HISTORY_STORE_FILES {
        match fs::remove_file(path.join(name)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(())
}

/// Opens the proof-history MDBX store at `path`, creating it when missing.
fn open_mdbx(path: &Path) -> eyre::Result<Arc<MdbxProofsStorage>> {
    MdbxProofsStorage::new(path)
        .map(Arc::new)
        .wrap_err_with(|| format!("failed to create proof-history MDBX at {path:?}"))
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

/// Decision for delayed proof-history initialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DelayedProofHistoryStart {
    /// No finalized block has been observed yet.
    WaitForFinalized,
    /// Local execution has not reached the derived proof-history start block.
    WaitForExecution {
        /// First block where proof-history should initialize.
        start_block: u64,
    },
    /// Local execution has passed the derived proof-history start block.
    MissedStart {
        /// First block where proof-history should have initialized.
        start_block: u64,
    },
    /// Local execution is exactly at the derived proof-history start block.
    Ready {
        /// First block where proof-history should initialize.
        start_block: u64,
    },
}

/// Returns the first block retained by a finalized proof-history window.
pub(super) const fn proof_history_window_start_block(finalized_block: u64, window: u64) -> u64 {
    finalized_block.saturating_sub(window)
}

/// Computes whether delayed proof-history initialization can start.
pub(super) fn delayed_proof_history_start(
    finalized_block: Option<u64>,
    executed_head: u64,
    window: u64,
) -> DelayedProofHistoryStart {
    let Some(finalized_block) = finalized_block else {
        return DelayedProofHistoryStart::WaitForFinalized;
    };

    let start_block = proof_history_window_start_block(finalized_block, window);
    match executed_head.cmp(&start_block) {
        std::cmp::Ordering::Less => DelayedProofHistoryStart::WaitForExecution { start_block },
        std::cmp::Ordering::Equal => DelayedProofHistoryStart::Ready { start_block },
        std::cmp::Ordering::Greater => DelayedProofHistoryStart::MissedStart { start_block },
    }
}

/// Returns the persisted DB block used to label current-state proof-history initialization.
fn proof_history_current_state_anchor<Provider>(provider: &Provider) -> eyre::Result<BlockNumHash>
where
    Provider: BlockNumReader + HeaderProvider,
{
    let best_number = provider.best_block_number()?;
    let best_header = provider
        .sealed_header(best_number)?
        .ok_or_else(|| eyre!("missing proof-history current-state anchor header {best_number}"))?;
    Ok(BlockNumHash::new(best_number, best_header.hash()))
}

/// Initializes empty proof-history storage from the node's current canonical state.
///
/// Runs upstream [`InitializationJob`] at the on-disk best block, read from the same reth read
/// transaction the job copies from, so the anchor and the copied tables describe the same block.
pub(super) fn initialize_proof_history_storage<Provider, Storage>(
    provider: &Provider,
    storage: Storage,
) -> eyre::Result<()>
where
    Provider: DatabaseProviderFactory,
    Provider::Provider: BlockNumReader + HeaderProvider + StorageSettingsCache,
    <Provider::DB as Database>::TX: Sync,
    Storage: OpProofsStore + Send,
{
    if !proof_history_storage_needs_initialization(&storage)? {
        return Ok(());
    }

    let db_provider = provider.database_provider_ro()?.disable_long_read_transaction_safety();
    let anchor = proof_history_current_state_anchor(&db_provider)?;
    info!(
        target: "reth::taiko::proof_history",
        best_number = anchor.number,
        best_hash = ?anchor.hash,
        "initializing proof-history storage from current canonical state"
    );

    let layout = if db_provider.cached_storage_settings().is_v2() {
        RethTrieStorageLayout::Packed
    } else {
        RethTrieStorageLayout::Legacy
    };
    InitializationJob::new(storage, db_provider.into_tx(), layout)
        .run(anchor.number, anchor.hash)?;

    info!(
        target: "reth::taiko::proof_history",
        best_number = anchor.number,
        best_hash = ?anchor.hash,
        "proof-history storage initialized"
    );

    Ok(())
}

/// Maximum number of blocks one reth read transaction serves while backfilling proof history.
///
/// Each chunk opens a fresh reth provider so a long backfill does not pin one MDBX reader (and the
/// pages it keeps alive) for the whole window.
pub(super) const PROOF_HISTORY_BACKFILL_CHUNK_BLOCKS: u64 = 10_000;

/// Extends proof-history storage backward from reth changesets until it retains the window the
/// pruner keeps.
///
/// The target is `max(latest_stored, on_disk_best) - window` (saturating), where `on_disk_best` is
/// reth's persisted best block; both are re-read for every chunk. The pruner keeps
/// `latest_stored - window`, and the sync loop later raises `latest_stored` to reth's head, so
/// anchoring on the higher of the two stops the backfill from filling blocks the pruner will drop:
/// after `UnwindToEarliest` the store is `[E, E]` while reth's head is still far above `E`, and a
/// target of `E - window` would backfill up to a whole extra window (and need reth changesets for
/// it). The target never depends on the stored earliest block, so an interrupted backfill
/// converges on retry.
///
/// Upstream [`BackfillJob`] prepends one block at a time and validates each block's state root
/// against the reth header, committing in small batches. The stored earliest block is the
/// progress marker, so an interrupted backfill resumes where it stopped. Returns immediately when
/// the window is already full. Runs in chunks of `chunk_blocks` blocks, each against a fresh reth
/// provider, and logs the overall progress once per chunk.
pub(super) fn backfill_proof_history_window<Provider, Storage>(
    provider: &Provider,
    storage: Storage,
    window: u64,
    chunk_blocks: u64,
) -> eyre::Result<()>
where
    Provider: DatabaseProviderFactory,
    Provider::Provider: BlockHashReader
        + BlockNumReader
        + ChangeSetReader
        + HeaderProvider
        + StageCheckpointReader
        + StorageChangeSetReader
        + StorageSettingsCache
        + Send,
    Storage: OpProofsBackfillStore + Clone + Send,
{
    loop {
        let ProofWindowRange { earliest, latest } = storage.provider_ro()?.get_proof_window()?;
        let db_provider = provider.database_provider_ro()?.disable_long_read_transaction_safety();
        let on_disk_best = db_provider.best_block_number()?;
        let target = latest.number.max(on_disk_best).saturating_sub(window);
        if earliest.number <= target {
            return Ok(());
        }

        let chunk_target = target.max(earliest.number.saturating_sub(chunk_blocks.max(1)));
        info!(
            target: "reth::taiko::proof_history",
            earliest = earliest.number,
            latest = latest.number,
            on_disk_best,
            target,
            chunk_target,
            remaining = earliest.number - target,
            "backfilling proof-history window from reth changesets"
        );
        BackfillJob::new(db_provider, storage.clone()).run(chunk_target).map_err(backfill_error)?;
    }
}

/// Converts a backfill failure into an error that tells the operator how to recover.
fn backfill_error(error: BackfillError) -> eyre::Report {
    let hint = match &error {
        BackfillError::StateRootMismatch { .. } => {
            "; wipe the proof-history storage directory and restart to rebuild it"
        }
        BackfillError::BlockBodyPruned(_) => {
            "; the backfill window is set by --proofs-history.window"
        }
        _ => "",
    };
    eyre!("proof-history backfill failed: {error}{hint}")
}

/// Returns the latest finalized block number recorded in the node database.
pub(super) fn finalized_block_number<Provider>(provider: &Provider) -> eyre::Result<Option<u64>>
where
    Provider: DatabaseProviderFactory,
    Provider::Provider: ChainStateBlockReader,
{
    Ok(provider.database_provider_ro()?.last_finalized_block_number()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_eips::eip1898::BlockWithParent;
    use alloy_primitives::B256;
    use reth_db::mdbx::{DatabaseArguments, DatabaseFlags, init_db_for};
    use reth_ethereum_primitives::EthPrimitives;
    use reth_optimism_trie::{
        InMemoryProofsStorage, OpProofsStorage, db::Tables, proof::DatabaseStateRoot,
    };
    use reth_primitives_traits::SealedHeader;
    use reth_provider::{
        BlockWriter, ExecutionOutcome, ProviderFactory, StageCheckpointWriter,
        test_utils::{
            MockEthProvider, MockNodeTypesWithDB, blocks::BlockchainTestData,
            create_test_provider_factory,
        },
    };
    use reth_trie::{HashedPostState, KeccakKeyHasher, StateRoot};

    #[test]
    fn proof_history_storage_initialization_check_tracks_empty_storage() {
        let storage: OpProofsStorage<InMemoryProofsStorage> =
            InMemoryProofsStorage::default().into();

        assert!(proof_history_storage_needs_initialization(&storage).unwrap());

        let initializer = storage.initialization_provider().expect("initialization provider");
        initializer
            .set_initial_state_anchor(alloy_eips::BlockNumHash::new(0, B256::ZERO))
            .expect("set initial state anchor");
        initializer.commit_initial_state().expect("commit initial state");
        initializer.commit().expect("commit");

        assert!(!proof_history_storage_needs_initialization(&storage).unwrap());
    }

    #[test]
    fn proof_history_current_state_anchor_uses_provider_best_header() {
        let provider = MockEthProvider::<EthPrimitives>::new();
        provider.add_header(B256::with_last_byte(10), Header { number: 10, ..Header::default() });
        provider.add_header(B256::with_last_byte(12), Header { number: 12, ..Header::default() });

        let anchor = proof_history_current_state_anchor(&provider).unwrap();
        let expected_hash = provider.sealed_header(12).unwrap().unwrap().hash();

        assert_eq!(anchor, BlockNumHash::new(12, expected_hash));
    }

    #[test]
    fn proof_history_backfill_waits_when_executed_head_has_no_next_parent_state() {
        // Nothing is executed locally beyond the stored anchor, so there is nothing to backfill
        // regardless of how far ahead the notified canonical tip is.
        assert_eq!(proof_history_sync_target(0, 0), None);
    }

    #[test]
    fn proof_history_window_start_saturates_at_genesis() {
        assert_eq!(proof_history_window_start_block(100, 350_000), 0);
    }

    #[test]
    fn proof_history_window_start_subtracts_window() {
        assert_eq!(proof_history_window_start_block(1_000_000, 350_000), 650_000);
    }

    #[test]
    fn delayed_proof_history_initialization_waits_without_finalized_head() {
        assert_eq!(
            delayed_proof_history_start(None, 900_000, 350_000),
            DelayedProofHistoryStart::WaitForFinalized
        );
    }

    #[test]
    fn delayed_proof_history_initialization_waits_until_local_execution_reaches_start() {
        assert_eq!(
            delayed_proof_history_start(Some(1_000_000), 649_999, 350_000),
            DelayedProofHistoryStart::WaitForExecution { start_block: 650_000 }
        );
    }

    #[test]
    fn delayed_proof_history_initialization_reports_missed_start() {
        assert_eq!(
            delayed_proof_history_start(Some(1_000_000), 650_001, 350_000),
            DelayedProofHistoryStart::MissedStart { start_block: 650_000 }
        );
    }

    #[test]
    fn delayed_proof_history_initialization_starts_when_execution_reaches_window_start() {
        assert_eq!(
            delayed_proof_history_start(Some(1_000_000), 650_000, 350_000),
            DelayedProofHistoryStart::Ready { start_block: 650_000 }
        );
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

    /// Creates the removed v1 `ProofWindow` table with one row in the proofs database at `path`,
    /// mirroring what older releases left on disk (OP `create_legacy_tables`).
    fn write_legacy_v1_table(path: &Path) {
        let env = init_db_for::<_, Tables>(path, DatabaseArguments::default())
            .expect("raw proofs database opens");
        let tx = env.begin_rw_txn().expect("write transaction opens");
        let table = tx
            .create_db(Some("ProofWindow"), DatabaseFlags::default())
            .expect("legacy table is created");
        tx.put(table.dbi(), [0u8], [1u8], Default::default()).expect("legacy row writes");
        tx.commit().expect("legacy table commits");
    }

    /// Opens `path`, records an initial-state anchor at block 5, and commits the initial state only
    /// when `complete` is set, leaving an interrupted copy otherwise.
    fn open_and_initialize(path: &Path, complete: bool) {
        let storage = open_proof_history_storage(path).unwrap();
        let initializer = storage.initialization_provider().unwrap();
        initializer
            .set_initial_state_anchor(BlockNumHash::new(5, B256::with_last_byte(5)))
            .unwrap();
        if complete {
            initializer.commit_initial_state().unwrap();
        }
        initializer.commit().unwrap();
    }

    #[test]
    fn open_wipes_v1_only_directory() {
        let dir = tempfile::tempdir().unwrap();
        write_legacy_v1_table(dir.path());
        let marker = dir.path().join("taiko-historical-init-target");
        fs::write(&marker, "version=1\n").unwrap();

        let storage = open_proof_history_storage(dir.path()).unwrap();

        assert!(!storage.holds_only_legacy_v1_data().unwrap());
        assert!(!marker.exists());
        assert!(proof_history_storage_needs_initialization(&storage).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn open_wipes_in_place_keeping_the_directory() {
        // The wipe must delete the store's files, not the directory: a mount point cannot be
        // removed and a symlink would be unlinked, orphaning the data on the real volume. Entries
        // the store did not create survive, in case the path is shared with other data.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_legacy_v1_table(&link);
        let marker = real.join("taiko-historical-init-target");
        fs::write(&marker, "version=1\n").unwrap();
        fs::create_dir(real.join("stale-dir")).unwrap();
        fs::write(real.join("stale-dir").join("stale-file"), "x").unwrap();
        fs::write(real.join("jwt.hex"), "x").unwrap();

        let storage = open_proof_history_storage(&link).unwrap();

        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(!storage.holds_only_legacy_v1_data().unwrap());
        assert!(
            real.join("mdbx.dat").exists(),
            "the rebuilt store lives in the original directory"
        );
        assert!(!marker.exists());
        assert!(real.join("stale-dir").join("stale-file").exists());
        assert!(real.join("jwt.hex").exists());
    }

    #[test]
    fn open_wipes_incomplete_initialization() {
        let dir = tempfile::tempdir().unwrap();
        open_and_initialize(dir.path(), false);

        let storage = open_proof_history_storage(dir.path()).unwrap();

        let anchor = storage.initialization_provider().unwrap().initial_state_anchor().unwrap();
        assert!(matches!(anchor.status, InitialStateStatus::NotStarted));
        assert!(anchor.block.is_none());
    }

    #[test]
    fn open_keeps_initialized_store() {
        let dir = tempfile::tempdir().unwrap();
        open_and_initialize(dir.path(), true);

        let storage = open_proof_history_storage(dir.path()).unwrap();

        assert_eq!(storage.provider_ro().unwrap().get_earliest_block().unwrap().number, 5);
    }

    #[test]
    fn open_creates_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist");

        let storage = open_proof_history_storage(&path).unwrap();

        assert!(proof_history_storage_needs_initialization(&storage).unwrap());
    }

    /// Reth database holding blocks 1-4 of reth's test chain (with real changesets) on top of its
    /// genesis, with the Finish checkpoint at block 4. `headers[n]` is block `n`.
    ///
    /// Block 3 only creates accounts and storage; block 4 destroys half of them. The fixture's
    /// block 5 is left out: its reverts do not record the accounts it recreates, so block 4's
    /// state cannot be rebuilt from its changesets.
    fn chain_fixture() -> (ProviderFactory<MockNodeTypesWithDB>, Vec<SealedHeader>) {
        let mut data = BlockchainTestData::default();
        data.blocks.truncate(4);
        let factory = create_test_provider_factory();
        let provider_rw = factory.provider_rw().unwrap();
        // Genesis carries an empty receipt list so the receipts static file starts at block 0.
        provider_rw
            .append_blocks_with_state(
                vec![data.genesis.clone().try_recover().unwrap()],
                &ExecutionOutcome { first_block: 0, receipts: vec![vec![]], ..Default::default() },
                Default::default(),
            )
            .unwrap();
        for (block, outcome) in &data.blocks {
            provider_rw
                .append_blocks_with_state(
                    vec![block.clone()],
                    outcome,
                    outcome.hash_state_slow::<KeccakKeyHasher>().into_sorted(),
                )
                .unwrap();
        }
        provider_rw.update_pipeline_stages(4, false).unwrap();
        provider_rw.commit().unwrap();

        let headers = std::iter::once(data.genesis.clone_sealed_header())
            .chain(data.blocks.iter().map(|(block, _)| block.clone_sealed_header()))
            .collect();
        (factory, headers)
    }

    /// Earliest block number retained in `storage`.
    fn earliest(storage: &Arc<MdbxProofsStorage>) -> u64 {
        storage.provider_ro().unwrap().get_earliest_block().unwrap().number
    }

    #[test]
    fn window_only_init_backfills_to_latest_minus_window() {
        let (factory, headers) = chain_fixture();
        let dir = tempfile::tempdir().unwrap();
        let storage = open_proof_history_storage(dir.path()).unwrap();

        initialize_proof_history_storage(&factory, storage.clone()).unwrap();
        backfill_proof_history_window(
            &factory,
            storage.clone(),
            3,
            PROOF_HISTORY_BACKFILL_CHUNK_BLOCKS,
        )
        .unwrap();

        let ro = storage.provider_ro().unwrap();
        assert_eq!(ro.get_earliest_block().unwrap(), BlockNumHash::new(1, headers[1].hash()));
        assert_eq!(ro.get_latest_block().unwrap(), BlockNumHash::new(4, headers[4].hash()));
        for n in 1..=4 {
            let root = StateRoot::overlay_root(&ro, n, HashedPostState::default()).unwrap();
            assert_eq!(root, headers[n as usize].state_root, "state root at block {n}");
        }
    }

    #[test]
    fn backfill_resumes_and_extends_after_window_increase() {
        let (factory, _) = chain_fixture();
        let dir = tempfile::tempdir().unwrap();
        let storage = open_proof_history_storage(dir.path()).unwrap();
        initialize_proof_history_storage(&factory, storage.clone()).unwrap();

        backfill_proof_history_window(&factory, storage.clone(), 1, 1).unwrap();
        assert_eq!(earliest(&storage), 3);

        // The window is already full: a restart must not move it.
        backfill_proof_history_window(&factory, storage.clone(), 1, 1).unwrap();
        assert_eq!(earliest(&storage), 3);

        // A larger window resumes from the stored earliest block, one chunk at a time.
        backfill_proof_history_window(&factory, storage.clone(), 3, 1).unwrap();
        assert_eq!(earliest(&storage), 1);
    }

    #[test]
    fn backfill_targets_reth_head_after_unwind() {
        use reth_optimism_trie::api::OpProofsProviderRw;

        let (factory, headers) = chain_fixture();
        let dir = tempfile::tempdir().unwrap();
        let storage = open_proof_history_storage(dir.path()).unwrap();
        initialize_proof_history_storage(&factory, storage.clone()).unwrap();
        backfill_proof_history_window(&factory, storage.clone(), 2, 1).unwrap();
        assert_eq!(earliest(&storage), 2);

        // Unwind the store to its earliest block, as the sidecar's `UnwindToEarliest` does.
        let provider_rw = storage.provider_rw().unwrap();
        provider_rw
            .unwind_history(BlockWithParent::new(
                headers[2].hash(),
                BlockNumHash::new(3, headers[3].hash()),
            ))
            .unwrap();
        OpProofsProviderRw::commit(provider_rw).unwrap();
        let ro = storage.provider_ro().unwrap();
        assert_eq!(ro.get_earliest_block().unwrap().number, 2);
        assert_eq!(ro.get_latest_block().unwrap().number, 2);
        drop(ro);

        // Reth's head is still 4: the pruner will keep `[2, 4]`, so nothing below 2 is backfilled.
        backfill_proof_history_window(&factory, storage.clone(), 2, 1).unwrap();
        assert_eq!(earliest(&storage), 2);

        backfill_proof_history_window(&factory, storage.clone(), 3, 1).unwrap();
        assert_eq!(earliest(&storage), 1);
    }
}
