//! V2 proof-history snapshot initialization and cancellable backward backfill.

use super::store::ProofHistoryDatabase;
use alloy_consensus::BlockHeader;
use eyre::{WrapErr, eyre};
use reth_db::Database;
use reth_optimism_trie::{
    BackfillJob, InitializationJob, OpProofsBackfillStore, OpProofsProviderRO,
    OpProofsSnapshotInitProvider, OpProofsStore, RethTrieStorageLayout, SnapshotInitStatus,
    backfill::DEFAULT_BACKFILL_BATCH_SIZE, proof::DatabaseStateRoot,
};
use reth_provider::{
    BlockHashReader, BlockNumReader, ChainStateBlockReader, ChangeSetReader, DBProvider,
    DatabaseProviderFactory, HeaderProvider, StageCheckpointReader, StorageChangeSetReader,
    StorageSettingsCache,
};
use reth_stages_types::StageId;
use reth_trie::StateRoot;
use std::{fs, io, path::Path, sync::Arc};
use tracing::{info, warn};

/// Copies a single persisted current-state snapshot using the upstream initialization job.
/// Trie tables share one MDBX snapshot, while headers are backed by reth's shared static files.
/// A root mismatch retries once with a fresh source, then fails with repair guidance. Canonical
/// reconciliation checks the copied anchor before reads begin. Bulk rows bypass metrics wrappers.
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
    for attempt in 0..2 {
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
        if db.get_stage_checkpoint_progress(StageId::MerkleExecute)?.is_some_and(|p| !p.is_empty())
        {
            info!(target: "reth::taiko::proof_history", number, "waiting for partial Merkle execution to finish");
            return Ok(false);
        }
        let Some(header) = db.sealed_header(number)? else {
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
            storage.reset_bootstrap()?;
            if attempt == 0 {
                warn!(target: "reth::taiko::proof_history", number, actual = ?root,
                expected = ?header.state_root(), "snapshot root mismatch; retrying once with a fresh source");
                continue;
            }
            return Err(eyre!(
                "proof-history snapshot state root mismatch at block {number} ({:?}): \
                 computed {root:?}, expected {:?}; the invalid copy was discarded. \
                 Verify or repair the node's source trie/hashed state and storage layout before \
                 restarting; copying the same source again cannot repair it",
                header.hash(),
                header.state_root()
            ));
        }
        info!(target: "reth::taiko::proof_history", number, "initialized proof-history snapshot");
        return Ok(true);
    }
    unreachable!("each final snapshot attempt returns")
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
pub(super) fn backfill_proof_history_storage<Provider>(
    provider: &Provider,
    storage: Arc<ProofHistoryDatabase>,
    target: u64,
) -> eyre::Result<()>
where
    Provider: DatabaseProviderFactory,
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
) -> eyre::Result<()>
where
    Provider: DatabaseProviderFactory,
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
    let db = provider.database_provider_ro()?.disable_long_read_transaction_safety();
    let window = storage.provider_ro()?.get_proof_window()?;
    if db.block_hash(window.earliest.number)? != Some(window.earliest.hash) ||
        db.block_hash(window.latest.number)? != Some(window.latest.hash)
    {
        info!(target: "reth::taiko::proof_history", earliest = window.earliest.number,
            latest = window.latest.number, "backfill source changed; returning to reconciliation");
        return Ok(());
    }
    let earliest = window.earliest.number;
    if target >= earliest {
        return Ok(());
    }
    let next = earliest.saturating_sub(max_blocks.max(1)).max(target);
    let result = (|| -> eyre::Result<()> {
        // Journal only this chunk, below committed earliest. A crash leaves harmless extra rows.
        for start in (next..earliest).step_by(1000) {
            storage.check_bootstrap_cancelled()?;
            let end = start.saturating_add(1000).min(earliest);
            let hashes = db.canonical_hashes_range(start, end)?;
            if hashes.len() as u64 != end - start {
                return Err(eyre!(
                    "missing canonical hashes for proof-history backfill {start}..{end}"
                ));
            }
            storage.record_hashes((start..end).zip(hashes))?;
        }
        let snapshot = storage.snapshot_initialization_provider()?.snapshot_init_anchor()?;
        // Keep an existing snapshot in sync even when only its final small chunk remains. Use
        // total remaining distance, not the chunk size, to choose the initial strategy.
        let use_snapshot = !matches!(snapshot.status, SnapshotInitStatus::NotStarted) ||
            earliest - target > DEFAULT_BACKFILL_BATCH_SIZE as u64;
        let job = BackfillJob::new(db, Arc::clone(&storage));
        if use_snapshot { job.run_with_snapshot(next) } else { job.run(next) }.map_err(Into::into)
    })();
    if result.is_err() {
        // Static-file headers can change even while the job's MDBX snapshot stays pinned.
        // An ordinary reorg must return to reconciliation instead of becoming a fatal root error.
        let current = provider.database_provider_ro()?;
        let retained = storage.provider_ro()?.get_proof_window()?;
        if current.block_hash(retained.earliest.number)? != Some(retained.earliest.hash) ||
            current.block_hash(retained.latest.number)? != Some(retained.latest.hash)
        {
            return Ok(());
        }
    }
    result.wrap_err(
        "failed to backfill proof history; required historical changesets must remain unpruned",
    )
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
        assert!(backfill_proof_history_storage(&factory, stale.clone(), 2).is_ok());
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
}
