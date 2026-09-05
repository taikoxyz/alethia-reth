//! Canonical-chain adapter for the upstream proof-history engine.

use super::{
    ProofHistoryStorage,
    config::ProofHistoryConfig,
    opt_block,
    storage_init::{
        backfill_proof_history_storage, finish_backfill, initialize_proof_history_storage,
        pending_backfill_target,
    },
    store::ProofHistoryDatabase,
};
use alethia_reth_rpc::proof_state::ProofHistoryReadiness;
use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use alloy_primitives::B256;
use derive_more::Constructor;
use eyre::eyre;
use reth::tasks::shutdown::GracefulShutdown;
use reth_db::Database;
use reth_ethereum_primitives::{Block, EthPrimitives};
use reth_evm::ConfigureEvm;
use reth_optimism_trie::{
    EngineHandle, OpProofStoragePruner, OpProofsProviderRO, OpProofsProviderRw, OpProofsStore,
    proof::DatabaseStateRoot,
};
use reth_primitives_traits::AlloyBlockHeader;
use reth_provider::{
    BlockHashReader, BlockNumReader, BlockReader, CanonStateNotification, CanonStateSubscriptions,
    ChainStateBlockReader, ChangeSetReader, DBProvider, DatabaseProviderFactory, HeaderProvider,
    StageCheckpointReader, StateProviderFactory, StateReader, StorageChangeSetReader,
    StorageSettingsCache,
};
use reth_trie::StateRoot;
use reth_trie_common::SortedTrieData;
use std::{sync::Arc, time::Duration};
use tokio::{sync::broadcast, task, time};
use tracing::{debug, warn};

/// Maximum backward history extension per startup step, in blocks.
const BACKFILL_BATCH_SIZE: u64 = 50;
/// Delay between attempts when canonical state has not reached the retained window.
const STARTUP_RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// Persist even a single idle block; frequent head polls must not postpone idle flushing.
const PERSISTENCE_THRESHOLD: u64 = 1;
/// Bound accepted in-memory work while a persistence transaction is running, in blocks.
const BACKPRESSURE_THRESHOLD: u64 = 10;

/// Awaits blocking work while preserving worker panics as critical-task failures.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> eyre::Result<T> + Send + 'static,
) -> eyre::Result<T> {
    match task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(eyre!("proof-history worker failed to join: {error}")),
    }
}

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

/// Startup either waits for canonical state, makes bounded progress, or starts indexing.
enum StartupStep {
    /// Canonical state is still unavailable; retry after a delay.
    Wait,
    /// Snapshot/backfill/reconciliation progressed; retry without an idle delay.
    Progress,
    /// Initialization and canonical reconciliation completed.
    Ready(EngineHandle<Block>),
}

/// Owns Taiko lifecycle policy while upstream owns indexing, persistence and pruning.
#[derive(Debug, Constructor)]
pub(super) struct ProofHistorySidecar<Evm, Provider> {
    /// Canonical provider supplying notifications and persisted catch-up targets.
    provider: Provider,
    /// Taiko EVM configuration used by upstream replay.
    evm_config: Evm,
    /// Metrics-wrapped storage used by live indexing and RPC.
    storage: ProofHistoryStorage,
    /// Unwrapped storage used for bulk initialization and backward backfill.
    init_storage: Arc<ProofHistoryDatabase>,
    /// Retention, verification and maintenance settings.
    config: ProofHistoryConfig,
    /// Read permission published only after canonical reconciliation.
    readiness: ProofHistoryReadiness,
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

impl<Evm, Provider> ProofHistorySidecar<Evm, Provider>
where
    Evm: ConfigureEvm<Primitives = EthPrimitives> + 'static,
    Provider: BlockHashReader
        + BlockNumReader
        + BlockReader<Block = Block>
        + DatabaseProviderFactory
        + StateReader
        + StateProviderFactory
        + Clone
        + Send
        + Sync
        + 'static,
    Provider::Provider: BlockNumReader
        + HeaderProvider
        + DBProvider
        + ChainStateBlockReader
        + ChangeSetReader
        + StorageChangeSetReader
        + StorageSettingsCache
        + StageCheckpointReader,
    <Provider::DB as Database>::TX: Sync,
{
    /// Runs until shutdown, keeping all engine operations and final thread joins off Tokio.
    pub(super) async fn run(self, shutdown: GracefulShutdown) -> eyre::Result<()>
    where
        Provider: CanonStateSubscriptions<Primitives = EthPrimitives>,
    {
        let this = Arc::new(self);
        // Keep shutdown pending until provider runtimes and engine threads have been joined.
        let cleanup_guard = shutdown.clone();
        let result = Self::run_loop(&this, shutdown).await;
        this.readiness.set_not_ready();
        blocking(move || {
            drop(this);
            Ok(())
        })
        .await?;
        drop(cleanup_guard);
        result
    }

    /// Drives initialization and canonical updates while the caller owns final resource cleanup.
    async fn run_loop(this: &Arc<Self>, mut shutdown: GracefulShutdown) -> eyre::Result<()>
    where
        Provider: CanonStateSubscriptions<Primitives = EthPrimitives>,
    {
        let mut notifications = this.provider.subscribe_to_canonical_state();
        let mut engine = None;
        let mut retry = Duration::ZERO;
        let mut interval = time::interval(this.config.prune_interval.max(Duration::from_millis(1)));
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

        loop {
            let notification = tokio::select! {
                biased;
                guard = &mut shutdown => {
                    this.readiness.set_not_ready();
                    blocking(move || { drop(engine); Ok(()) }).await?;
                    drop(guard);
                    return Ok(());
                }
                _ = time::sleep(retry), if engine.is_none() => {
                    let worker = Arc::clone(this);
                    match blocking(move || worker.prepare()).await? {
                        StartupStep::Wait => retry = STARTUP_RETRY_INTERVAL,
                        StartupStep::Progress => retry = Duration::ZERO,
                        StartupStep::Ready(handle) => {
                            engine = Some(handle);
                            this.readiness.set_ready();
                        }
                    }
                    continue;
                }
                notification = notifications.recv(), if engine.is_some() => {
                    match notification {
                        Ok(notification) => Some(notification),
                        Err(error) => {
                            this.readiness.set_not_ready();
                            let stopped = engine.take();
                            blocking(move || { drop(stopped); Ok(()) }).await?;
                            if matches!(error, broadcast::error::RecvError::Closed) {
                                return Ok(());
                            }
                            warn!(target: "reth::taiko::proof_history", ?error,
                                "canonical notifications lagged; reconciling proof history");
                            notifications = this.provider.subscribe_to_canonical_state();
                            retry = Duration::ZERO;
                            continue;
                        }
                    }
                }
                _ = interval.tick(), if engine.is_some() => None,
            };

            if matches!(notification, Some(CanonStateNotification::Reorg { .. })) {
                this.readiness.set_not_ready();
            }
            let handle = engine.take().expect("running engine selected");
            let worker = Arc::clone(this);
            engine = blocking(move || {
                let result = (|| -> eyre::Result<bool> {
                    let valid = match notification {
                        Some(notification) => worker.handle_notification(&handle, &notification)?,
                        None => {
                            matches!(worker.startup_action()?, ProofHistoryStartupAction::Ready)
                        }
                    };
                    if valid {
                        handle.sync_to(
                            worker.provider.database_provider_ro()?.best_block_number()?,
                        )?;
                    }
                    Ok(valid)
                })();
                if !matches!(result, Ok(true)) {
                    worker.readiness.set_not_ready();
                    // Drop the only handle here: no engine writer may survive reconciliation.
                    drop(handle);
                    return result.map(|_| None);
                }
                Ok(Some(handle))
            })
            .await?;
            if engine.is_some() {
                this.readiness.set_ready();
            } else {
                notifications = this.provider.subscribe_to_canonical_state();
                retry = Duration::ZERO;
            }
        }
    }

    /// Reconciles storage and performs at most one snapshot or bounded backward batch.
    fn prepare(&self) -> eyre::Result<StartupStep> {
        let target_path = self.config.required_storage_path()?.join("backfill-target");
        let pending_target = pending_backfill_target(&target_path)?;
        if pending_target.is_some() &&
            let Some((number, hash)) =
                opt_block(self.storage.provider_ro()?.get_earliest_block())? &&
            self.provider.block_hash(number)?.is_some_and(|canonical| canonical != hash)
        {
            // A current-state snapshot can be reorged before backfill reaches a stable anchor.
            // The pending marker proves this bootstrap has never been served or indexed live.
            self.init_storage.reset_bootstrap()?;
            return Ok(StartupStep::Progress);
        }
        match self.startup_action()? {
            ProofHistoryStartupAction::Uninitialized => {
                if self.config.backfill_window_only {
                    let db = self.provider.database_provider_ro()?;
                    let Some(finalized) = db.last_finalized_block_number()? else {
                        return Ok(StartupStep::Wait);
                    };
                    if db.best_block_number()? < finalized.saturating_sub(self.config.window) {
                        return Ok(StartupStep::Wait);
                    }
                }
                // An unfinished copy can restart at a newer source head; replace its target too.
                finish_backfill(&target_path)?;
                initialize_proof_history_storage(
                    &self.provider,
                    self.init_storage.clone(),
                    self.config
                        .backfill_window_only
                        .then_some((target_path.as_path(), self.config.window)),
                )?;
                return Ok(StartupStep::Progress);
            }
            ProofHistoryStartupAction::WaitForCanonicalEarliest { .. } |
            ProofHistoryStartupAction::WaitForCanonicalLatest { .. } => {
                debug!(target: "reth::taiko::proof_history", "waiting for canonical proof-history state");
                return Ok(StartupStep::Wait);
            }
            ProofHistoryStartupAction::UnwindToEarliest { earliest } => {
                let first = earliest
                    .number
                    .checked_add(1)
                    .ok_or_else(|| eyre!("cannot unwind beyond u64::MAX"))?;
                let hash = self
                    .provider
                    .block_hash(first)?
                    .ok_or_else(|| eyre!("missing proof-history unwind block {first}"))?;
                let rw = self.storage.provider_rw()?;
                rw.unwind_history(BlockWithParent::new(
                    earliest.hash,
                    BlockNumHash::new(first, hash),
                ))?;
                rw.commit()?;
                return Ok(StartupStep::Progress);
            }
            ProofHistoryStartupAction::Ready => {}
        }
        let window = self.storage.provider_ro()?.get_proof_window()?;
        let header = self.provider.sealed_header(window.latest.number)?.ok_or_else(|| {
            eyre!("missing proof-history snapshot header {}", window.latest.number)
        })?;
        let root = StateRoot::overlay_root(
            self.storage.provider_ro()?,
            window.latest.number,
            Default::default(),
        )?;
        if root != header.state_root() {
            return Err(eyre!(
                "proof-history state root mismatch at block {}",
                window.latest.number
            ));
        }
        if let Some(target) = pending_target {
            if window.earliest.number > target {
                let next = window.earliest.number.saturating_sub(BACKFILL_BATCH_SIZE).max(target);
                backfill_proof_history_storage(&self.provider, self.init_storage.clone(), next)?;
                return Ok(StartupStep::Progress);
            }
            finish_backfill(&target_path)?;
        }
        let to_prune = window
            .latest
            .number
            .saturating_sub(self.config.window)
            .saturating_sub(window.earliest.number);
        if to_prune > self.config.max_startup_prune_blocks {
            return Err(eyre!(
                "configuration requires pruning {to_prune} proof-history blocks, exceeding {}; \
                 raise --proofs-history.max-startup-prune-blocks or restore --proofs-history.window",
                self.config.max_startup_prune_blocks
            ));
        }
        let pruner = OpProofStoragePruner::new(
            self.storage.clone(),
            self.provider.clone(),
            self.config.window,
        );
        // Prune startup excess before the engine exists; all subsequent pruning is in its save txn.
        let rw = self.storage.provider_rw()?;
        pruner.prune_with_provider(&rw)?;
        rw.commit()?;
        Ok(StartupStep::Ready(EngineHandle::spawn_with_thresholds(
            self.evm_config.clone(),
            self.provider.clone(),
            self.storage.clone(),
            pruner,
            PERSISTENCE_THRESHOLD,
            BACKPRESSURE_THRESHOLD,
        )))
    }

    /// Checks both persisted window anchors against the currently observed canonical chain.
    fn startup_action(&self) -> eyre::Result<ProofHistoryStartupAction> {
        let ro = self.storage.provider_ro()?;
        let earliest = opt_block(ro.get_earliest_block())?;
        let latest = opt_block(ro.get_latest_block())?;
        let best = self.provider.best_block_number()?;
        proof_history_startup_action(
            earliest,
            latest,
            best,
            earliest.map(|(n, _)| self.provider.block_hash(n)).transpose()?.flatten(),
            latest
                .filter(|(n, _)| *n <= best)
                .map(|(n, _)| self.provider.block_hash(n))
                .transpose()?
                .flatten(),
        )
    }

    /// Applies a current notification, returning false when its branch needs reconciliation.
    fn handle_notification(
        &self,
        engine: &EngineHandle<Block>,
        notification: &CanonStateNotification<EthPrimitives>,
    ) -> eyre::Result<bool> {
        let new = match notification {
            CanonStateNotification::Commit { new } => new,
            CanonStateNotification::Reorg { old, new } => {
                let target = if new.is_empty() {
                    old.fork_block()
                } else {
                    BlockNumHash::new(new.tip().number, new.tip().hash())
                };
                if self.provider.block_hash(target.number)? != Some(target.hash) ||
                    (new.is_empty() && self.provider.best_block_number()? != target.number)
                {
                    return Ok(false);
                }
                let earliest = self.storage.provider_ro()?.get_earliest_block()?;
                if old.first().number() <= earliest.number &&
                    self.provider.block_hash(earliest.number)? == Some(earliest.hash)
                {
                    return Ok(false); // queued reorg already covered by a newer canonical anchor
                }
                ensure_canonical_update_above_earliest(
                    "reorg",
                    earliest,
                    BlockNumHash::new(old.first().number(), old.first().hash()),
                )?;
                if !new.is_empty() && old.fork_block() != new.fork_block() {
                    return Err(eyre!("proof-history reorg fork blocks do not match"));
                }
                // The engine's buffered tip may exceed the persisted tip: always forward unwind.
                engine.unwind(old.first().block_with_parent())?;
                if new.is_empty() {
                    return Ok(true);
                }
                new
            }
        };
        if self.provider.block_hash(new.tip().number)? != Some(new.tip().hash()) {
            return Ok(false);
        }
        let latest = self.storage.provider_ro()?.get_latest_block()?.number;
        if !committed_chain_is_contiguous(new.first().number(), latest) {
            // Submitting a gap would make upstream replay up to an unpersisted notification tip.
            // The caller instead sets sync_to to the on-disk executed head.
            return Ok(true);
        }
        for (number, block) in new.blocks() {
            let verify = self.config.verification_interval > 0 &&
                number.is_multiple_of(self.config.verification_interval);
            if !verify && let Some(data) = new.trie_data_at(*number) {
                let SortedTrieData { hashed_state, trie_updates } = &data.get().sorted;
                engine.index_block(
                    block.block_with_parent(),
                    (**trie_updates).clone(),
                    (**hashed_state).clone(),
                )?;
            } else {
                // Replay from the on-disk head through sync_to. execute_block can silently skip
                // unavailable parent state; submitting the next live block would then turn it
                // into a gap and raise upstream's replay target beyond persisted execution.
                break;
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProofHistoryDatabase, ProofHistorySidecar, ProofHistoryStartupAction, StartupStep,
        committed_chain_is_contiguous, ensure_canonical_update_above_earliest,
        proof_history_startup_action,
    };
    use crate::proof_history::{
        ProofHistoryConfig, storage_init::initialize_proof_history_storage,
    };
    use alethia_reth_rpc::proof_state::ProofHistoryReadiness;
    use alloy_eips::BlockNumHash;
    use alloy_primitives::B256;
    use reth_chain_state::{ExecutedBlock, NewCanonicalChain};
    use reth_chainspec::{ChainSpec, ChainSpecBuilder};
    use reth_db_common::init::init_genesis;
    use reth_ethereum_primitives::{Block, BlockBody};
    use reth_evm_ethereum::EthEvmConfig;
    use reth_optimism_trie::{
        EngineHandle, OpProofStoragePruner, OpProofsInitProvider, OpProofsProviderRO, OpProofsStore,
    };
    use reth_primitives_traits::Block as _;
    use reth_provider::{
        providers::BlockchainProvider,
        test_utils::{MockNodeTypesWithDB, create_test_provider_factory_with_chain_spec},
    };
    use std::sync::Arc;

    fn sidecar_fixture() -> (
        ProofHistorySidecar<EthEvmConfig, BlockchainProvider<MockNodeTypesWithDB>>,
        Arc<ChainSpec>,
        tempfile::TempDir,
    ) {
        let spec = Arc::new(ChainSpecBuilder::mainnet().paris_activated().build());
        let factory = create_test_provider_factory_with_chain_spec(spec.clone());
        init_genesis(&factory).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(ProofHistoryDatabase::open(dir.path()).unwrap());
        initialize_proof_history_storage(&factory, storage.clone(), None).unwrap();
        let sidecar = ProofHistorySidecar::new(
            BlockchainProvider::new(factory).unwrap(),
            EthEvmConfig::ethereum(spec.clone()),
            storage.clone().into(),
            storage,
            ProofHistoryConfig {
                storage_path: Some(dir.path().to_path_buf()),
                ..ProofHistoryConfig::disabled()
            },
            ProofHistoryReadiness::new(),
        );
        (sidecar, spec, dir)
    }

    #[test]
    fn prepare_rejects_a_completed_snapshot_with_an_invalid_root() {
        let (mut sidecar, spec, _dir) = sidecar_fixture();
        let invalid_dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(ProofHistoryDatabase::open(invalid_dir.path()).unwrap());
        let init = storage.initialization_provider().unwrap();
        init.set_initial_state_anchor(BlockNumHash::new(0, spec.genesis_hash())).unwrap();
        init.commit_initial_state().unwrap();
        init.commit().unwrap();
        sidecar.storage = storage.clone().into();
        sidecar.init_storage = storage;
        assert!(sidecar.prepare().is_err(), "a completed marker cannot bypass root verification");
    }

    #[test]
    fn prepare_restarts_a_pending_bootstrap_after_its_anchor_reorgs() {
        let (mut sidecar, _spec, _dir) = sidecar_fixture();
        let pending = tempfile::tempdir().unwrap();
        let storage = Arc::new(ProofHistoryDatabase::open(pending.path()).unwrap());
        let init = storage.initialization_provider().unwrap();
        init.set_initial_state_anchor(BlockNumHash::new(0, B256::repeat_byte(7))).unwrap();
        init.commit_initial_state().unwrap();
        init.commit().unwrap();
        std::fs::write(pending.path().join("backfill-target"), "0").unwrap();
        sidecar.storage = storage.clone().into();
        sidecar.init_storage = storage;
        sidecar.config.storage_path = Some(pending.path().to_path_buf());

        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Progress));
        assert!(sidecar.storage.provider_ro().unwrap().get_earliest_block().is_err());
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Progress));
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Ready(_)));
        assert!(!pending.path().join("backfill-target").exists());
    }

    #[test]
    fn prepare_waits_for_finalized_window_before_snapshot() {
        let (mut sidecar, _spec, _dir) = sidecar_fixture();
        let empty = tempfile::tempdir().unwrap();
        let storage = Arc::new(ProofHistoryDatabase::open(empty.path()).unwrap());
        sidecar.storage = storage.clone().into();
        sidecar.init_storage = storage;
        sidecar.config.storage_path = Some(empty.path().to_path_buf());
        sidecar.config.backfill_window_only = true;
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Wait));
    }

    fn executed(number: u64, parent_hash: B256, state_root: B256, fork: u8) -> ExecutedBlock {
        let block = Block {
            header: alloy_consensus::Header {
                number,
                parent_hash,
                state_root,
                extra_data: vec![fork].into(),
                ..Default::default()
            },
            body: BlockBody::default(),
        }
        .try_into_recovered()
        .unwrap();
        ExecutedBlock { recovered_block: Arc::new(block), ..Default::default() }
    }

    #[test]
    fn reorg_replaces_buffered_blocks_above_the_persisted_tip() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        // Hold updates in memory to exercise the real buffered-tip/persisted-tip distinction.
        let engine = EngineHandle::spawn_with_thresholds(
            sidecar.evm_config.clone(),
            sidecar.provider.clone(),
            sidecar.storage.clone(),
            OpProofStoragePruner::new(sidecar.storage.clone(), sidecar.provider.clone(), 100),
            100,
            101,
        );
        let old = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
        let state = sidecar.provider.canonical_in_memory_state();
        let commit = NewCanonicalChain::Commit { new: vec![old.clone()] };
        let notification = commit.to_chain_notification();
        state.update_chain(commit);
        state.set_canonical_head(old.recovered_block.clone_sealed_header());
        assert!(sidecar.handle_notification(&engine, &notification).unwrap());
        assert_eq!(sidecar.storage.provider_ro().unwrap().get_latest_block().unwrap().number, 0);

        let new = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 2);
        let update = NewCanonicalChain::Reorg { old: vec![old], new: vec![new.clone()] };
        let notification = update.to_chain_notification();
        state.update_chain(update);
        state.set_canonical_head(new.recovered_block.clone_sealed_header());
        assert!(sidecar.handle_notification(&engine, &notification).unwrap());
        // A following block must accept the replacement's hash as parent, proving that the
        // buffered old branch was actually removed instead of skipped using the persisted tip.
        engine
            .index_block(
                alloy_eips::eip1898::BlockWithParent::new(
                    new.recovered_block.hash(),
                    BlockNumHash::new(2, hash(9)),
                ),
                Default::default(),
                Default::default(),
            )
            .unwrap();
        drop(engine);
    }

    #[test]
    fn stale_reorg_is_reconciled_before_touching_the_engine() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let StartupStep::Ready(engine) = sidecar.prepare().unwrap() else { panic!("ready") };
        let old = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
        let stale = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 2);
        let current = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 3);
        let state = sidecar.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Commit { new: vec![current.clone()] });
        state.set_canonical_head(current.recovered_block.clone_sealed_header());
        let stale_update = NewCanonicalChain::Reorg { old: vec![old], new: vec![stale] };
        assert!(
            !sidecar.handle_notification(&engine, &stale_update.to_chain_notification()).unwrap()
        );
        assert_eq!(sidecar.storage.provider_ro().unwrap().get_latest_block().unwrap().number, 0);
        drop(engine);
    }

    #[test]
    fn sidecar_persists_an_idle_commit_and_joins_on_shutdown() {
        use reth::tasks::Runtime;
        use std::time::Duration;
        let (mut sidecar, spec, _dir) = sidecar_fixture();
        sidecar.config.prune_interval = Duration::from_millis(20);
        let state = sidecar.provider.canonical_in_memory_state();
        let storage = sidecar.storage.clone();
        let readiness = sidecar.readiness.clone();
        let runtime = Runtime::test();
        let task = runtime.spawn_with_graceful_shutdown_signal(move |shutdown| async move {
            sidecar.run(shutdown).await.unwrap();
        });
        runtime.handle().block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !readiness.is_ready() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let block = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
            let expected = block.recovered_block.hash();
            let update = NewCanonicalChain::Commit { new: vec![block.clone()] };
            let notification = update.to_chain_notification();
            state.update_chain(update);
            state.set_canonical_head(block.recovered_block.clone_sealed_header());
            state.notify_canon_state(notification);
            // Repeated head polls must not postpone persistence of a single paused tail block.
            tokio::time::timeout(Duration::from_secs(5), async {
                while storage.provider_ro().unwrap().get_latest_block().unwrap().hash != expected {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        });
        assert!(runtime.graceful_shutdown_with_timeout(Duration::from_secs(5)));
        runtime.handle().block_on(task).unwrap();
        assert!(!readiness.is_ready());
    }

    #[test]
    fn sidecar_replays_verified_blocks_only_after_disk_execution_advances() {
        use reth::tasks::Runtime;
        use reth_provider::{BlockWriter, DBProvider, DatabaseProviderFactory, ExecutionOutcome};
        use std::time::Duration;
        let (mut sidecar, spec, _dir) = sidecar_fixture();
        sidecar.config.prune_interval = Duration::from_millis(20);
        sidecar.config.verification_interval = 1;
        let provider = sidecar.provider.clone();
        let state = provider.canonical_in_memory_state();
        let storage = sidecar.storage.clone();
        let readiness = sidecar.readiness.clone();
        let runtime = Runtime::test();
        let task = runtime.spawn_with_graceful_shutdown_signal(move |shutdown| async move {
            sidecar.run(shutdown).await.unwrap();
        });
        runtime.handle().block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !readiness.is_ready() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let block = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
            let update = NewCanonicalChain::Commit { new: vec![block.clone()] };
            let notification = update.to_chain_notification();
            state.update_chain(update);
            state.set_canonical_head(block.recovered_block.clone_sealed_header());
            state.notify_canon_state(notification);
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(storage.provider_ro().unwrap().get_latest_block().unwrap().number, 0);

            let rw = provider.database_provider_rw().unwrap();
            rw.append_blocks_with_state(
                vec![(*block.recovered_block).clone()],
                &ExecutionOutcome {
                    first_block: 1,
                    receipts: vec![vec![]],
                    requests: vec![Default::default()],
                    ..Default::default()
                },
                Default::default(),
            )
            .unwrap();
            rw.commit().unwrap();
            // No second canonical notification: the disk-head poll must discover this gap.
            tokio::time::timeout(Duration::from_secs(5), async {
                while storage.provider_ro().unwrap().get_latest_block().unwrap().number != 1 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        });
        assert!(runtime.graceful_shutdown_with_timeout(Duration::from_secs(5)));
        runtime.handle().block_on(task).unwrap();
    }

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
