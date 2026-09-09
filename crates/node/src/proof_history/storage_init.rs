//! V2 proof-history snapshot initialization and cancellable backward backfill.

use super::store::ProofHistoryDatabase;
use alloy_consensus::BlockHeader;
use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use eyre::{WrapErr, eyre};
use reth_db::Database;
use reth_optimism_trie::{
    BackfillError, BackfillJob, InitializationJob, OpProofsBackfillProvider, OpProofsBackfillStore,
    OpProofsProviderRO, OpProofsSnapshotInitProvider, OpProofsStore, ProofWindowRange,
    RethTrieStorageLayout, SnapshotInitJob, SnapshotInitStatus,
    backfill::DEFAULT_BACKFILL_BATCH_SIZE, proof::DatabaseStateRoot, snapshot::SnapshotError,
};
use reth_provider::{
    BlockHashReader, BlockNumReader, ChainStateBlockReader, ChangeSetReader, DBProvider,
    DatabaseProviderFactory, HeaderProvider, ProviderError, StageCheckpointReader,
    StorageChangeSetReader, StorageSettingsCache,
};
use reth_stages_types::StageId;
use reth_trie::StateRoot;
use std::{fs, io, path::Path, sync::Arc};
use tracing::{info, warn};

/// Copies a single persisted current-state snapshot using the upstream initialization job.
/// Trie tables share one MDBX snapshot, while headers are backed by reth's shared static files.
/// A root mismatch discards the copy, then waits if the header changed or fails with repair
/// guidance if it is stable. Canonical reconciliation precedes reads. Bulk rows bypass metrics.
pub(super) fn initialize_proof_history_storage<Provider>(
    provider: &Provider,
    storage: Arc<ProofHistoryDatabase>,
    backfill: Option<(&Path, u64)>,
) -> eyre::Result<bool>
where
    Provider: DatabaseProviderFactory,
    Provider::Provider: BlockNumReader
        + ChainStateBlockReader
        + HeaderProvider
        + StorageSettingsCache
        + StageCheckpointReader,
    <Provider::DB as Database>::TX: Sync,
{
    let db = provider.database_provider_ro()?.disable_long_read_transaction_safety();
    let number = db.best_block_number()?;
    for stage in [
        StageId::Execution,
        StageId::AccountHashing,
        StageId::StorageHashing,
        StageId::MerkleExecute,
    ] {
        let checkpoint = db.get_stage_checkpoint(stage)?.unwrap_or_default().block_number;
        if checkpoint != number {
            info!(target: "reth::taiko::proof_history", ?stage, checkpoint, finish = number,
            "waiting for consistent pipeline state before proof-history snapshot");
            return Ok(false);
        }
    }
    if db.get_stage_checkpoint_progress(StageId::MerkleExecute)?.is_some_and(|p| !p.is_empty()) {
        info!(target: "reth::taiko::proof_history", number, "waiting for partial Merkle execution to finish");
        return Ok(false);
    }
    let Some(header) = db.sealed_header(number)? else {
        warn!(target: "reth::taiko::proof_history", number, "waiting for missing proof-history snapshot header at Finish");
        return Ok(false);
    };
    let layout = if db.cached_storage_settings().is_v2() {
        RethTrieStorageLayout::Packed
    } else {
        RethTrieStorageLayout::Legacy
    };
    if let Some((path, window)) = backfill {
        let finalized = db.last_finalized_block_number()?.unwrap_or(number);
        let target = number.max(finalized).saturating_sub(window).min(number);
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, target.to_string())?;
        fs::File::open(&temporary)?.sync_all()?;
        fs::rename(temporary, path)?;
        sync_backfill_directory(path)?;
    }
    storage.record_hashes([(number, header.hash())])?;
    InitializationJob::new(storage.clone(), db.into_tx(), layout).run(number, header.hash())?;
    let root = StateRoot::overlay_root(storage.provider_ro()?, number, Default::default())?;
    if root != header.state_root() {
        return reject_initial_snapshot(
            &storage,
            BlockNumHash::new(number, header.hash()),
            root,
            header.state_root(),
            || Ok(provider.database_provider_ro()?.sealed_header(number)?.map(|h| h.hash())),
        );
    }

    info!(target: "reth::taiko::proof_history", number, "initialized proof-history snapshot");
    Ok(true)
}

/// Discards a failed initial copy and checks fresh header identity before choosing recovery.
/// A moved/missing header waits for reconciliation; a stable mismatch fails without recopying.
fn reject_initial_snapshot(
    storage: &ProofHistoryDatabase,
    anchor: BlockNumHash,
    computed: B256,
    expected: B256,
    fresh_hash: impl FnOnce() -> eyre::Result<Option<B256>>,
) -> eyre::Result<bool> {
    storage.reset_bootstrap()?;
    if fresh_hash()? != Some(anchor.hash) {
        warn!(target: "reth::taiko::proof_history", block = anchor.number, actual = ?computed,
            ?expected, "snapshot header changed; discarded the invalid copy and waiting to retry");
        return Ok(false);
    }
    Err(eyre!(
        "proof-history snapshot state root mismatch at block {} ({:?}): computed {computed:?}, \
         expected {expected:?}; the invalid copy was discarded. Verify or repair the node's \
         source trie/hashed state and storage layout before restarting; copying the same source \
         again cannot repair it",
        anchor.number,
        anchor.hash
    ))
}

/// Reads the pending backward-bootstrap target. Absence means bootstrap is complete or disabled.
pub(super) fn pending_backfill_target(path: &Path) -> eyre::Result<Option<u64>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(value.parse().wrap_err("invalid proof-history backfill target")?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Marks backward bootstrap complete before starting live indexing, preserving normal pruning.
pub(super) fn finish_backfill(path: &Path) -> eyre::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    sync_backfill_directory(path)
}

/// Persists marker creation/removal before MDBX publication or live indexing can advance.
fn sync_backfill_directory(path: &Path) -> eyre::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Advances backward by at most 10,000 blocks, then releases the pinned node read transaction.
/// Preparation revalidates canonical bounds before the next chunk. Upstream commits atomic
/// batches and checks cancellation at their boundaries; tiny fresh ranges avoid a second copy.
/// Returns false when source/canonical reconciliation needs a delayed retry.
pub(super) fn backfill_proof_history_storage<Provider>(
    provider: &Provider,
    storage: Arc<ProofHistoryDatabase>,
    target: u64,
) -> eyre::Result<bool>
where
    Provider: DatabaseProviderFactory + HeaderProvider + BlockHashReader + Sync,
    Provider::Provider: BlockNumReader
        + BlockHashReader
        + HeaderProvider
        + DBProvider
        + ChangeSetReader
        + StorageChangeSetReader
        + StorageSettingsCache
        + StageCheckpointReader
        + Send
        + Sync,
{
    backfill_proof_history_chunk(provider, storage, target, 10_000)
}

/// Runs one bounded backward chunk from a fresh, canonically validated node read transaction.
/// `max_blocks` is positive and limits retained MDBX pages between source-provider refreshes.
fn backfill_proof_history_chunk<Provider>(
    provider: &Provider,
    storage: Arc<ProofHistoryDatabase>,
    target: u64,
    max_blocks: u64,
) -> eyre::Result<bool>
where
    Provider: DatabaseProviderFactory + HeaderProvider + BlockHashReader + Sync,
    Provider::Provider: BlockNumReader
        + BlockHashReader
        + HeaderProvider
        + DBProvider
        + ChangeSetReader
        + StorageChangeSetReader
        + StorageSettingsCache
        + StageCheckpointReader
        + Send
        + Sync,
{
    storage.check_bootstrap_cancelled()?;
    let window = storage.provider_ro()?.get_proof_window()?;
    let earliest = window.earliest.number;
    if target >= earliest {
        return Ok(true);
    }
    if !source_matches_window(provider, window)? {
        warn!(target: "reth::taiko::proof_history", ?window, "canonical backfill bounds changed; waiting for reconciliation");
        return Ok(false);
    }
    let next = earliest.saturating_sub(max_blocks.max(1)).max(target);
    let Some(snapshot) = auxiliary_snapshot_status(&storage, window.earliest)? else {
        return Ok(false);
    };
    // Use the total remaining distance. An existing snapshot stays active through the last chunk.
    let use_snapshot = !matches!(snapshot, SnapshotInitStatus::NotStarted) ||
        earliest - target > DEFAULT_BACKFILL_BATCH_SIZE as u64;
    if use_snapshot && !matches!(snapshot, SnapshotInitStatus::Completed) {
        // The high-level provider opens short reads for the hash/header lookups. Passing a DB
        // provider here would pin its transaction throughout the full auxiliary copy.
        let result = SnapshotInitJob::new(provider, storage.clone())
            .run(earliest)
            .map(|_| ())
            .map_err(BackfillError::from);
        if !backfill_result(result, || {
            if auxiliary_snapshot_status(&storage, window.earliest)?.is_none() {
                return Ok(false);
            }
            source_matches_window(provider, window)
        })? || auxiliary_snapshot_status(&storage, window.earliest)?.is_none()
        {
            return Ok(false);
        }
    }
    // Open the long-lived source only after auxiliary snapshot initialization/resumption.
    let db = provider.database_provider_ro()?.disable_long_read_transaction_safety();
    if !source_matches_window(provider, window)? || !source_matches_window(&db, window)? {
        warn!(target: "reth::taiko::proof_history", ?window, "waiting for persisted backfill source to match canonical bounds");
        return Ok(false);
    }
    // Journal failures and missing hashes are actionable errors, even during a concurrent reorg.
    // Write only below committed earliest; a crash leaves harmless extra rows for this chunk.
    for start in (next..earliest).step_by(1000) {
        storage.check_bootstrap_cancelled()?;
        let end = start.saturating_add(1000).min(earliest);
        let hashes = db.canonical_hashes_range(start, end)?;
        if hashes.len() as u64 != end - start {
            return Err(eyre!("missing canonical hashes for proof-history backfill {start}..{end}"));
        }
        storage.record_hashes((start..end).zip(hashes))?;
    }
    let job = BackfillJob::new(db, Arc::clone(&storage));
    let result = if use_snapshot { job.run_with_snapshot(next) } else { job.run(next) };
    let progressed = backfill_result(result, || {
        source_matches_window(provider, storage.provider_ro()?.get_proof_window()?)
    })?;
    if progressed {
        info!(target: "reth::taiko::proof_history", earliest = next, target,
            remaining = next - target, "proof-history backfill checkpoint");
    }
    Ok(progressed)
}

/// Checks derived snapshot identity before resume and after upstream's separate header/hash reads.
/// A stale auxiliary anchor is discarded without changing retained proofs or their hash journal.
/// Returns None after clearing it so the caller retries after a delay.
fn auxiliary_snapshot_status(
    storage: &ProofHistoryDatabase,
    earliest: BlockNumHash,
) -> eyre::Result<Option<SnapshotInitStatus>> {
    let rw = storage.snapshot_initialization_provider()?;
    let snapshot = rw.snapshot_init_anchor()?;
    if snapshot.block.is_some_and(|anchor| anchor != earliest) {
        warn!(target: "reth::taiko::proof_history", expected = ?earliest, actual = ?snapshot.block,
            "discarding stale auxiliary snapshot; retained proof history is unchanged");
        rw.clear_snapshot()?;
        OpProofsBackfillProvider::commit(rw)?;
        return Ok(None);
    }
    Ok(Some(snapshot.status))
}

/// Checks both retained anchors in a canonical or persisted provider view.
fn source_matches_window(
    provider: &impl BlockHashReader,
    window: ProofWindowRange,
) -> eyre::Result<bool> {
    Ok(provider.block_hash(window.earliest.number)? == Some(window.earliest.hash) &&
        provider.block_hash(window.latest.number)? == Some(window.latest.hash))
}

/// Recovers only header/root races proven by changed canonical or auxiliary anchors, logging the
/// cause. Pruning, journal and storage failures remain actionable even if a reorg happened
/// concurrently.
fn backfill_result(
    result: Result<(), BackfillError>,
    still_canonical: impl FnOnce() -> eyre::Result<bool>,
) -> eyre::Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(error) => {
            let reorg_error = matches!(
                &error,
                BackfillError::StateRootMismatch { .. } |
                    BackfillError::Provider(ProviderError::HeaderNotFound(_)) |
                    BackfillError::Snapshot(
                        SnapshotError::StateRootMismatch { .. } |
                            SnapshotError::Provider(ProviderError::HeaderNotFound(_)) |
                            SnapshotError::SnapshotResumeDriftDetected { .. }
                    )
            );
            if reorg_error && !still_canonical()? {
                warn!(target: "reth::taiko::proof_history", %error,
                    "backfill interrupted by a canonical change; waiting for reconciliation");
                return Ok(false);
            }
            Err(error).wrap_err("failed to backfill proof history; required historical changesets must remain unpruned")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{super::store::ProofHistoryDatabase, *};
    use alloy_consensus::Header;
    use alloy_eips::BlockNumHash;
    use alloy_primitives::B256;
    use reth_optimism_trie::{
        OpProofsInitProvider, OpProofsProviderRO, api::InitialStateStatus, db::MdbxProofsStorage,
    };
    use std::sync::Arc;

    #[test]
    fn opening_legacy_storage_preserves_it_and_requires_a_new_path() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = MdbxProofsStorage::new(dir.path()).unwrap();
        let init = legacy.initialization_provider().unwrap();
        init.set_initial_state_anchor(BlockNumHash::new(7, B256::repeat_byte(7))).unwrap();
        init.commit_initial_state().unwrap();
        OpProofsInitProvider::commit(init).unwrap();
        drop(legacy);

        let result = ProofHistoryDatabase::open(dir.path());
        assert!(result.is_err());
        let legacy = MdbxProofsStorage::new(dir.path()).unwrap();
        assert_eq!(legacy.provider_ro().unwrap().get_latest_block().unwrap().number, 7);
    }

    #[test]
    fn opening_v2_storage_restarts_only_an_unpublished_snapshot() {
        use reth_optimism_trie::db::MdbxProofsStorageV2;
        let dir = tempfile::tempdir().unwrap();
        let storage = MdbxProofsStorageV2::new(dir.path()).unwrap();
        let init = storage.initialization_provider().unwrap();
        init.set_initial_state_anchor(BlockNumHash::new(7, B256::repeat_byte(7))).unwrap();
        init.store_hashed_accounts(vec![(B256::repeat_byte(1), Some(Default::default()))]).unwrap();
        OpProofsInitProvider::commit(init).unwrap();
        drop(storage);

        let storage = ProofHistoryDatabase::open(dir.path()).unwrap();
        let init = storage.initialization_provider().unwrap();
        let anchor = init.initial_state_anchor().unwrap();
        assert!(matches!(anchor.status, InitialStateStatus::NotStarted));
        assert!(anchor.latest_hashed_account_key.is_none());
        init.set_initial_state_anchor(BlockNumHash::new(9, B256::repeat_byte(9))).unwrap();
        init.commit_initial_state().unwrap();
        OpProofsInitProvider::commit(init).unwrap();
        drop(storage);

        let storage = ProofHistoryDatabase::open(dir.path()).unwrap();
        assert_eq!(storage.provider_ro().unwrap().get_latest_block().unwrap().number, 9);
    }

    #[test]
    fn historical_backfill_extends_a_completed_snapshot_and_resumes() {
        use alloy_consensus::{SignableTransaction, TxEip2930};
        use alloy_primitives::{Address, TxKind, U256};
        use reth_chainspec::{ChainSpecBuilder, MAINNET};
        use reth_db_common::init::init_genesis;
        use reth_ethereum_primitives::{Block, BlockBody, TransactionSigned};
        use reth_evm::{ConfigureEvm, execute::Executor};
        use reth_evm_ethereum::EthEvmConfig;
        use reth_primitives_traits::{
            Block as _, SignerRecoverable, crypto::secp256k1::sign_message,
        };
        use reth_provider::{
            BlockWriter, ChainStateBlockWriter, ExecutionOutcome, HashedPostStateProvider,
            LatestStateProviderRef, StateProofProvider, StateRootProvider,
            test_utils::create_test_provider_factory_with_chain_spec,
        };
        use reth_revm::database::StateProviderDatabase;

        let recipient = Address::repeat_byte(0x99);
        let transaction = |nonce| -> TransactionSigned {
            let tx = TxEip2930 {
                chain_id: 1,
                nonce,
                gas_limit: 21_000,
                gas_price: 1_500_000_000,
                to: TxKind::Call(recipient),
                value: U256::from(1),
                ..Default::default()
            };
            let signature = sign_message(B256::repeat_byte(0x42), tx.signature_hash()).unwrap();
            tx.into_signed(signature).into()
        };
        let sender = transaction(0).recover_signer().unwrap();
        let mut genesis = MAINNET.genesis.clone();
        genesis.alloc.clear();
        genesis.alloc.entry(sender).or_default().balance = U256::from(10_u64.pow(18));
        let spec = Arc::new(ChainSpecBuilder::mainnet().genesis(genesis).paris_activated().build());
        let factory = create_test_provider_factory_with_chain_spec(spec.clone());
        init_genesis(&factory).unwrap();
        let mut parent = spec.genesis_hash();
        let mut roots = vec![spec.genesis_header().state_root];
        for number in 1..=3 {
            let mut block = Block {
                header: Header {
                    number,
                    parent_hash: parent,
                    gas_limit: 21_000,
                    gas_used: 21_000,
                    ..Default::default()
                },
                body: BlockBody {
                    transactions: vec![transaction(number - 1)],
                    ..Default::default()
                },
            }
            .try_into_recovered()
            .unwrap();
            let db = factory.database_provider_ro().unwrap();
            let state = LatestStateProviderRef::new(&db);
            let execution = EthEvmConfig::ethereum(spec.clone())
                .batch_executor(StateProviderDatabase::new(&state))
                .execute(&block)
                .unwrap();
            let hashed = state.hashed_post_state(&execution.state);
            let root = state.state_root(hashed.clone()).unwrap();
            roots.push(root);
            block.set_state_root(root);
            parent = block.hash();
            let provider = factory.database_provider_rw().unwrap();
            provider
                .append_blocks_with_state(
                    vec![block],
                    &ExecutionOutcome {
                        first_block: number,
                        bundle: execution.state.clone(),
                        receipts: vec![execution.receipts.clone()],
                        requests: vec![execution.requests.clone()],
                    },
                    hashed.into_sorted(),
                )
                .unwrap();
            provider.commit().unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(ProofHistoryDatabase::open(dir.path()).unwrap());
        let db = factory.database_provider_rw().unwrap();
        db.save_finalized_block_number(5).unwrap();
        db.commit().unwrap();
        let target_path = dir.path().join("backfill-target");
        assert!(
            initialize_proof_history_storage(&factory, storage.clone(), Some((&target_path, 3)))
                .unwrap()
        );
        assert_eq!(storage.provider_ro().unwrap().get_earliest_block().unwrap().number, 3);
        // During partial sync, do not backfill older than the finalized window start (5 - 3).
        assert_eq!(pending_backfill_target(&target_path).unwrap(), Some(2));

        // An obsolete proof anchor must return to canonical reconciliation before a fresh
        // source provider can journal or backfill any part of a replacement branch.
        let stale_dir = tempfile::tempdir().unwrap();
        let stale = Arc::new(ProofHistoryDatabase::open(stale_dir.path()).unwrap());
        let init = stale.initialization_provider().unwrap();
        init.set_initial_state_anchor(BlockNumHash::new(3, B256::repeat_byte(0xee))).unwrap();
        init.commit_initial_state().unwrap();
        OpProofsInitProvider::commit(init).unwrap();
        assert!(!backfill_proof_history_storage(&factory, stale.clone(), 2).unwrap());
        assert_eq!(stale.provider_ro().unwrap().get_earliest_block().unwrap().number, 3);
        assert_eq!(stale.indexed_hash(2).unwrap(), None);

        storage.cancel_bootstrap();
        assert!(backfill_proof_history_storage(&factory, storage.clone(), 2).is_err());
        assert_eq!(storage.provider_ro().unwrap().get_earliest_block().unwrap().number, 3);
        storage.resume_bootstrap();
        backfill_proof_history_storage(&factory, storage.clone(), 2).unwrap();
        assert_eq!(storage.provider_ro().unwrap().get_earliest_block().unwrap().number, 2);
        use reth_optimism_trie::{
            OpProofsBackfillStore, OpProofsSnapshotInitProvider, SnapshotInitStatus,
        };
        assert!(
            matches!(
                storage
                    .snapshot_initialization_provider()
                    .unwrap()
                    .snapshot_init_anchor()
                    .unwrap()
                    .status,
                SnapshotInitStatus::NotStarted
            ),
            "a one-block backfill must not duplicate the entire state"
        );
        drop(storage);
        let storage = Arc::new(ProofHistoryDatabase::open(dir.path()).unwrap());
        assert_eq!(pending_backfill_target(&target_path).unwrap(), Some(2));
        finish_backfill(&target_path).unwrap();
        assert_eq!(pending_backfill_target(&target_path).unwrap(), None);
        reth_optimism_trie::SnapshotInitJob::new(
            factory.database_provider_ro().unwrap(),
            storage.clone(),
        )
        .run(2)
        .unwrap();
        backfill_proof_history_chunk(&factory, storage.clone(), 0, 1).unwrap();
        assert_eq!(storage.provider_ro().unwrap().get_earliest_block().unwrap().number, 1);
        assert_eq!(
            storage
                .snapshot_initialization_provider()
                .unwrap()
                .snapshot_init_anchor()
                .unwrap()
                .block
                .unwrap()
                .number,
            1
        );
        drop(storage);
        let storage = Arc::new(ProofHistoryDatabase::open(dir.path()).unwrap());
        backfill_proof_history_chunk(&factory, storage.clone(), 0, 1).unwrap();
        assert_eq!(
            storage
                .snapshot_initialization_provider()
                .unwrap()
                .snapshot_init_anchor()
                .unwrap()
                .block
                .unwrap()
                .number,
            0
        );
        let window = storage.provider_ro().unwrap().get_proof_window().unwrap();
        assert_eq!(window.earliest, BlockNumHash::new(0, spec.genesis_hash()));
        assert_eq!(window.latest, BlockNumHash::new(3, parent));

        std::fs::write(&target_path, "0").unwrap();
        let readiness = alethia_reth_rpc::proof_state::ProofHistoryReadiness::new();
        let sidecar = super::super::sidecar::ProofHistorySidecar::new(
            reth_provider::providers::BlockchainProvider::new(factory.clone()).unwrap(),
            EthEvmConfig::ethereum(spec.clone()),
            storage.clone().into(),
            storage.clone(),
            super::super::ProofHistoryConfig {
                storage_path: Some(dir.path().to_path_buf()),
                ..super::super::ProofHistoryConfig::disabled()
            },
            readiness.clone(),
        );
        let runtime = reth::tasks::Runtime::test();
        let task = runtime.spawn_with_graceful_shutdown_signal(move |shutdown| async move {
            sidecar.run(shutdown).await.unwrap();
        });
        runtime.handle().block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !readiness.is_ready() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        });
        assert!(!target_path.exists());
        assert!(matches!(
            storage
                .snapshot_initialization_provider()
                .unwrap()
                .snapshot_init_anchor()
                .unwrap()
                .status,
            SnapshotInitStatus::NotStarted
        ));
        assert!(runtime.graceful_shutdown_with_timeout(std::time::Duration::from_secs(5)));
        runtime.handle().block_on(task).unwrap();

        for (number, root) in roots.into_iter().enumerate() {
            let state = reth_optimism_trie::provider::OpProofsStateProviderRef::new(
                factory.latest().unwrap(),
                storage.provider_ro().unwrap(),
                number as u64,
            );
            let proof = state.proof(Default::default(), recipient, &[]).unwrap();
            assert_eq!(proof.info.unwrap_or_default().balance, U256::from(number));
            proof.verify(root).unwrap();
        }
    }
    #[test]
    fn initial_copy_mismatch_waits_only_when_the_header_moved() {
        for fresh in [None, Some(B256::repeat_byte(2)), Some(B256::repeat_byte(1))] {
            let dir = tempfile::tempdir().unwrap();
            let storage = ProofHistoryDatabase::open(dir.path()).unwrap();
            let anchor = BlockNumHash::new(7, B256::repeat_byte(1));
            let init = storage.initialization_provider().unwrap();
            init.set_initial_state_anchor(anchor).unwrap();
            init.commit_initial_state().unwrap();
            OpProofsInitProvider::commit(init).unwrap();
            storage.record_hashes([(7, anchor.hash)]).unwrap();
            let result =
                reject_initial_snapshot(&storage, anchor, B256::ZERO, B256::repeat_byte(9), || {
                    Ok(fresh)
                });
            if fresh == Some(anchor.hash) {
                assert!(result.unwrap_err().to_string().contains("copying the same source"));
            } else {
                assert!(!result.unwrap());
            }
            assert!(storage.provider_ro().unwrap().get_latest_block().is_err());
            assert_eq!(storage.indexed_hash(7).unwrap(), None);
        }
    }

    #[test]
    fn only_proven_header_or_root_races_retry_backfill() {
        for canonical in [false, true] {
            for error in [
                BackfillError::StateRootMismatch {
                    block_number: 1,
                    computed: B256::ZERO,
                    expected: B256::repeat_byte(1),
                },
                BackfillError::Provider(ProviderError::HeaderNotFound(1.into())),
                BackfillError::Snapshot(SnapshotError::StateRootMismatch {
                    block_number: 1,
                    computed: B256::ZERO,
                    expected: B256::repeat_byte(1),
                }),
                BackfillError::Snapshot(SnapshotError::Provider(ProviderError::HeaderNotFound(
                    1.into(),
                ))),
            ] {
                let result = backfill_result(Err(error), || Ok(canonical));
                if canonical {
                    assert!(result.is_err());
                } else {
                    assert!(!result.unwrap());
                }
            }
        }
        for error in [
            BackfillError::BlockBodyPruned(7),
            BackfillError::Storage(reth_db::DatabaseError::Other("disk failure".into()).into()),
        ] {
            let report = backfill_result(Err(error), || {
                panic!("unrelated errors must not inspect reorg state")
            })
            .unwrap_err();
            let report = format!("{report:#}");
            assert!(report.contains("pruned") || report.contains("disk failure"));
        }
    }

    fn empty_chain_factory(
        count: u64,
    ) -> (
        reth_provider::ProviderFactory<reth_provider::test_utils::MockNodeTypesWithDB>,
        Vec<BlockNumHash>,
    ) {
        use reth_chainspec::{ChainSpecBuilder, MAINNET};
        use reth_db_common::init::init_genesis;
        use reth_ethereum_primitives::{Block, BlockBody};
        use reth_primitives_traits::Block as _;
        use reth_provider::{
            BlockWriter, ExecutionOutcome, test_utils::create_test_provider_factory_with_chain_spec,
        };
        let mut genesis = MAINNET.genesis.clone();
        genesis.alloc.clear();
        let spec = Arc::new(ChainSpecBuilder::mainnet().genesis(genesis).paris_activated().build());
        let factory = create_test_provider_factory_with_chain_spec(spec.clone());
        init_genesis(&factory).unwrap();
        let mut hashes = vec![BlockNumHash::new(0, spec.genesis_hash())];
        let mut blocks = Vec::new();
        for number in 1..=count {
            let block = Block {
                header: Header {
                    number,
                    parent_hash: hashes.last().unwrap().hash,
                    state_root: spec.genesis_header().state_root,
                    ..Default::default()
                },
                body: BlockBody::default(),
            }
            .try_into_recovered()
            .unwrap();
            hashes.push(BlockNumHash::new(number, block.hash()));
            blocks.push(block);
        }
        let rw = factory.database_provider_rw().unwrap();
        rw.append_blocks_with_state(
            blocks,
            &ExecutionOutcome {
                first_block: 1,
                receipts: vec![vec![]; count as usize],
                requests: vec![Default::default(); count as usize],
                ..Default::default()
            },
            Default::default(),
        )
        .unwrap();
        rw.commit().unwrap();
        (factory, hashes)
    }

    #[test]
    fn backfill_chunks_cover_multiple_journal_batches_and_resume() {
        let (factory, hashes) = empty_chain_factory(1027);
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(ProofHistoryDatabase::open(dir.path()).unwrap());
        assert!(initialize_proof_history_storage(&factory, storage.clone(), None).unwrap());
        assert!(backfill_proof_history_chunk(&factory, storage.clone(), 0, 1001).unwrap());
        assert_eq!(storage.provider_ro().unwrap().get_earliest_block().unwrap(), hashes[26]);
        for anchor in &hashes[26..] {
            assert_eq!(storage.indexed_hash(anchor.number).unwrap(), Some(anchor.hash));
        }
        assert_eq!(storage.indexed_hash(25).unwrap(), None);
        drop(storage);
        let storage = Arc::new(ProofHistoryDatabase::open(dir.path()).unwrap());
        assert!(backfill_proof_history_chunk(&factory, storage.clone(), 0, 1001).unwrap());
        assert_eq!(storage.provider_ro().unwrap().get_earliest_block().unwrap(), hashes[0]);
        for anchor in &hashes {
            assert_eq!(storage.indexed_hash(anchor.number).unwrap(), Some(anchor.hash));
        }
    }

    #[test]
    fn fresh_backfill_uses_snapshot_only_above_one_upstream_batch() {
        for count in [DEFAULT_BACKFILL_BATCH_SIZE as u64, DEFAULT_BACKFILL_BATCH_SIZE as u64 + 1] {
            let (factory, _) = empty_chain_factory(count);
            let dir = tempfile::tempdir().unwrap();
            let storage = Arc::new(ProofHistoryDatabase::open(dir.path()).unwrap());
            assert!(initialize_proof_history_storage(&factory, storage.clone(), None).unwrap());
            assert!(backfill_proof_history_chunk(&factory, storage.clone(), 0, 1).unwrap());
            let status = storage
                .snapshot_initialization_provider()
                .unwrap()
                .snapshot_init_anchor()
                .unwrap()
                .status;
            assert_eq!(
                matches!(status, SnapshotInitStatus::Completed),
                count > DEFAULT_BACKFILL_BATCH_SIZE as u64
            );
            assert_eq!(
                storage.provider_ro().unwrap().get_earliest_block().unwrap().number,
                count - 1
            );
        }
    }
    #[test]
    fn stale_auxiliary_snapshots_are_rebuilt_without_resetting_retained_history() {
        for completed in [false, true] {
            let (factory, hashes) = empty_chain_factory(DEFAULT_BACKFILL_BATCH_SIZE as u64 + 1);
            let dir = tempfile::tempdir().unwrap();
            let storage = Arc::new(ProofHistoryDatabase::open(dir.path()).unwrap());
            assert!(initialize_proof_history_storage(&factory, storage.clone(), None).unwrap());
            let retained = storage.provider_ro().unwrap().get_proof_window().unwrap();
            let snapshot = storage.snapshot_initialization_provider().unwrap();
            snapshot
                .set_snapshot_init_anchor(BlockNumHash::new(
                    retained.earliest.number,
                    B256::repeat_byte(0xff),
                ))
                .unwrap();
            if completed {
                snapshot.commit_snapshot().unwrap();
            }
            OpProofsSnapshotInitProvider::commit(snapshot).unwrap();
            assert!(!backfill_proof_history_chunk(&factory, storage.clone(), 0, 1).unwrap());
            assert_eq!(storage.provider_ro().unwrap().get_proof_window().unwrap(), retained);
            assert_eq!(
                storage.indexed_hash(retained.latest.number).unwrap(),
                Some(retained.latest.hash)
            );
            assert!(matches!(
                storage
                    .snapshot_initialization_provider()
                    .unwrap()
                    .snapshot_init_anchor()
                    .unwrap()
                    .status,
                SnapshotInitStatus::NotStarted
            ));
            assert!(backfill_proof_history_chunk(&factory, storage.clone(), 0, 1).unwrap());
            assert_eq!(
                storage.provider_ro().unwrap().get_earliest_block().unwrap(),
                hashes[hashes.len() - 2]
            );
        }
    }
}
