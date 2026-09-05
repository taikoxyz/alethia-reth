//! V2 proof-history snapshot initialization and cancellable backward backfill.

use super::store::ProofHistoryDatabase;
use alloy_consensus::BlockHeader;
use eyre::{WrapErr, eyre};
use reth_db::Database;
use reth_optimism_trie::{
    BackfillJob, InitializationJob, OpProofsProviderRO, OpProofsStore, RethTrieStorageLayout,
    proof::DatabaseStateRoot,
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
/// The header and trie tables come from the same read transaction; the resulting root is
/// verified before the sidecar publishes readiness. Use an unwrapped store to avoid collecting
/// a metrics observation for every copied row.
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
    let header = db
        .sealed_header(number)?
        .ok_or_else(|| eyre!("missing proof-history snapshot header {number}"))?;
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
        warn!(target: "reth::taiko::proof_history", number, actual = ?root, expected = ?header.state_root(),
            "discarding invalid proof-history snapshot; initialization will retry");
        storage.reset_bootstrap()?;
        return Ok(false);
    }
    info!(target: "reth::taiko::proof_history", number, "initialized proof-history snapshot");
    Ok(true)
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

/// Extends a completed snapshot backward from its committed earliest block to `target`.
/// The upstream snapshot-assisted job validates historical roots and commits atomic batches.
/// Cancellation is observed when opening the next batch transaction.
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
    let db = provider.database_provider_ro()?.disable_long_read_transaction_safety();
    let earliest = storage.provider_ro()?.get_earliest_block()?.number;
    // Write ahead only below the committed earliest block; a crash leaves harmless extra rows.
    for start in (target..earliest).step_by(1000) {
        storage.check_bootstrap_cancelled()?;
        let end = start.saturating_add(1000).min(earliest);
        let hashes = db.canonical_hashes_range(start, end)?;
        if hashes.len() as u64 != end - start {
            return Err(eyre!("missing canonical hashes for proof-history backfill {start}..{end}"));
        }
        storage.record_hashes((start..end).zip(hashes))?;
    }
    BackfillJob::new(db, storage).run_with_snapshot(target).wrap_err(
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
        init.commit().unwrap();
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
        init.commit().unwrap();
        drop(storage);

        let storage = ProofHistoryDatabase::open(dir.path()).unwrap();
        let init = storage.initialization_provider().unwrap();
        let anchor = init.initial_state_anchor().unwrap();
        assert!(matches!(anchor.status, InitialStateStatus::NotStarted));
        assert!(anchor.latest_hashed_account_key.is_none());
        init.set_initial_state_anchor(BlockNumHash::new(9, B256::repeat_byte(9))).unwrap();
        init.commit_initial_state().unwrap();
        init.commit().unwrap();
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

        storage.cancel_bootstrap();
        assert!(backfill_proof_history_storage(&factory, storage.clone(), 2).is_err());
        assert_eq!(storage.provider_ro().unwrap().get_earliest_block().unwrap().number, 3);
        storage.resume_bootstrap();
        backfill_proof_history_storage(&factory, storage.clone(), 2).unwrap();
        assert_eq!(storage.provider_ro().unwrap().get_earliest_block().unwrap().number, 2);
        drop(storage);
        let storage = Arc::new(ProofHistoryDatabase::open(dir.path()).unwrap());
        assert_eq!(pending_backfill_target(&target_path).unwrap(), Some(2));
        finish_backfill(&target_path).unwrap();
        assert_eq!(pending_backfill_target(&target_path).unwrap(), None);
        backfill_proof_history_storage(&factory, storage.clone(), 0).unwrap();
        let window = storage.provider_ro().unwrap().get_proof_window().unwrap();
        assert_eq!(window.earliest, BlockNumHash::new(0, spec.genesis_hash()));
        assert_eq!(window.latest, BlockNumHash::new(3, parent));

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
