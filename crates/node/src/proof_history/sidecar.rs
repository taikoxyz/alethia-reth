//! Proof-history sidecar: notification handling, engine lifecycle, and finality pruning.

use super::{
    config::ProofHistoryConfig,
    engine::{ProofHistoryEngine, ReorgBlockUpdates},
    prune::{FinalityProofHistoryPruner, FinalityPruneOutcome, startup_prune_exposure},
    storage_init::{
        initialize_finalized_window_proof_history_storage, initialize_proof_history_storage,
    },
};
use alethia_reth_rpc::proof_state::ProofHistoryReadiness;
use alloy_consensus::BlockHeader;
use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use alloy_primitives::B256;
use eyre::eyre;
use reth::{
    providers::{
        BlockHashReader, BlockNumReader, BlockReader, CanonStateNotification,
        CanonStateSubscriptions, DBProvider, DatabaseProviderFactory, HeaderProvider,
        StageCheckpointReader,
    },
    tasks::{TaskExecutor, shutdown::GracefulShutdown},
};
use reth_db::Database;
use reth_execution_types::Chain;
use reth_node_api::{FullNodeComponents, NodePrimitives, NodeTypes};
use reth_optimism_trie::{
    OpProofsBackfillStore, OpProofsStorage, OpProofsStorageError, OpProofsStore,
    api::{OpProofsProviderRO, OpProofsProviderRw, ProofWindowRange},
};
use reth_storage_api::{
    ChainStateBlockReader, ChangeSetReader, StorageChangeSetReader, StorageSettingsCache,
};
use reth_trie_common::SortedTrieData;
use std::{future::Future, panic, path::Path, sync::Arc, time::Duration};
use tokio::{
    sync::broadcast,
    task,
    time::{self, Instant, MissedTickBehavior},
};
use tracing::{debug, error, info, warn};

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

/// Logs one finality-aware pruner result while preserving blocking-worker panics.
fn log_prune_join_result(result: Result<eyre::Result<FinalityPruneOutcome>, task::JoinError>) {
    match blocking_join_result(result, "proof-history pruner worker").and_then(|result| result) {
        Ok(FinalityPruneOutcome::MissingFinality) => {
            debug!(target: "reth::taiko::proof_history", "proof-history prune deferred until persisted finality is available");
        }
        Ok(FinalityPruneOutcome::UpToDate) => {
            debug!(target: "reth::taiko::proof_history", "proof-history finalized retention boundary is up to date");
        }
        Ok(FinalityPruneOutcome::CanonicalMismatch) => {
            warn!(target: "reth::taiko::proof_history", "proof-history prune canonical snapshot did not match stored bounds; retrying on a later tick");
        }
        Ok(FinalityPruneOutcome::Pruned { from, to }) => {
            info!(
                target: "reth::taiko::proof_history",
                from = from.number,
                to = to.number,
                "advanced proof-history finalized retention boundary"
            );
        }
        Err(error) => {
            error!(
                target: "reth::taiko::proof_history",
                ?error,
                "proof-history finality-aware prune pass failed; retrying on a later tick"
            );
        }
    }
}

/// Delay used while waiting for delayed proof-history initialization to become possible.
const PROOF_HISTORY_DELAYED_START_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Delay between polls of the node's executed head while proof-history is caught up, so a
/// staged-sync gap is backfilled even when no live canonical notification arrives.
const PROOF_HISTORY_HEAD_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Sole owned upstream proof-history engine generation.
///
/// The box is intentionally non-cloneable: taking it from the slot proves the sidecar no longer
/// has a sender capable of mutating the old engine generation before reconciliation begins.
type EngineSlot<Block> = Option<Box<dyn ProofHistoryEngine<Block>>>;

/// Live reconciliation result for committed proof-history storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProofHistoryLiveAction {
    /// The complete retained proof window matches the pinned persisted canonical snapshot.
    Ready,
    /// The suffix must be removed through the first block above the retained canonical anchor.
    Unwind {
        /// Inclusive V2 unwind marker; its parent becomes the new committed latest identity.
        first_removed: BlockWithParent,
    },
}

/// Chooses a fail-closed live reconciliation action from one proof and canonical snapshot.
///
/// Unlike startup reconciliation, this never waits for a stored head above persisted canonical
/// state. V2 stores endpoint identities only, so an interior height cannot be safely relabelled as
/// canonical. Once the retained earliest identity is validated, the entire untrusted suffix is
/// removed and a fresh engine generation syncs forward from that anchor.
fn proof_history_live_action(
    proof_window: ProofWindowRange,
    canonical_best: u64,
    canonical_earliest_hash: Option<B256>,
    canonical_latest_hash: Option<B256>,
    canonical_child: Option<BlockWithParent>,
    storage_path: &Path,
) -> eyre::Result<ProofHistoryLiveAction> {
    let ProofWindowRange { earliest, latest } = proof_window;
    if earliest.number > latest.number {
        return Err(eyre!(
            "proof-history storage at {} has earliest block {} after latest block {}; wipe that proof-history directory or use a fresh path and restart initialization",
            storage_path.display(),
            earliest.number,
            latest.number,
        ));
    }
    if earliest.number == latest.number && earliest.hash != latest.hash {
        return Err(eyre!(
            "proof-history storage at {} has conflicting hashes {:?} and {:?} for one retained height {}; wipe that proof-history directory or use a fresh path and restart initialization",
            storage_path.display(),
            earliest.hash,
            latest.hash,
            earliest.number,
        ));
    }
    if canonical_best < earliest.number {
        return Err(eyre!(
            "persisted canonical head {canonical_best} is below proof-history retained earliest block {} hash {:?}; this is a deep reorg that crosses the retained boundary, so wipe proof-history storage at {} or use a fresh path and restart initialization",
            earliest.number,
            earliest.hash,
            storage_path.display(),
        ));
    }
    if canonical_earliest_hash != Some(earliest.hash) {
        return Err(eyre!(
            "proof-history retained earliest block {} hash {:?} is not canonical during live recovery; this is a deep reorg that crosses the retained boundary, so wipe proof-history storage at {} or use a fresh path and restart initialization",
            earliest.number,
            earliest.hash,
            storage_path.display(),
        ));
    }
    if latest.number <= canonical_best && canonical_latest_hash == Some(latest.hash) {
        return Ok(ProofHistoryLiveAction::Ready)
    }

    let first_removed_number = earliest
        .number
        .checked_add(1)
        .ok_or_else(|| eyre!("cannot construct a proof-history unwind marker beyond u64::MAX"))?;
    let first_removed = match canonical_child {
        Some(child)
            if child.block.number == first_removed_number && child.parent == earliest.hash =>
        {
            child
        }
        Some(child) => {
            return Err(eyre!(
                "persisted canonical child {} has parent {:?}, expected proof-history retained earliest block {} hash {:?}; wipe proof-history storage at {} or use a fresh path and restart initialization",
                child.block.number,
                child.parent,
                earliest.number,
                earliest.hash,
                storage_path.display(),
            ));
        }
        None if canonical_best == earliest.number => {
            // V2 `unwind_history` uses the marker number as the inclusive first removal and its
            // parent as the resulting latest identity; the marker's own hash is not consulted.
            BlockWithParent::new(earliest.hash, BlockNumHash::new(first_removed_number, B256::ZERO))
        }
        None => {
            return Err(eyre!(
                "persisted canonical snapshot has no child above proof-history retained earliest block {} hash {:?} despite canonical best {}; wipe proof-history storage at {} or use a fresh path and restart initialization",
                earliest.number,
                earliest.hash,
                canonical_best,
                storage_path.display(),
            ));
        }
    };

    Ok(ProofHistoryLiveAction::Unwind { first_removed })
}

/// Atomically reconciles one committed V2 window against one persisted canonical snapshot.
///
/// The proof RW transaction opens before either endpoint is read and remains open through an
/// optional unwind commit, preventing the independent finality pruner from moving the retained
/// boundary between the decision and mutation.
fn reconcile_live_storage_atomic<Storage, Provider>(
    storage: &OpProofsStorage<Storage>,
    provider: &Provider,
    storage_path: &Path,
) -> eyre::Result<ProofHistoryLiveAction>
where
    Storage: OpProofsStore,
    Provider: DatabaseProviderFactory,
    Provider::Provider: BlockNumReader + HeaderProvider,
{
    let proof_rw = storage.provider_rw()?;
    let proof_window = proof_rw.get_proof_window().map_err(|error| {
        eyre!(
            "proof-history live recovery found no initialized V2 window at {}: {error}; wipe that proof-history directory or use a fresh path and restart initialization",
            storage_path.display()
        )
    })?;

    let canonical = provider.database_provider_ro()?;
    let canonical_best = canonical.best_block_number()?;
    let canonical_earliest_hash = if proof_window.earliest.number <= canonical_best {
        canonical.sealed_header(proof_window.earliest.number)?.map(|header| header.hash())
    } else {
        None
    };
    let canonical_latest_hash = if proof_window.latest.number <= canonical_best {
        canonical.sealed_header(proof_window.latest.number)?.map(|header| header.hash())
    } else {
        None
    };
    let needs_unwind = proof_window.latest.number > canonical_best ||
        canonical_latest_hash != Some(proof_window.latest.hash);
    let canonical_child = if needs_unwind && canonical_best > proof_window.earliest.number {
        let child_number = proof_window.earliest.number.checked_add(1).ok_or_else(|| {
            eyre!("cannot resolve a proof-history canonical child beyond u64::MAX")
        })?;
        canonical.sealed_header(child_number)?.map(|header| {
            BlockWithParent::new(
                header.parent_hash(),
                BlockNumHash::new(child_number, header.hash()),
            )
        })
    } else {
        None
    };

    let action = proof_history_live_action(
        proof_window,
        canonical_best,
        canonical_earliest_hash,
        canonical_latest_hash,
        canonical_child,
        storage_path,
    )?;
    drop(canonical);

    if let ProofHistoryLiveAction::Unwind { first_removed } = action {
        proof_rw.unwind_history(first_removed).map_err(|error| {
            eyre!(
                "atomic proof-history live unwind from block {} above retained anchor {:?} failed at {}: {error}; wipe that proof-history directory or use a fresh path and restart initialization",
                first_removed.block.number,
                first_removed.parent,
                storage_path.display(),
            )
        })?;
        proof_rw.commit()?;
    }
    Ok(action)
}

/// Drops and joins an owned engine generation outside the asynchronous executor worker.
async fn drop_engine<Block>(engine: Box<dyn ProofHistoryEngine<Block>>) -> eyre::Result<()>
where
    Block: reth_primitives_traits::Block + 'static,
{
    blocking_join_result(
        task::spawn_blocking(move || drop(engine)).await,
        "proof-history engine shutdown worker",
    )
}

/// Takes and drops the sole installed engine generation, if one exists.
async fn clear_engine_slot<Block>(slot: &mut EngineSlot<Block>) -> eyre::Result<()>
where
    Block: reth_primitives_traits::Block + 'static,
{
    if let Some(engine) = slot.take() {
        drop_engine(engine).await?;
    }
    Ok(())
}

/// Replaces a lagged engine generation only after committed storage is reconciled.
///
/// The new generation remains local until its initial persisted-head sync request succeeds. Any
/// error therefore leaves `slot` empty and readiness clear.
async fn recover_engine_after_lag<Block, Reconcile, ReconcileFuture, SpawnEngine, ReadHead>(
    slot: &mut EngineSlot<Block>,
    readiness: &ProofHistoryReadiness,
    reconcile: Reconcile,
    spawn_engine: SpawnEngine,
    read_persisted_head: ReadHead,
) -> eyre::Result<()>
where
    Block: reth_primitives_traits::Block + 'static,
    Reconcile: FnOnce() -> ReconcileFuture,
    ReconcileFuture: Future<Output = eyre::Result<()>>,
    SpawnEngine: FnOnce() -> eyre::Result<Box<dyn ProofHistoryEngine<Block>>>,
    ReadHead: FnOnce() -> eyre::Result<u64>,
{
    readiness.set_not_ready();
    clear_engine_slot(slot).await?;
    reconcile().await?;

    install_engine_generation(slot, readiness, spawn_engine, read_persisted_head).await
}

/// Replaces a lagged notification receiver before rebuilding its engine generation.
///
/// Keeping receiver replacement and recovery in one dispatch helper prevents an empty engine slot
/// from accidentally selecting startup reconciliation, which waits rather than rolling an
/// unauditable V2 suffix back to its retained anchor.
async fn recover_after_lagged_receiver<ReplaceReceiver, Recover, RecoverFuture>(
    replace_receiver: ReplaceReceiver,
    recover: Recover,
) -> eyre::Result<()>
where
    ReplaceReceiver: FnOnce(),
    Recover: FnOnce() -> RecoverFuture,
    RecoverFuture: Future<Output = eyre::Result<()>>,
{
    replace_receiver();
    recover().await
}

/// Spawns, initially syncs, and then installs one fresh engine generation.
///
/// The generation is kept local and joined on every post-spawn failure, so callers never expose a
/// partially initialized engine through the owned slot.
async fn install_engine_generation<Block, SpawnEngine, ReadHead>(
    slot: &mut EngineSlot<Block>,
    readiness: &ProofHistoryReadiness,
    spawn_engine: SpawnEngine,
    read_persisted_head: ReadHead,
) -> eyre::Result<()>
where
    Block: reth_primitives_traits::Block + 'static,
    SpawnEngine: FnOnce() -> eyre::Result<Box<dyn ProofHistoryEngine<Block>>>,
    ReadHead: FnOnce() -> eyre::Result<u64>,
{
    if slot.is_some() {
        return Err(eyre!("proof-history engine slot is already occupied"));
    }
    readiness.set_not_ready();

    let engine = spawn_engine()?;
    let persisted_head = match read_persisted_head() {
        Ok(head) => head,
        Err(error) => {
            drop_engine(engine).await?;
            return Err(error)
        }
    };
    if let Err(error) = engine.sync_to(persisted_head) {
        drop_engine(engine).await?;
        return Err(error)
    }

    *slot = Some(engine);
    readiness.set_ready();
    Ok(())
}

/// Polls the persisted executed head and forwards it to the sole installed engine.
///
/// A transient provider-read failure leaves the healthy generation installed for the next poll.
/// An engine-channel failure invalidates the generation, clears readiness, and joins it before
/// returning the error.
async fn sync_engine_to_persisted_head<Block, ReadHead>(
    slot: &mut EngineSlot<Block>,
    readiness: &ProofHistoryReadiness,
    read_persisted_head: ReadHead,
) -> eyre::Result<()>
where
    Block: reth_primitives_traits::Block + 'static,
    ReadHead: FnOnce() -> eyre::Result<u64>,
{
    let target = read_persisted_head()?;
    let result = slot
        .as_ref()
        .ok_or_else(|| eyre!("proof-history engine is not installed"))?
        .sync_to(target);
    if let Err(error) = result {
        readiness.set_not_ready();
        clear_engine_slot(slot).await?;
        return Err(error)
    }
    Ok(())
}

/// Clears readiness and joins the sole installed engine during shutdown or channel closure.
async fn shutdown_engine<Block>(
    slot: &mut EngineSlot<Block>,
    readiness: &ProofHistoryReadiness,
) -> eyre::Result<()>
where
    Block: reth_primitives_traits::Block + 'static,
{
    readiness.set_not_ready();
    clear_engine_slot(slot).await
}

/// Runs an external pruner spawn callback at most once for the sidecar lifetime.
fn spawn_pruner_once<Spawn>(spawned: &mut bool, spawn: Spawn)
where
    Spawn: FnOnce(),
{
    if !*spawned {
        spawn();
        *spawned = true;
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
    /// Stored proof-history must remove a divergent suffix before syncing forward.
    Unwind {
        /// First stored block removed by the unwind, including its validated canonical parent.
        first_removed: BlockWithParent,
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

/// Result of reconciling an installed engine after a routed reorg or revert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconciliationOutcome {
    /// Persisted canonical state and committed proof storage agree without replacing the engine.
    Ready,
    /// Persisted canonical state has not reached the committed proof head yet.
    WaitingForPersistence,
    /// A conflicting persisted branch required receiver-first live engine recovery.
    Recovered,
}

/// Whether routing one notification requires a persisted-state reconciliation pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotificationHandlingOutcome {
    /// The notification needs no persisted-state barrier before another event is consumed.
    Complete,
    /// A reorg or revert was routed and must be checked against persisted canonical state.
    ReconcilePersistedState,
}

/// Whether canonical notifications may be consumed while reorg persistence catches up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ReconciliationState {
    /// Persisted state is reconciled, so the notification receiver may advance.
    #[default]
    Reconciled,
    /// Persisted state trails a routed reorg, so notifications must remain buffered.
    Pending,
}

impl From<ReconciliationOutcome> for ReconciliationState {
    /// Converts one reconciliation attempt into the receiver-consumption state for the next loop.
    fn from(outcome: ReconciliationOutcome) -> Self {
        match outcome {
            ReconciliationOutcome::WaitingForPersistence => Self::Pending,
            ReconciliationOutcome::Ready | ReconciliationOutcome::Recovered => Self::Reconciled,
        }
    }
}

impl ReconciliationState {
    /// Returns whether the sidecar may consume another canonical notification.
    const fn should_receive_notifications(self) -> bool {
        matches!(self, Self::Reconciled)
    }
}

/// Reconciles an installed engine without using startup mutation while its sender remains live.
///
/// A persisted-state lag is retryable and leaves readiness clear. A divergent persisted branch
/// replaces the notification receiver before dropping the engine and running atomic live
/// reconciliation, so no startup unwind races an installed engine generation.
async fn reconcile_installed_engine<EnsureReady, ReplaceReceiver, Recover, RecoverFuture>(
    action: ProofHistoryStartupAction,
    readiness: &ProofHistoryReadiness,
    ensure_ready: EnsureReady,
    replace_receiver: ReplaceReceiver,
    recover: Recover,
) -> eyre::Result<ReconciliationOutcome>
where
    EnsureReady: FnOnce() -> eyre::Result<()>,
    ReplaceReceiver: FnOnce(),
    Recover: FnOnce() -> RecoverFuture,
    RecoverFuture: Future<Output = eyre::Result<()>>,
{
    match action {
        ProofHistoryStartupAction::Uninitialized => {
            readiness.set_not_ready();
            Err(eyre!(
                "installed proof-history engine lost its initialized V2 window during canonical reconciliation"
            ))
        }
        ProofHistoryStartupAction::Ready => {
            ensure_ready().inspect_err(|_| readiness.set_not_ready())?;
            readiness.set_ready();
            Ok(ReconciliationOutcome::Ready)
        }
        ProofHistoryStartupAction::WaitForCanonicalLatest { latest, canonical_best } => {
            readiness.set_not_ready();
            warn!(
                target: "reth::taiko::proof_history",
                latest,
                canonical_best,
                "persisted canonical state is behind a routed proof-history reorg; buffering notifications until it catches up"
            );
            Ok(ReconciliationOutcome::WaitingForPersistence)
        }
        ProofHistoryStartupAction::Unwind { first_removed } => {
            readiness.set_not_ready();
            warn!(
                target: "reth::taiko::proof_history",
                first_removed = first_removed.block.number,
                retained_parent = ?first_removed.parent,
                "persisted canonical state caught up on a conflicting branch; replacing the proof-history engine generation"
            );
            recover_after_lagged_receiver(replace_receiver, recover).await?;
            readiness.set_ready();
            Ok(ReconciliationOutcome::Recovered)
        }
    }
}

/// Canonical facts captured from one persisted main-database read transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProofHistoryCanonicalSnapshot {
    /// Highest canonical block persisted in the snapshot.
    canonical_best: u64,
    /// Canonical hash at the stored earliest height, when that height is persisted.
    canonical_earliest_hash: Option<B256>,
    /// Canonical hash at the stored latest height, when that height is persisted.
    canonical_latest_hash: Option<B256>,
    /// Canonical child immediately above the stored earliest block, resolved for a possible
    /// suffix unwind and validated to descend from the retained anchor.
    first_removed: Option<BlockWithParent>,
}

/// Determines how proof-history storage should be reconciled against canonical block hashes.
pub(super) fn proof_history_startup_action(
    proof_window: Option<ProofWindowRange>,
    canonical_best: u64,
    canonical_earliest_hash: Option<B256>,
    canonical_latest_hash: Option<B256>,
    first_removed: Option<BlockWithParent>,
    storage_path: &Path,
) -> eyre::Result<ProofHistoryStartupAction> {
    let Some(ProofWindowRange { earliest, latest }) = proof_window else {
        return Ok(ProofHistoryStartupAction::Uninitialized);
    };

    if earliest.number > latest.number {
        return Err(eyre!(
            "proof-history storage at {} has earliest block {} after latest block {}; wipe that proof-history directory or use a fresh path and restart initialization",
            storage_path.display(),
            earliest.number,
            latest.number,
        ));
    }

    // The stored head runs ahead of the canonical chain. This is the normal aftermath of an
    // ungraceful restart: live indexing follows in-memory commits, which outpace the persisted
    // head by up to the engine persistence threshold. Wait for canonical state to catch back up
    // and only then decide between `Ready` and an unwind — discarding the retained window here
    // would trade a few seconds of catch-up for days of re-execution.
    //
    // When the persisted chain has already reached the retained earliest height, still validate
    // that anchor before waiting for the latest height. A stale earliest block cannot become
    // canonical merely by allowing the chain to advance.
    if earliest.number > canonical_best {
        return Ok(ProofHistoryStartupAction::WaitForCanonicalLatest {
            latest: latest.number,
            canonical_best,
        });
    }

    if canonical_earliest_hash != Some(earliest.hash) {
        let label = if earliest == latest {
            "one-block initialization anchor"
        } else {
            "earliest stored block"
        };
        return Err(eyre!(
            "proof-history {label} {} hash {:?} is not canonical in the persisted database; wipe proof-history storage at {} or use a fresh path and restart initialization",
            earliest.number,
            earliest.hash,
            storage_path.display(),
        ));
    }

    if latest.number > canonical_best {
        return Ok(ProofHistoryStartupAction::WaitForCanonicalLatest {
            latest: latest.number,
            canonical_best,
        });
    }

    let Some(canonical_latest) = canonical_latest_hash else {
        return Err(eyre!(
            "canonical database snapshot has no header for stored proof-history latest block {} at or below canonical best {}; wipe proof-history storage at {} or use a fresh path and restart initialization",
            latest.number,
            canonical_best,
            storage_path.display(),
        ));
    };

    if canonical_latest == latest.hash {
        return Ok(ProofHistoryStartupAction::Ready);
    }

    if earliest == latest {
        return Err(eyre!(
            "proof-history one-block initialization anchor {} hash {:?} is not canonical in the persisted database; wipe proof-history storage at {} or use a fresh path and restart initialization",
            earliest.number,
            earliest.hash,
            storage_path.display(),
        ));
    }

    let first_removed = first_removed.ok_or_else(|| {
        eyre!(
            "canonical database snapshot has no validated child above proof-history earliest block {} hash {:?}; wipe proof-history storage at {} or use a fresh path and restart initialization",
            earliest.number,
            earliest.hash,
            storage_path.display(),
        )
    })?;

    // The canonical chain reached the stored height with a different block: real divergence
    // (a reorg happened while the sidecar was down). Carry the child resolved from the same
    // snapshot so the unwind cannot switch branches before its write transaction opens.
    Ok(ProofHistoryStartupAction::Unwind { first_removed })
}

/// Reads a proof range and its canonical reconciliation inputs exactly once each.
fn proof_history_startup_reconciliation<ReadProofWindow, ReadCanonicalSnapshot>(
    read_proof_window: ReadProofWindow,
    read_canonical_snapshot: ReadCanonicalSnapshot,
    storage_path: &Path,
) -> eyre::Result<ProofHistoryStartupAction>
where
    ReadProofWindow: FnOnce() -> eyre::Result<Option<ProofWindowRange>>,
    ReadCanonicalSnapshot: FnOnce(ProofWindowRange) -> eyre::Result<ProofHistoryCanonicalSnapshot>,
{
    let proof_window = read_proof_window()?;
    let Some(range) = proof_window else {
        return Ok(ProofHistoryStartupAction::Uninitialized);
    };
    let snapshot = read_canonical_snapshot(range)?;
    proof_history_startup_action(
        Some(range),
        snapshot.canonical_best,
        snapshot.canonical_earliest_hash,
        snapshot.canonical_latest_hash,
        snapshot.first_removed,
        storage_path,
    )
}

/// Captures canonical endpoint hashes and a possible unwind child from one main-database snapshot.
fn proof_history_canonical_snapshot<Provider>(
    provider: &Provider,
    proof_window: ProofWindowRange,
    storage_path: &Path,
) -> eyre::Result<ProofHistoryCanonicalSnapshot>
where
    Provider: BlockNumReader + HeaderProvider,
{
    let canonical_best = provider.best_block_number()?;
    let canonical_earliest = (proof_window.earliest.number <= canonical_best)
        .then(|| provider.sealed_header(proof_window.earliest.number))
        .transpose()?
        .flatten();
    let canonical_latest = (proof_window.latest.number <= canonical_best)
        .then(|| provider.sealed_header(proof_window.latest.number))
        .transpose()?
        .flatten();
    let canonical_earliest_hash = canonical_earliest.as_ref().map(|header| header.hash());
    let canonical_latest_hash = canonical_latest.as_ref().map(|header| header.hash());

    let first_removed = if proof_window.earliest.number < proof_window.latest.number &&
        canonical_earliest_hash == Some(proof_window.earliest.hash) &&
        canonical_latest_hash != Some(proof_window.latest.hash) &&
        proof_window.latest.number <= canonical_best
    {
        let child_number = proof_window
            .earliest
            .number
            .checked_add(1)
            .ok_or_else(|| eyre!("cannot resolve a proof-history child beyond u64::MAX"))?;
        let child = provider.sealed_header(child_number)?.ok_or_else(|| {
            eyre!(
                "canonical database snapshot has no proof-history unwind child at block {child_number}; wipe proof-history storage at {} or use a fresh path and restart initialization",
                storage_path.display(),
            )
        })?;
        if child.parent_hash() != proof_window.earliest.hash {
            return Err(eyre!(
                "canonical proof-history unwind child {child_number} has parent {:?}, expected retained earliest hash {:?}; wipe proof-history storage at {} or use a fresh path and restart initialization",
                child.parent_hash(),
                proof_window.earliest.hash,
                storage_path.display(),
            ));
        }
        Some(BlockWithParent::new(
            child.parent_hash(),
            BlockNumHash::new(child_number, child.hash()),
        ))
    } else {
        None
    };

    Ok(ProofHistoryCanonicalSnapshot {
        canonical_best,
        canonical_earliest_hash,
        canonical_latest_hash,
        first_removed,
    })
}

/// Taiko proof-history sidecar that keeps OP proofs storage behind locally executed state.
pub(super) struct ProofHistorySidecar<Node, Storage, EngineFactory>
where
    Node: FullNodeComponents,
{
    /// Canonical provider used for state notifications and block reads.
    provider: Node::Provider,
    /// Task executor used to spawn critical proof-history workers.
    task_executor: TaskExecutor,
    /// Proof-history storage populated by the extension.
    storage: OpProofsStorage<Storage>,
    /// Raw proof-history storage handle used for initialization and backward backfill.
    init_storage: Storage,
    /// Runtime settings that govern proof-history retention and startup behavior.
    config: ProofHistoryConfig,
    /// Readiness flag consumed by the RPC layer; set only while storage is reconciled.
    readiness: ProofHistoryReadiness,
    /// Factory that starts a fresh, independently owned upstream engine generation.
    engine_factory: EngineFactory,
}

/// Returns whether a committed chain starting at `first_block` leaves no gap above the stored
/// proof-history head, so its blocks can be consumed directly from the notification.
const fn committed_chain_is_contiguous(first_block: u64, latest_stored: u64) -> bool {
    first_block <= latest_stored.saturating_add(1)
}

/// Returns whether a block must be independently executed to verify precomputed trie data.
const fn proof_history_verification_due(verification_interval: u64, block_number: u64) -> bool {
    verification_interval > 0 && block_number.is_multiple_of(verification_interval)
}

/// Routes one exact recovered notification block through the proof-history engine.
///
/// Precomputed updates are accepted only outside configured verification heights. Missing trie
/// data and verification heights execute the exact block carried by the notification; this
/// function never resolves a block by number from the mutable canonical provider.
fn process_notification_block<Primitives, Engine>(
    block_number: u64,
    chain: &Chain<Primitives>,
    verification_interval: u64,
    engine: &Engine,
) -> eyre::Result<()>
where
    Primitives: NodePrimitives,
    Engine: ProofHistoryEngine<Primitives::Block> + ?Sized,
{
    let block = chain.blocks().get(&block_number).ok_or_else(|| {
        eyre!(
            "canonical notification is missing exact proof-history block {block_number}; engine sync is required"
        )
    })?;

    if !proof_history_verification_due(verification_interval, block_number) &&
        let Some(trie_data) = chain.trie_data_at(block_number)
    {
        let SortedTrieData { hashed_state, trie_updates } = &trie_data.get().sorted;
        engine.index_block(
            block.block_with_parent(),
            (**trie_updates).clone(),
            (**hashed_state).clone(),
        )?;
    } else {
        engine.execute_block(block)?;
    }

    Ok(())
}

/// Checks that a commit notification carries a complete parent-linked suffix above the committed
/// proof head.
///
/// A missing height requests canonical engine sync; a conflicting parent or overlapping stored
/// hash is a malformed commit and fails closed instead of extending the wrong branch.
fn commit_notification_covers_suffix<Primitives>(
    new: &Chain<Primitives>,
    committed_latest: BlockNumHash,
) -> eyre::Result<bool>
where
    Primitives: NodePrimitives,
{
    if let Some(overlap) = new.blocks().get(&committed_latest.number) &&
        overlap.hash() != committed_latest.hash
    {
        return Err(eyre!(
            "canonical commit overlaps proof-history block {} with hash {:?}, expected committed hash {:?}",
            committed_latest.number,
            overlap.hash(),
            committed_latest.hash
        ));
    }

    let Some(first_uncovered) = committed_latest.number.checked_add(1) else { return Ok(true) };
    let mut expected_parent = committed_latest.hash;
    for number in first_uncovered..=new.tip().number() {
        let Some(block) = new.blocks().get(&number) else { return Ok(false) };
        if block.parent_hash() != expected_parent {
            return Err(eyre!(
                "canonical commit block {number} has parent {:?}, expected committed suffix parent {:?}",
                block.parent_hash(),
                expected_parent
            ));
        }
        expected_parent = block.hash();
    }
    Ok(true)
}

/// Committed proof identity and canonical hashes captured before routing one commit notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CommitRoutingSnapshot {
    /// Latest committed proof block that a current commit may extend.
    committed_latest: BlockNumHash,
    /// Canonical hash at the committed latest height from one pinned main-database snapshot.
    canonical_latest_hash: Option<B256>,
    /// Canonical hash at the notification tip height from the same pinned snapshot.
    canonical_commit_tip_hash: Option<B256>,
}

/// Routes a commit notification, processing only the suffix not already present in committed
/// proof-history storage.
///
/// A gap is delegated to the engine's canonical sync path. Engine failures clear readiness so
/// RPC callers never observe a sidecar that knows notification processing failed.
fn route_chain_commit<Primitives, Engine>(
    new: &Chain<Primitives>,
    snapshot: CommitRoutingSnapshot,
    verification_interval: u64,
    engine: &Engine,
    readiness: &ProofHistoryReadiness,
) -> eyre::Result<()>
where
    Primitives: NodePrimitives,
    Engine: ProofHistoryEngine<Primitives::Block> + ?Sized,
{
    if new.is_empty() {
        readiness.set_not_ready();
        return Err(eyre!("canonical commit notification contains no blocks"));
    }

    if snapshot.canonical_latest_hash != Some(snapshot.committed_latest.hash) {
        readiness.set_not_ready();
        return Err(eyre!(
            "committed proof-history latest block {} hash {:?} is not canonical in the pinned notification snapshot",
            snapshot.committed_latest.number,
            snapshot.committed_latest.hash
        ));
    }

    if new.tip().number() <= snapshot.committed_latest.number {
        return Ok(())
    }

    // A buffered notification may describe an old branch, or its tip may not be persisted yet.
    // Once the committed proof head is proven canonical, ignoring that notification is safe: a
    // later notification or periodic engine sync will observe the persisted canonical target.
    if snapshot.canonical_commit_tip_hash != Some(new.tip().hash()) {
        return Ok(())
    }

    if !committed_chain_is_contiguous(new.first().number(), snapshot.committed_latest.number) ||
        !commit_notification_covers_suffix(new, snapshot.committed_latest)
            .inspect_err(|_| readiness.set_not_ready())?
    {
        return engine.sync_to(new.tip().number()).inspect_err(|_| readiness.set_not_ready())
    }

    for block_number in snapshot.committed_latest.number.saturating_add(1)..=new.tip().number() {
        process_notification_block(block_number, new, verification_interval, engine)
            .inspect_err(|_| readiness.set_not_ready())?;
    }

    Ok(())
}

/// Validates that a replacement chain describes one contiguous branch from the same fork as the
/// removed chain.
///
/// Validation completes before any engine mutation so malformed notifications fail closed
/// without partially unwinding proof history.
fn validate_replacement_chain<Primitives>(
    old: &Chain<Primitives>,
    new: &Chain<Primitives>,
) -> eyre::Result<()>
where
    Primitives: NodePrimitives,
{
    if old.is_empty() {
        return Err(eyre!("canonical reorg notification contains no old blocks"));
    }
    if new.is_empty() {
        return Ok(())
    }

    if old.fork_block() != new.fork_block() {
        return Err(eyre!(
            "proof-history fork blocks do not match: old={:?}, new={:?}",
            old.fork_block(),
            new.fork_block()
        ));
    }
    if old.first().number() != new.first().number() {
        return Err(eyre!(
            "proof-history replacement starts at block {}, expected old branch height {}",
            new.first().number(),
            old.first().number()
        ));
    }
    if old.first().parent_hash() != new.first().parent_hash() {
        return Err(eyre!(
            "proof-history replacement block {} has parent {:?}, expected old branch parent {:?}",
            new.first().number(),
            new.first().parent_hash(),
            old.first().parent_hash()
        ));
    }

    let mut expected_number = new.first().number();
    let mut expected_parent = new.first().parent_hash();
    for (number, block) in new.blocks() {
        if *number != expected_number {
            return Err(eyre!(
                "proof-history replacement is not contiguous: found block {number}, expected {expected_number}"
            ));
        }
        if block.parent_hash() != expected_parent {
            return Err(eyre!(
                "proof-history replacement block {number} has parent {:?}, expected {:?}",
                block.parent_hash(),
                expected_parent
            ));
        }
        expected_parent = block.hash();
        if *number != new.tip().number() {
            expected_number = number.checked_add(1).ok_or_else(|| {
                eyre!("proof-history replacement cannot continue beyond block u64::MAX")
            })?;
        }
    }

    Ok(())
}

/// Proof-window identities and canonical hashes captured before routing one reorg notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReorgRoutingSnapshot {
    /// Earliest retained proof block, which the replacement must preserve.
    committed_earliest: BlockNumHash,
    /// Latest committed proof block used to detect a covered replacement suffix.
    committed_latest: BlockNumHash,
    /// Canonical hash at the committed latest height from one pinned main-database snapshot.
    canonical_latest_hash: Option<B256>,
    /// Canonical hash at the replacement tip height from the same pinned snapshot.
    canonical_replacement_tip_hash: Option<B256>,
}

/// Routes a reorg through either one precomputed engine replacement or an unwind followed by
/// exact ordered replay.
///
/// The committed latest hash is considered covered only when one pinned canonical snapshot
/// confirms both that exact identity and the replacement tip. This prevents a same-height
/// old-fork proof head or a stale pre-reorg snapshot from consuming a replacement notification.
/// Readiness is cleared before validation and remains clear until the caller reconciles committed
/// storage with [`reconcile_installed_engine`].
fn route_chain_reorg<Primitives, Engine>(
    old: &Chain<Primitives>,
    new: &Chain<Primitives>,
    snapshot: ReorgRoutingSnapshot,
    verification_interval: u64,
    engine: &Engine,
    readiness: &ProofHistoryReadiness,
) -> eyre::Result<()>
where
    Primitives: NodePrimitives,
    Engine: ProofHistoryEngine<Primitives::Block> + ?Sized,
{
    readiness.set_not_ready();
    validate_replacement_chain(old, new)?;

    let first_old = BlockNumHash::new(old.first().number(), old.first().hash());
    ensure_canonical_update_above_earliest("reorg", snapshot.committed_earliest, first_old)?;
    ensure_retained_boundary_parent(
        "reorg",
        snapshot.committed_earliest,
        old.first().block_with_parent(),
    )?;

    if new.is_empty() {
        return engine.unwind(old.first().block_with_parent())
    }

    if snapshot.committed_latest.number >= new.tip().number() &&
        snapshot.canonical_latest_hash == Some(snapshot.committed_latest.hash) &&
        snapshot.canonical_replacement_tip_hash == Some(new.tip().hash())
    {
        return Ok(())
    }

    let can_reorg_directly = new.blocks().keys().all(|number| new.trie_data_at(*number).is_some()) &&
        new.blocks()
            .keys()
            .all(|number| !proof_history_verification_due(verification_interval, *number));

    if can_reorg_directly {
        let mut updates: ReorgBlockUpdates = Vec::with_capacity(new.len());
        for (number, block) in new.blocks() {
            let trie_data = new
                .trie_data_at(*number)
                .expect("direct reorg eligibility checked every replacement block");
            let SortedTrieData { hashed_state, trie_updates } = &trie_data.get().sorted;
            updates.push((block.block_with_parent(), trie_updates.clone(), hashed_state.clone()));
        }
        engine.reorg(updates)?;
    } else {
        engine.unwind(old.first().block_with_parent())?;
        for number in new.blocks().keys().copied() {
            process_notification_block(number, new, verification_interval, engine)?;
        }
    }

    Ok(())
}

/// Routes a retained-range revert through an inclusive engine unwind.
///
/// Safe reverts are always forwarded, even when committed storage already ends at the common
/// ancestor, because the engine may still hold an unpersisted old-fork suffix. Errors and
/// successful mutations both leave readiness clear until committed storage is reconciled.
fn route_chain_revert<Primitives, Engine>(
    old: &Chain<Primitives>,
    committed_earliest: BlockNumHash,
    engine: &Engine,
    readiness: &ProofHistoryReadiness,
) -> eyre::Result<()>
where
    Primitives: NodePrimitives,
    Engine: ProofHistoryEngine<Primitives::Block> + ?Sized,
{
    readiness.set_not_ready();
    if old.is_empty() {
        return Err(eyre!("canonical revert notification contains no old blocks"));
    }
    ensure_canonical_update_above_earliest(
        "revert",
        committed_earliest,
        BlockNumHash::new(old.first().number(), old.first().hash()),
    )?;
    ensure_retained_boundary_parent("revert", committed_earliest, old.first().block_with_parent())?;
    engine.unwind(old.first().block_with_parent())
}

/// Clears readiness for a reorg or revert before notification routing begins.
fn prepare_notification_readiness<Primitives>(
    notification: &CanonStateNotification<Primitives>,
    readiness: &ProofHistoryReadiness,
) where
    Primitives: NodePrimitives,
{
    if matches!(notification, CanonStateNotification::Reorg { .. }) {
        readiness.set_not_ready();
    }
}

/// Reads a notification routing snapshot while enforcing readiness failure semantics.
///
/// Any snapshot error, including during commit validation, clears readiness before it is
/// propagated. Reorg/revert callers additionally use [`prepare_notification_readiness`] before
/// reading their routing snapshot.
fn read_notification_routing_snapshot<Snapshot, ReadSnapshot>(
    readiness: &ProofHistoryReadiness,
    read_snapshot: ReadSnapshot,
) -> eyre::Result<Snapshot>
where
    ReadSnapshot: FnOnce() -> eyre::Result<Snapshot>,
{
    read_snapshot().inspect_err(|_| readiness.set_not_ready())
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

/// Ensures an update immediately above the retained boundary descends from its exact hash.
fn ensure_retained_boundary_parent(
    update_kind: &'static str,
    committed_earliest: BlockNumHash,
    first_old: BlockWithParent,
) -> eyre::Result<()> {
    if committed_earliest.number.checked_add(1) == Some(first_old.block.number) &&
        first_old.parent != committed_earliest.hash
    {
        return Err(eyre!(
            "proof-history boundary {update_kind} block {} has parent {:?}, expected retained earliest hash {:?}",
            first_old.block.number,
            first_old.parent,
            committed_earliest.hash
        ));
    }
    Ok(())
}

impl<Node, Storage, EngineFactory> ProofHistorySidecar<Node, Storage, EngineFactory>
where
    Node: FullNodeComponents,
{
    /// Creates a proof-history sidecar with one injected upstream-engine factory.
    pub(super) fn new(
        provider: Node::Provider,
        task_executor: TaskExecutor,
        storage: OpProofsStorage<Storage>,
        init_storage: Storage,
        config: ProofHistoryConfig,
        readiness: ProofHistoryReadiness,
        engine_factory: EngineFactory,
    ) -> Self {
        Self { provider, task_executor, storage, init_storage, config, readiness, engine_factory }
    }
}

impl<Node, Storage, EngineFactory, Primitives> ProofHistorySidecar<Node, Storage, EngineFactory>
where
    Node: FullNodeComponents<Types: NodeTypes<Primitives = Primitives>>,
    Node::Provider: BlockHashReader
        + BlockNumReader
        + BlockReader
        + CanonStateSubscriptions
        + DatabaseProviderFactory,
    <Node::Provider as DatabaseProviderFactory>::Provider: BlockNumReader
        + ChainStateBlockReader
        + ChangeSetReader
        + DBProvider
        + HeaderProvider
        + StageCheckpointReader
        + StorageChangeSetReader
        + StorageSettingsCache,
    <Node::Provider as DatabaseProviderFactory>::DB: Database,
    <<Node::Provider as DatabaseProviderFactory>::DB as Database>::TX: Sync,
    Primitives: NodePrimitives,
    Primitives::Block: 'static,
    Storage: OpProofsBackfillStore + Clone + Send + 'static,
    EngineFactory: Fn() -> eyre::Result<Box<dyn ProofHistoryEngine<Primitives::Block>>>
        + Send
        + Sync
        + 'static,
{
    /// Runs proof-history indexing until the node shuts down, then joins the sole engine.
    pub(super) async fn run(self, mut shutdown: GracefulShutdown) -> eyre::Result<()> {
        let mut engine: EngineSlot<Primitives::Block> = None;
        let run_result = self.run_loop(&mut shutdown, &mut engine).await;
        let shutdown_result = shutdown_engine(&mut engine, &self.readiness).await;
        match run_result {
            Err(error) => Err(error),
            Ok(()) => shutdown_result,
        }
    }

    /// Drives notifications, delayed startup, lag recovery, and persisted-head polling.
    async fn run_loop(
        &self,
        shutdown: &mut GracefulShutdown,
        engine: &mut EngineSlot<Primitives::Block>,
    ) -> eyre::Result<()> {
        let mut notifications = self.provider.subscribe_to_canonical_state();
        let mut reconciliation_state = ReconciliationState::default();
        let mut pruner_spawned = false;
        if self.try_start(engine).await? {
            spawn_pruner_once(&mut pruner_spawned, || self.spawn_pruner_task());
        }

        let mut retry_interval = time::interval_at(
            Instant::now() + PROOF_HISTORY_DELAYED_START_RETRY_INTERVAL,
            PROOF_HISTORY_DELAYED_START_RETRY_INTERVAL,
        );
        retry_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut head_poll = time::interval_at(
            Instant::now() + PROOF_HISTORY_HEAD_POLL_INTERVAL,
            PROOF_HISTORY_HEAD_POLL_INTERVAL,
        );
        head_poll.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                notification = notifications.recv(), if reconciliation_state.should_receive_notifications() => {
                    let notification = match notification {
                        Ok(notification) => notification,
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                target: "reth::taiko::proof_history",
                                skipped,
                                "proof-history sidecar lagged canonical notifications; replacing its engine generation"
                            );
                            recover_after_lagged_receiver(
                                || {
                                    // Replace first so commits published during recovery buffer in
                                    // the fresh receiver rather than extending the incomplete stream.
                                    notifications = self.provider.subscribe_to_canonical_state();
                                },
                                || self.recover_from_lag(engine),
                            ).await?;
                            spawn_pruner_once(&mut pruner_spawned, || self.spawn_pruner_task());
                            head_poll.reset_after(PROOF_HISTORY_HEAD_POLL_INTERVAL);
                            continue;
                        }
                    };

                    if engine.is_none() && self.try_start(engine).await? {
                        spawn_pruner_once(&mut pruner_spawned, || self.spawn_pruner_task());
                        head_poll.reset_after(PROOF_HISTORY_HEAD_POLL_INTERVAL);
                    }
                    let Some(installed_engine) = engine.as_deref_mut() else { continue };
                    let handling = self.handle_notification(notification, installed_engine).await?;
                    if handling == NotificationHandlingOutcome::ReconcilePersistedState {
                        let outcome = self.reconcile_installed_generation(
                            || {
                                // A conflicting persisted branch invalidates the buffered stream.
                                // Replace it before dropping the old engine and opening the live
                                // reconciliation snapshot.
                                notifications = self.provider.subscribe_to_canonical_state();
                            },
                            || self.recover_from_lag(engine),
                        ).await?;
                        reconciliation_state = outcome.into();
                        match outcome {
                            ReconciliationOutcome::WaitingForPersistence => {
                                head_poll.reset_after(PROOF_HISTORY_HEAD_POLL_INTERVAL);
                            }
                            ReconciliationOutcome::Recovered => {
                                spawn_pruner_once(
                                    &mut pruner_spawned,
                                    || self.spawn_pruner_task(),
                                );
                                head_poll.reset_after(PROOF_HISTORY_HEAD_POLL_INTERVAL);
                            }
                            ReconciliationOutcome::Ready => {}
                        }
                    }
                }
                _ = &mut *shutdown => break,
                _ = retry_interval.tick(), if engine.is_none() => {
                    if self.try_start(engine).await? {
                        spawn_pruner_once(&mut pruner_spawned, || self.spawn_pruner_task());
                        head_poll.reset_after(PROOF_HISTORY_HEAD_POLL_INTERVAL);
                    }
                }
                _ = head_poll.tick(), if engine.is_some() => {
                    if reconciliation_state == ReconciliationState::Pending {
                        let outcome = self.reconcile_installed_generation(
                            || {
                                // Replace first so notifications published during live recovery
                                // accumulate on the fresh receiver.
                                notifications = self.provider.subscribe_to_canonical_state();
                            },
                            || self.recover_from_lag(engine),
                        ).await?;
                        reconciliation_state = outcome.into();
                        match outcome {
                            ReconciliationOutcome::WaitingForPersistence => continue,
                            ReconciliationOutcome::Recovered => {
                                spawn_pruner_once(
                                    &mut pruner_spawned,
                                    || self.spawn_pruner_task(),
                                );
                                head_poll.reset_after(PROOF_HISTORY_HEAD_POLL_INTERVAL);
                                continue;
                            }
                            ReconciliationOutcome::Ready => {}
                        }
                    }
                    if let Err(error) = sync_engine_to_persisted_head(
                        engine,
                        &self.readiness,
                        || self.persisted_executed_head(),
                    ).await {
                        if engine.is_some() {
                            warn!(
                                target: "reth::taiko::proof_history",
                                ?error,
                                "failed to read persisted proof-history sync head; retaining engine for the next poll"
                            );
                        } else {
                            error!(
                                target: "reth::taiko::proof_history",
                                ?error,
                                "proof-history engine sync channel failed; generation was joined and will be restarted"
                            );
                            retry_interval
                                .reset_after(PROOF_HISTORY_DELAYED_START_RETRY_INTERVAL);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Performs startup reconciliation, then spawns and initially syncs one engine generation.
    async fn try_start(&self, engine: &mut EngineSlot<Primitives::Block>) -> eyre::Result<bool> {
        if engine.is_some() {
            return Ok(true);
        }
        self.readiness.set_not_ready();
        if !self.prepare_storage_or_wait().await? || !self.reconcile_or_wait().await? {
            return Ok(false);
        }
        install_engine_generation(
            engine,
            &self.readiness,
            || (self.engine_factory)(),
            || self.persisted_executed_head(),
        )
        .await?;
        Ok(true)
    }

    /// Drops a lagged generation, atomically reconciles V2, and installs a fresh generation.
    async fn recover_from_lag(
        &self,
        engine: &mut EngineSlot<Primitives::Block>,
    ) -> eyre::Result<()> {
        recover_engine_after_lag(
            engine,
            &self.readiness,
            || self.reconcile_live_storage(),
            || (self.engine_factory)(),
            || self.persisted_executed_head(),
        )
        .await
    }

    /// Rechecks persisted state for a routed reorg while preserving installed-engine ownership.
    async fn reconcile_installed_generation<ReplaceReceiver, Recover, RecoverFuture>(
        &self,
        replace_receiver: ReplaceReceiver,
        recover: Recover,
    ) -> eyre::Result<ReconciliationOutcome>
    where
        ReplaceReceiver: FnOnce(),
        Recover: FnOnce() -> RecoverFuture,
        RecoverFuture: Future<Output = eyre::Result<()>>,
    {
        reconcile_installed_engine(
            self.startup_action()?,
            &self.readiness,
            || self.ensure_initialized(),
            replace_receiver,
            recover,
        )
        .await
    }

    /// Reads the executed head from a fresh persisted main-database snapshot.
    fn persisted_executed_head(&self) -> eyre::Result<u64> {
        Ok(self.provider.database_provider_ro()?.best_block_number()?)
    }

    /// Reconciles current proof-history bounds against the canonical database.
    async fn reconcile_or_wait(&self) -> eyre::Result<bool> {
        match self.startup_action()? {
            ProofHistoryStartupAction::Uninitialized => Ok(false),
            ProofHistoryStartupAction::Ready => {
                self.ensure_initialized()?;
                Ok(true)
            }
            ProofHistoryStartupAction::Unwind { first_removed } => {
                self.unwind_to_earliest(first_removed).await?;
                self.ensure_initialized()?;
                Ok(true)
            }
            ProofHistoryStartupAction::WaitForCanonicalLatest { latest, canonical_best } => {
                // Expected briefly after an ungraceful restart while the driver re-derives the
                // gap; warn so a *stuck* wait (chain unwound for good) stays visible.
                warn!(
                    target: "reth::taiko::proof_history",
                    latest,
                    canonical_best,
                    "canonical chain is behind the stored proof-history head; waiting for the node to catch up before reconciling"
                );
                Ok(false)
            }
        }
    }

    /// Computes the reconciliation action for the current proof-history storage bounds.
    fn startup_action(&self) -> eyre::Result<ProofHistoryStartupAction> {
        let storage_path = self.config.required_storage_path()?;
        proof_history_startup_reconciliation(
            || {
                let provider_ro = self.storage.provider_ro()?;
                match provider_ro.get_proof_window() {
                    Ok(range) => Ok(Some(range)),
                    Err(OpProofsStorageError::NoBlocksFound) => Ok(None),
                    Err(error) => Err(error.into()),
                }
            },
            |proof_window| {
                let provider_ro = self.provider.database_provider_ro()?;
                proof_history_canonical_snapshot(&provider_ro, proof_window, storage_path)
            },
            storage_path,
        )
    }

    /// Reconciles committed V2 storage against one pinned canonical snapshot during lag recovery.
    ///
    /// The proof RW transaction is opened first and owns both the decision window and any suffix
    /// unwind. MDBX therefore excludes the external pruner from changing the retained boundary
    /// between validation and mutation.
    async fn reconcile_live_storage(&self) -> eyre::Result<()> {
        let storage = self.storage.clone();
        let provider = self.provider.clone();
        let storage_path = self.config.required_storage_path()?.clone();
        let reconcile = task::spawn_blocking(move || {
            reconcile_live_storage_atomic(&storage, &provider, &storage_path)
        });

        let action =
            blocking_join_result(reconcile.await, "proof-history live reconciliation worker")??;
        match action {
            ProofHistoryLiveAction::Ready => {
                debug!(target: "reth::taiko::proof_history", "committed proof-history V2 window remains canonical after notification lag");
            }
            ProofHistoryLiveAction::Unwind { first_removed } => {
                info!(
                    target: "reth::taiko::proof_history",
                    first_removed = first_removed.block.number,
                    retained_parent = ?first_removed.parent,
                    "rolled proof-history suffix back to its retained canonical anchor after notification lag"
                );
            }
        }
        Ok(())
    }

    /// Removes the divergent suffix beginning at a child resolved by startup reconciliation.
    async fn unwind_to_earliest(&self, first_removed: BlockWithParent) -> eyre::Result<()> {
        info!(
            target: "reth::taiko::proof_history",
            first_removed = first_removed.block.number,
            retained_parent = ?first_removed.parent,
            "unwinding proof-history storage to retained canonical earliest block"
        );

        let storage = self.storage.clone();
        let unwind_task = task::spawn_blocking(move || -> Result<(), OpProofsStorageError> {
            let provider_rw = storage.provider_rw()?;
            provider_rw.unwind_history(first_removed)?;
            provider_rw.commit()
        });
        blocking_join_result(unwind_task.await, "proof-history unwind worker")??;
        Ok(())
    }

    /// Prepares proof storage with upstream initialization and optional finalized-window backfill.
    ///
    /// Returns `false` for retryable finality/execution waits and `true` only after preparation has
    /// completed; errors abort startup without changing RPC readiness.
    async fn prepare_storage_or_wait(&self) -> eyre::Result<bool> {
        let provider = self.provider.clone();
        let storage = self.init_storage.clone();
        let storage_path = self.config.required_storage_path()?.clone();
        let window = self.config.window;
        let backfill_window_only = self.config.backfill_window_only;
        let init_task = task::spawn_blocking(move || {
            if backfill_window_only {
                initialize_finalized_window_proof_history_storage(
                    &provider,
                    storage,
                    &storage_path,
                    window,
                )
            } else {
                initialize_proof_history_storage(&provider, storage, &storage_path)?;
                Ok(true)
            }
        });
        blocking_join_result(init_task.await, "proof-history preparation worker")?
    }

    /// Verifies the proof-history database is initialized and safe to prune automatically.
    fn ensure_initialized(&self) -> eyre::Result<()> {
        let provider_ro = self.storage.provider_ro()?;
        let proof_window = provider_ro
            .get_proof_window()
            .map_err(|error| eyre!("proof-history storage is not initialized: {error}"))?;
        let canonical_snapshot = self.provider.database_provider_ro()?;
        let blocks_to_prune =
            startup_prune_exposure(proof_window, &canonical_snapshot, self.config.window)?;
        if blocks_to_prune > self.config.max_startup_prune_blocks {
            return Err(eyre!(
                "configuration requires pruning {} proof-history blocks, which exceeds the safety threshold of {}; raise --proofs-history.max-startup-prune-blocks or restore the previous --proofs-history.window to proceed",
                blocks_to_prune,
                self.config.max_startup_prune_blocks
            ));
        }

        Ok(())
    }

    /// Spawns the periodic proof-history pruning task.
    fn spawn_pruner_task(&self) {
        let pruner = Arc::new(FinalityProofHistoryPruner::new(
            self.storage.clone(),
            self.provider.clone(),
            self.config.window,
        ));
        let prune_interval = self.config.prune_interval;
        let retention_window = self.config.window;

        self.task_executor
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
                                let pruner = pruner.clone();
                                let mut prune_task =
                                    task::spawn_blocking(move || pruner.run_once());
                                tokio::select! {
                                    result = &mut prune_task => log_prune_join_result(result),
                                    _ = &mut signal => {
                                        // `spawn_blocking` workers cannot be aborted, so wait for
                                        // the prune to finish to avoid tearing down a write txn
                                        // mid-flight. A deeper fix would need a cancel-aware
                                        // pruner API or smaller prune chunks.
                                        info!(
                                            target: "reth::taiko::proof_history",
                                            "shutdown requested while proof-history prune is running; waiting for prune to finish"
                                        );
                                        log_prune_join_result(prune_task.await);
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

    /// Handles one canonical notification using exact notification blocks and the engine seam.
    async fn handle_notification<Engine>(
        &self,
        notification: CanonStateNotification<Primitives>,
        engine: &mut Engine,
    ) -> eyre::Result<NotificationHandlingOutcome>
    where
        Engine: ProofHistoryEngine<Primitives::Block> + ?Sized,
    {
        prepare_notification_readiness(&notification, &self.readiness);
        let (
            earliest_stored,
            latest_stored,
            canonical_latest_hash,
            canonical_notification_tip_hash,
        ) = read_notification_routing_snapshot(&self.readiness, || {
            let provider_ro = self.storage.provider_ro()?;
            let ProofWindowRange { earliest: earliest_stored, latest: latest_stored } =
                provider_ro.get_proof_window()?;
            drop(provider_ro);

            // Pin both proof-head and notification-tip decisions to one persisted canonical
            // snapshot. Matching heights alone cannot distinguish a buffered old-fork event.
            let canonical_snapshot = self.provider.database_provider_ro()?;
            let canonical_latest_hash =
                canonical_snapshot.sealed_header(latest_stored.number)?.map(|header| header.hash());
            let notification_tip_number = match &notification {
                CanonStateNotification::Commit { new } if !new.is_empty() => {
                    Some(new.tip().number())
                }
                CanonStateNotification::Reorg { new, .. } if !new.is_empty() => {
                    Some(new.tip().number())
                }
                _ => None,
            };
            let canonical_notification_tip_hash = notification_tip_number
                .map(|number| canonical_snapshot.sealed_header(number))
                .transpose()?
                .flatten()
                .map(|header| header.hash());
            drop(canonical_snapshot);

            Ok((
                earliest_stored,
                latest_stored,
                canonical_latest_hash,
                canonical_notification_tip_hash,
            ))
        })?;

        let outcome = match &notification {
            CanonStateNotification::Commit { new } => {
                route_chain_commit(
                    new,
                    CommitRoutingSnapshot {
                        committed_latest: latest_stored,
                        canonical_latest_hash,
                        canonical_commit_tip_hash: canonical_notification_tip_hash,
                    },
                    self.config.verification_interval,
                    engine,
                    &self.readiness,
                )?;
                NotificationHandlingOutcome::Complete
            }
            // A reorg that replaces the old blocks with nothing is a plain revert.
            CanonStateNotification::Reorg { old, new } if new.is_empty() => {
                route_chain_revert(old, earliest_stored, engine, &self.readiness)?;
                NotificationHandlingOutcome::ReconcilePersistedState
            }
            CanonStateNotification::Reorg { old, new } => {
                route_chain_reorg(
                    old,
                    new,
                    ReorgRoutingSnapshot {
                        committed_earliest: earliest_stored,
                        committed_latest: latest_stored,
                        canonical_latest_hash,
                        canonical_replacement_tip_hash: canonical_notification_tip_hash,
                    },
                    self.config.verification_interval,
                    engine,
                    &self.readiness,
                )?;
                NotificationHandlingOutcome::ReconcilePersistedState
            }
        };

        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CommitRoutingSnapshot, EngineSlot, ProofHistoryCanonicalSnapshot, ProofHistoryLiveAction,
        ProofHistoryStartupAction, ReconciliationOutcome, ReconciliationState,
        ReorgRoutingSnapshot, committed_chain_is_contiguous,
        ensure_canonical_update_above_earliest, install_engine_generation,
        prepare_notification_readiness, proof_history_live_action, proof_history_startup_action,
        proof_history_startup_reconciliation, read_notification_routing_snapshot,
        reconcile_installed_engine, reconcile_live_storage_atomic, recover_after_lagged_receiver,
        recover_engine_after_lag, route_chain_commit, route_chain_reorg, route_chain_revert,
        shutdown_engine, spawn_pruner_once, sync_engine_to_persisted_head,
    };
    use crate::proof_history::engine::{ProofHistoryEngine, ReorgBlockUpdates};
    use alethia_reth_rpc::proof_state::ProofHistoryReadiness;
    use alloy_consensus::{BlockHeader, Header};
    use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
    use alloy_primitives::{B256, U256};
    use reth::providers::BlockWriter;
    use reth_db::Database;
    use reth_db_common::init::init_genesis;
    use reth_ethereum_primitives::{Block, BlockBody, EthPrimitives};
    use reth_execution_types::Chain;
    use reth_optimism_trie::{
        BlockStateDiff, InitializationJob, MdbxProofsStorageV2, OpProofsProviderRO,
        OpProofsProviderRw, OpProofsStorage, OpProofsStore, RethTrieStorageLayout,
        api::ProofWindowRange,
    };
    use reth_primitives_traits::{Block as _, RecoveredBlock};
    use reth_provider::{
        CanonStateNotification, ProviderFactory,
        test_utils::{MockNodeTypesWithDB, create_test_provider_factory},
    };
    use reth_storage_api::StorageSettingsCache;
    use reth_trie_common::{
        ComputedTrieData, HashedPostStateSorted, LazyTrieData, updates::TrieUpdatesSorted,
    };
    use std::{
        cell::Cell,
        collections::BTreeMap,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum EngineCall {
        Execute(BlockNumHash),
        Index(BlockWithParent),
        Reorg(Vec<BlockWithParent>),
        Unwind(BlockWithParent),
        Sync(u64),
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum EngineMethod {
        Execute,
        Index,
        Reorg,
        Unwind,
        Sync,
    }

    #[derive(Clone, Debug, Default)]
    struct RecordingEngine {
        calls: Arc<Mutex<Vec<EngineCall>>>,
        failure: Arc<Mutex<Option<EngineMethod>>>,
    }

    impl RecordingEngine {
        fn failing(method: EngineMethod) -> Self {
            Self { failure: Arc::new(Mutex::new(Some(method))), ..Default::default() }
        }

        fn calls(&self) -> Vec<EngineCall> {
            self.calls.lock().expect("recording engine lock is available").clone()
        }

        fn record(&self, method: EngineMethod, call: EngineCall) -> eyre::Result<()> {
            self.calls.lock().expect("recording engine lock is available").push(call);
            if self.failure.lock().expect("recording engine failure lock is available").as_ref() ==
                Some(&method)
            {
                return Err(eyre::eyre!("injected {method:?} failure"));
            }
            Ok(())
        }
    }

    impl ProofHistoryEngine<Block> for RecordingEngine {
        fn execute_block(&self, block: &RecoveredBlock<Block>) -> eyre::Result<()> {
            self.record(
                EngineMethod::Execute,
                EngineCall::Execute(BlockNumHash::new(block.number(), block.hash())),
            )
        }

        fn index_block(
            &self,
            block: BlockWithParent,
            _trie_updates: TrieUpdatesSorted,
            _post_state: HashedPostStateSorted,
        ) -> eyre::Result<()> {
            self.record(EngineMethod::Index, EngineCall::Index(block))
        }

        fn reorg(&self, updates: ReorgBlockUpdates) -> eyre::Result<()> {
            self.record(
                EngineMethod::Reorg,
                EngineCall::Reorg(updates.into_iter().map(|(block, _, _)| block).collect()),
            )
        }

        fn unwind(&self, from: BlockWithParent) -> eyre::Result<()> {
            self.record(EngineMethod::Unwind, EngineCall::Unwind(from))
        }

        fn sync_to(&self, target: u64) -> eyre::Result<()> {
            self.record(EngineMethod::Sync, EngineCall::Sync(target))
        }
    }

    fn test_block(number: u64, parent_hash: B256, marker: u8) -> Arc<RecoveredBlock<Block>> {
        Arc::new(
            Block {
                header: Header {
                    parent_hash,
                    number,
                    timestamp: marker.into(),
                    state_root: B256::repeat_byte(marker),
                    ..Default::default()
                },
                body: BlockBody::default(),
            }
            .try_into_recovered()
            .expect("empty test block recovers without senders"),
        )
    }

    fn test_chain(
        blocks: Vec<Arc<RecoveredBlock<Block>>>,
        precomputed_blocks: &[u64],
    ) -> Chain<EthPrimitives> {
        let trie_data = precomputed_blocks
            .iter()
            .copied()
            .map(|number| {
                (
                    number,
                    LazyTrieData::ready(ComputedTrieData::new(
                        Arc::new(HashedPostStateSorted::default()),
                        Arc::new(TrieUpdatesSorted::default()),
                    )),
                )
            })
            .collect::<BTreeMap<_, _>>();
        Chain::new(blocks, Default::default(), trie_data)
    }

    fn linear_chain(
        first_number: u64,
        parent_hash: B256,
        markers: &[u8],
        precomputed_blocks: &[u64],
    ) -> Chain<EthPrimitives> {
        let mut parent = parent_hash;
        let blocks = markers
            .iter()
            .enumerate()
            .map(|(offset, marker)| {
                let block = test_block(first_number + offset as u64, parent, *marker);
                parent = block.hash();
                block
            })
            .collect();
        test_chain(blocks, precomputed_blocks)
    }

    fn ready_flag() -> ProofHistoryReadiness {
        let readiness = ProofHistoryReadiness::new();
        readiness.set_ready();
        readiness
    }

    fn reorg_snapshot(
        committed_earliest: BlockNumHash,
        committed_latest: BlockNumHash,
        canonical_latest_hash: Option<B256>,
        canonical_replacement_tip_hash: Option<B256>,
    ) -> ReorgRoutingSnapshot {
        ReorgRoutingSnapshot {
            committed_earliest,
            committed_latest,
            canonical_latest_hash,
            canonical_replacement_tip_hash,
        }
    }

    fn commit_snapshot(
        committed_latest: BlockNumHash,
        canonical_latest_hash: Option<B256>,
        canonical_commit_tip_hash: Option<B256>,
    ) -> CommitRoutingSnapshot {
        CommitRoutingSnapshot { committed_latest, canonical_latest_hash, canonical_commit_tip_hash }
    }

    fn canonical_commit_snapshot(
        committed_latest: BlockNumHash,
        chain: &Chain<EthPrimitives>,
    ) -> CommitRoutingSnapshot {
        commit_snapshot(committed_latest, Some(committed_latest.hash), Some(chain.tip().hash()))
    }

    fn hash(byte: u8) -> B256 {
        B256::with_last_byte(byte)
    }

    fn proof_window(earliest: (u64, u8), latest: (u64, u8)) -> ProofWindowRange {
        ProofWindowRange {
            earliest: BlockNumHash::new(earliest.0, hash(earliest.1)),
            latest: BlockNumHash::new(latest.0, hash(latest.1)),
        }
    }

    fn unwind_marker(earliest: (u64, u8), child_hash: u8) -> BlockWithParent {
        BlockWithParent::new(hash(earliest.1), BlockNumHash::new(earliest.0 + 1, hash(child_hash)))
    }

    fn storage_path() -> &'static Path {
        Path::new("/configured/proof-history")
    }

    struct LiveReconcileFixture {
        factory: ProviderFactory<MockNodeTypesWithDB>,
        storage: OpProofsStorage<Arc<MdbxProofsStorageV2>>,
        canonical: Vec<BlockNumHash>,
        proof_path: PathBuf,
    }

    impl LiveReconcileFixture {
        fn ahead_of_persisted_head(anchor: u64, proof_latest: u64) -> Self {
            let factory = create_test_provider_factory();
            let genesis_hash = init_genesis(&factory).expect("genesis initializes");
            let mut canonical = vec![BlockNumHash::new(0, genesis_hash)];
            let provider = factory.provider_rw().expect("canonical writer opens");
            for number in 1..=anchor {
                let parent_hash = canonical.last().expect("canonical parent exists").hash;
                let block = RecoveredBlock::new_unhashed(
                    Block {
                        header: Header {
                            parent_hash,
                            number,
                            timestamp: number,
                            difficulty: U256::from(number),
                            ..Default::default()
                        },
                        body: BlockBody::default(),
                    },
                    Vec::new(),
                );
                provider.insert_block(&block).expect("canonical block inserts");
                canonical.push(BlockNumHash::new(number, block.hash()));
            }
            provider.commit().expect("canonical blocks commit");

            let proof_path = tempfile::tempdir().expect("proof tempdir").keep();
            let storage: OpProofsStorage<Arc<MdbxProofsStorageV2>> =
                Arc::new(MdbxProofsStorageV2::new(&proof_path).expect("V2 storage opens")).into();
            let layout = if factory.cached_storage_settings().is_v2() {
                RethTrieStorageLayout::Packed
            } else {
                RethTrieStorageLayout::Legacy
            };
            InitializationJob::new(
                storage.clone(),
                factory.db_ref().tx().expect("canonical snapshot opens"),
                layout,
            )
            .run(anchor, canonical[anchor as usize].hash)
            .expect("proof storage initializes at persisted anchor");

            let proof_rw = storage.provider_rw().expect("proof writer opens");
            let mut parent = canonical[anchor as usize].hash;
            for number in anchor.saturating_add(1)..=proof_latest {
                let block = BlockNumHash::new(number, B256::with_last_byte(number as u8));
                proof_rw
                    .store_trie_updates(
                        BlockWithParent::new(parent, block),
                        BlockStateDiff::default(),
                    )
                    .expect("synthetic proof suffix stores");
                parent = block.hash;
            }
            proof_rw.commit().expect("synthetic proof suffix commits");

            Self { factory, storage, canonical, proof_path }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum LifecycleEvent {
        ReceiverReplaced,
        Drop(u64),
        Reconcile,
        Spawn(u64),
        ReadPersistedHead,
        Sync(u64, u64),
    }

    struct LifecycleEngine {
        generation: u64,
        events: Arc<Mutex<Vec<LifecycleEvent>>>,
        fail_sync: bool,
    }

    impl Drop for LifecycleEngine {
        fn drop(&mut self) {
            self.events
                .lock()
                .expect("lifecycle event lock is available")
                .push(LifecycleEvent::Drop(self.generation));
        }
    }

    impl ProofHistoryEngine<Block> for LifecycleEngine {
        fn execute_block(&self, _block: &RecoveredBlock<Block>) -> eyre::Result<()> {
            Ok(())
        }

        fn index_block(
            &self,
            _block: BlockWithParent,
            _trie_updates: TrieUpdatesSorted,
            _post_state: HashedPostStateSorted,
        ) -> eyre::Result<()> {
            Ok(())
        }

        fn reorg(&self, _updates: ReorgBlockUpdates) -> eyre::Result<()> {
            Ok(())
        }

        fn unwind(&self, _from: BlockWithParent) -> eyre::Result<()> {
            Ok(())
        }

        fn sync_to(&self, target: u64) -> eyre::Result<()> {
            self.events
                .lock()
                .expect("lifecycle event lock is available")
                .push(LifecycleEvent::Sync(self.generation, target));
            if self.fail_sync {
                Err(eyre::eyre!("injected lifecycle sync failure"))
            } else {
                Ok(())
            }
        }
    }

    fn lifecycle_engine(
        generation: u64,
        events: Arc<Mutex<Vec<LifecycleEvent>>>,
    ) -> Box<dyn ProofHistoryEngine<Block>> {
        Box::new(LifecycleEngine { generation, events, fail_sync: false })
    }

    fn lifecycle_engine_with_sync_failure(
        generation: u64,
        events: Arc<Mutex<Vec<LifecycleEvent>>>,
    ) -> Box<dyn ProofHistoryEngine<Block>> {
        Box::new(LifecycleEngine { generation, events, fail_sync: true })
    }

    #[tokio::test]
    async fn lag_recovery_drops_old_engine_before_reconciliation() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = Some(lifecycle_engine(1, events.clone()));
        let readiness = ready_flag();

        recover_engine_after_lag(
            &mut engine,
            &readiness,
            {
                let events = events.clone();
                move || async move {
                    events
                        .lock()
                        .expect("lifecycle event lock is available")
                        .push(LifecycleEvent::Reconcile);
                    Ok(())
                }
            },
            {
                let events = events.clone();
                move || {
                    events
                        .lock()
                        .expect("lifecycle event lock is available")
                        .push(LifecycleEvent::Spawn(2));
                    Ok(lifecycle_engine(2, events.clone()))
                }
            },
            {
                let events = events.clone();
                move || {
                    events
                        .lock()
                        .expect("lifecycle event lock is available")
                        .push(LifecycleEvent::ReadPersistedHead);
                    Ok(17)
                }
            },
        )
        .await
        .expect("lag recovery succeeds");

        assert!(readiness.is_ready());
        assert!(engine.is_some());
        assert_eq!(
            *events.lock().expect("lifecycle event lock is available"),
            vec![
                LifecycleEvent::Drop(1),
                LifecycleEvent::Reconcile,
                LifecycleEvent::Spawn(2),
                LifecycleEvent::ReadPersistedHead,
                LifecycleEvent::Sync(2, 17),
            ]
        );
    }

    #[tokio::test]
    async fn lag_recovery_orders_receiver_replacement_before_generation_rebuild() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = Some(lifecycle_engine(1, events.clone()));
        let readiness = ready_flag();

        recover_after_lagged_receiver(
            {
                let events = events.clone();
                move || {
                    events
                        .lock()
                        .expect("lifecycle event lock is available")
                        .push(LifecycleEvent::ReceiverReplaced);
                }
            },
            || {
                recover_engine_after_lag(
                    &mut engine,
                    &readiness,
                    {
                        let events = events.clone();
                        move || async move {
                            events
                                .lock()
                                .expect("lifecycle event lock is available")
                                .push(LifecycleEvent::Reconcile);
                            Ok(())
                        }
                    },
                    {
                        let events = events.clone();
                        move || {
                            events
                                .lock()
                                .expect("lifecycle event lock is available")
                                .push(LifecycleEvent::Spawn(2));
                            Ok(lifecycle_engine(2, events.clone()))
                        }
                    },
                    {
                        let events = events.clone();
                        move || {
                            events
                                .lock()
                                .expect("lifecycle event lock is available")
                                .push(LifecycleEvent::ReadPersistedHead);
                            Ok(72)
                        }
                    },
                )
            },
        )
        .await
        .expect("ordered lag recovery succeeds");

        assert_eq!(
            *events.lock().expect("lifecycle event lock is available"),
            vec![
                LifecycleEvent::ReceiverReplaced,
                LifecycleEvent::Drop(1),
                LifecycleEvent::Reconcile,
                LifecycleEvent::Spawn(2),
                LifecycleEvent::ReadPersistedHead,
                LifecycleEvent::Sync(2, 72),
            ]
        );
    }

    #[tokio::test]
    async fn lagged_receiver_with_empty_slot_still_runs_live_recovery_before_spawn() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = None;
        let readiness = ProofHistoryReadiness::new();

        recover_after_lagged_receiver(
            {
                let events = events.clone();
                move || {
                    events
                        .lock()
                        .expect("lifecycle event lock is available")
                        .push(LifecycleEvent::ReceiverReplaced);
                }
            },
            || {
                recover_engine_after_lag(
                    &mut engine,
                    &readiness,
                    {
                        let events = events.clone();
                        move || async move {
                            events
                                .lock()
                                .expect("lifecycle event lock is available")
                                .push(LifecycleEvent::Reconcile);
                            Ok(())
                        }
                    },
                    {
                        let events = events.clone();
                        move || {
                            events
                                .lock()
                                .expect("lifecycle event lock is available")
                                .push(LifecycleEvent::Spawn(1));
                            Ok(lifecycle_engine(1, events.clone()))
                        }
                    },
                    {
                        let events = events.clone();
                        move || {
                            events
                                .lock()
                                .expect("lifecycle event lock is available")
                                .push(LifecycleEvent::ReadPersistedHead);
                            Ok(88)
                        }
                    },
                )
            },
        )
        .await
        .expect("empty-slot lag recovery succeeds");

        assert!(readiness.is_ready());
        assert!(engine.is_some());
        assert_eq!(
            *events.lock().expect("lifecycle event lock is available"),
            vec![
                LifecycleEvent::ReceiverReplaced,
                LifecycleEvent::Reconcile,
                LifecycleEvent::Spawn(1),
                LifecycleEvent::ReadPersistedHead,
                LifecycleEvent::Sync(1, 88),
            ]
        );
    }

    #[tokio::test]
    async fn periodic_poll_syncs_to_persisted_executed_head() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = Some(lifecycle_engine(1, events.clone()));
        let readiness = ready_flag();

        sync_engine_to_persisted_head(&mut engine, &readiness, || Ok(23))
            .await
            .expect("periodic persisted-head sync succeeds");

        assert_eq!(
            *events.lock().expect("lifecycle event lock is available"),
            vec![LifecycleEvent::Sync(1, 23)]
        );
        assert!(readiness.is_ready());
        assert!(engine.is_some());
    }

    #[tokio::test]
    async fn periodic_poll_does_not_use_in_memory_canonical_tip() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = Some(lifecycle_engine(1, events.clone()));
        let readiness = ready_flag();
        let in_memory_canonical_tip = 99;
        let persisted_executed_head = 31;

        sync_engine_to_persisted_head(&mut engine, &readiness, || Ok(persisted_executed_head))
            .await
            .expect("periodic persisted-head sync succeeds");

        assert_ne!(persisted_executed_head, in_memory_canonical_tip);
        assert_eq!(
            *events.lock().expect("lifecycle event lock is available"),
            vec![LifecycleEvent::Sync(1, persisted_executed_head)]
        );
    }

    #[tokio::test]
    async fn periodic_head_read_failure_keeps_live_engine_and_readiness() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = Some(lifecycle_engine(1, events.clone()));
        let readiness = ready_flag();

        let error = sync_engine_to_persisted_head(&mut engine, &readiness, || {
            Err(eyre::eyre!("transient persisted-head read failure"))
        })
        .await
        .expect_err("persisted-head read fails");

        assert!(error.to_string().contains("transient persisted-head"));
        assert!(readiness.is_ready());
        assert!(engine.is_some());
        assert!(events.lock().expect("lifecycle event lock is available").is_empty());
    }

    #[tokio::test]
    async fn periodic_sync_channel_failure_clears_readiness_and_drops_generation() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> =
            Some(lifecycle_engine_with_sync_failure(1, events.clone()));
        let readiness = ready_flag();

        let error = sync_engine_to_persisted_head(&mut engine, &readiness, || Ok(35))
            .await
            .expect_err("engine sync channel fails");

        assert!(error.to_string().contains("injected lifecycle sync failure"));
        assert!(!readiness.is_ready());
        assert!(engine.is_none());
        assert_eq!(
            *events.lock().expect("lifecycle event lock is available"),
            vec![LifecycleEvent::Sync(1, 35), LifecycleEvent::Drop(1)]
        );
    }

    #[tokio::test]
    async fn startup_engine_factory_failure_leaves_slot_empty() {
        let mut engine: EngineSlot<Block> = None;
        let readiness = ProofHistoryReadiness::new();

        let error = install_engine_generation(
            &mut engine,
            &readiness,
            || Err(eyre::eyre!("injected startup factory failure")),
            || unreachable!("head must not be read after failed startup factory"),
        )
        .await
        .expect_err("startup factory fails");

        assert!(error.to_string().contains("injected startup factory failure"));
        assert!(!readiness.is_ready());
        assert!(engine.is_none());
    }

    #[tokio::test]
    async fn startup_initial_sync_failure_leaves_slot_empty() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = None;
        let readiness = ProofHistoryReadiness::new();

        let error = install_engine_generation(
            &mut engine,
            &readiness,
            {
                let events = events.clone();
                move || Ok(lifecycle_engine_with_sync_failure(1, events))
            },
            || Ok(41),
        )
        .await
        .expect_err("startup initial sync fails");

        assert!(error.to_string().contains("injected lifecycle sync failure"));
        assert!(!readiness.is_ready());
        assert!(engine.is_none());
        assert_eq!(
            *events.lock().expect("lifecycle event lock is available"),
            vec![LifecycleEvent::Sync(1, 41), LifecycleEvent::Drop(1)]
        );
    }

    #[tokio::test]
    async fn lag_recovery_spawns_a_new_engine_generation() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = Some(lifecycle_engine(1, events.clone()));
        let readiness = ready_flag();

        recover_engine_after_lag(
            &mut engine,
            &readiness,
            || async { Ok(()) },
            {
                let events = events.clone();
                move || {
                    events
                        .lock()
                        .expect("lifecycle event lock is available")
                        .push(LifecycleEvent::Spawn(2));
                    Ok(lifecycle_engine(2, events.clone()))
                }
            },
            || Ok(8),
        )
        .await
        .expect("lag recovery succeeds");

        assert!(engine.is_some());
        assert!(
            events
                .lock()
                .expect("lifecycle event lock is available")
                .contains(&LifecycleEvent::Spawn(2))
        );
    }

    #[tokio::test]
    async fn lag_recovery_syncs_new_engine_to_persisted_head() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = Some(lifecycle_engine(1, events.clone()));
        let readiness = ready_flag();

        recover_engine_after_lag(
            &mut engine,
            &readiness,
            || async { Ok(()) },
            {
                let events = events.clone();
                move || Ok(lifecycle_engine(2, events))
            },
            || Ok(44),
        )
        .await
        .expect("lag recovery succeeds");

        assert!(
            events
                .lock()
                .expect("lifecycle event lock is available")
                .contains(&LifecycleEvent::Sync(2, 44))
        );
    }

    #[tokio::test]
    async fn lag_recovery_restores_readiness_only_after_reconciliation_and_sync() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = Some(lifecycle_engine(1, events.clone()));
        let readiness = ready_flag();

        recover_engine_after_lag(
            &mut engine,
            &readiness,
            {
                let readiness = readiness.clone();
                move || async move {
                    assert!(!readiness.is_ready());
                    Ok(())
                }
            },
            {
                let events = events.clone();
                move || Ok(lifecycle_engine(2, events))
            },
            || Ok(51),
        )
        .await
        .expect("lag recovery succeeds");

        assert!(readiness.is_ready());
    }

    #[tokio::test]
    async fn lag_reconciliation_failure_leaves_readiness_false_and_no_engine() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = Some(lifecycle_engine(1, events));
        let readiness = ready_flag();

        let error = recover_engine_after_lag(
            &mut engine,
            &readiness,
            || async { Err(eyre::eyre!("injected reconciliation failure")) },
            || unreachable!("engine must not spawn after failed reconciliation"),
            || unreachable!("head must not be read after failed reconciliation"),
        )
        .await
        .expect_err("lag reconciliation fails");

        assert!(error.to_string().contains("injected reconciliation failure"));
        assert!(!readiness.is_ready());
        assert!(engine.is_none());
    }

    #[tokio::test]
    async fn lag_engine_spawn_failure_leaves_readiness_false() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = Some(lifecycle_engine(1, events));
        let readiness = ready_flag();

        let error = recover_engine_after_lag(
            &mut engine,
            &readiness,
            || async { Ok(()) },
            || Err(eyre::eyre!("injected engine spawn failure")),
            || unreachable!("head must not be read after failed spawn"),
        )
        .await
        .expect_err("engine spawn fails");

        assert!(error.to_string().contains("injected engine spawn failure"));
        assert!(!readiness.is_ready());
        assert!(engine.is_none());
    }

    #[tokio::test]
    async fn lag_initial_sync_failure_leaves_readiness_false() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = Some(lifecycle_engine(1, events.clone()));
        let readiness = ready_flag();

        let error = recover_engine_after_lag(
            &mut engine,
            &readiness,
            || async { Ok(()) },
            {
                let events = events.clone();
                move || Ok(lifecycle_engine_with_sync_failure(2, events))
            },
            || Ok(61),
        )
        .await
        .expect_err("initial sync fails");

        assert!(error.to_string().contains("injected lifecycle sync failure"));
        assert!(!readiness.is_ready());
        assert!(engine.is_none());
        assert!(
            events
                .lock()
                .expect("lifecycle event lock is available")
                .contains(&LifecycleEvent::Drop(2))
        );
    }

    #[tokio::test]
    async fn shutdown_drops_the_sole_engine_handle() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = Some(lifecycle_engine(7, events.clone()));
        let readiness = ready_flag();

        shutdown_engine(&mut engine, &readiness).await.expect("engine shuts down");

        assert_eq!(
            *events.lock().expect("lifecycle event lock is available"),
            vec![LifecycleEvent::Drop(7)]
        );
        assert!(!readiness.is_ready());
        assert!(engine.is_none());
    }

    #[tokio::test]
    async fn closed_notification_channel_clears_readiness_and_drops_engine() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = Some(lifecycle_engine(9, events.clone()));
        let readiness = ready_flag();

        shutdown_engine(&mut engine, &readiness).await.expect("closed channel shuts engine down");

        assert!(!readiness.is_ready());
        assert!(engine.is_none());
        assert_eq!(
            *events.lock().expect("lifecycle event lock is available"),
            vec![LifecycleEvent::Drop(9)]
        );
    }

    #[test]
    fn external_finality_pruner_spawns_only_once_across_generations() {
        let mut spawned = false;
        let calls = Cell::new(0);

        spawn_pruner_once(&mut spawned, || calls.set(calls.get() + 1));
        spawn_pruner_once(&mut spawned, || calls.set(calls.get() + 1));

        assert!(spawned);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn persisted_head_below_retained_earliest_fails_closed() {
        let error = proof_history_live_action(
            proof_window((10, 0x10), (15, 0x15)),
            9,
            None,
            None,
            None,
            storage_path(),
        )
        .expect_err("deep reorg below retained anchor fails closed");

        let message = error.to_string();
        assert!(message.contains("deep reorg"));
        assert!(message.contains("/configured/proof-history"));
        assert!(message.contains("wipe"));
    }

    #[test]
    fn one_height_window_with_conflicting_endpoint_hashes_fails_closed() {
        let error = proof_history_live_action(
            proof_window((10, 0x10), (10, 0x11)),
            10,
            Some(hash(0x10)),
            Some(hash(0x11)),
            None,
            storage_path(),
        )
        .expect_err("conflicting same-height endpoints fail closed");

        let message = error.to_string();
        assert!(message.contains("conflicting hashes"));
        assert!(message.contains("wipe"));
    }

    #[test]
    fn noncanonical_live_latest_rolls_entire_suffix_back_to_validated_anchor() {
        let child = unwind_marker((10, 0x10), 0x11);
        let action = proof_history_live_action(
            proof_window((10, 0x10), (15, 0x15)),
            15,
            Some(hash(0x10)),
            Some(hash(0x99)),
            Some(child),
            storage_path(),
        )
        .expect("validated earliest permits noncanonical suffix rollback");

        assert_eq!(action, ProofHistoryLiveAction::Unwind { first_removed: child });
    }

    #[test]
    fn one_block_live_anchor_is_ready_when_its_identity_is_canonical() {
        let action = proof_history_live_action(
            proof_window((10, 0x10), (10, 0x10)),
            10,
            Some(hash(0x10)),
            Some(hash(0x10)),
            None,
            storage_path(),
        )
        .expect("canonical one-block anchor is valid");

        assert_eq!(action, ProofHistoryLiveAction::Ready);
    }

    #[test]
    fn ahead_of_persisted_head_rolls_back_to_retained_anchor() {
        let earliest = BlockNumHash::new(10, hash(0x10));
        let action = proof_history_live_action(
            proof_window((10, 0x10), (15, 0x15)),
            10,
            Some(earliest.hash),
            None,
            None,
            storage_path(),
        )
        .expect("canonical retained anchor permits suffix rollback");

        assert_eq!(
            action,
            ProofHistoryLiveAction::Unwind {
                first_removed: BlockWithParent::new(
                    earliest.hash,
                    BlockNumHash::new(11, B256::ZERO),
                ),
            }
        );
    }

    #[test]
    fn atomic_live_reconciliation_commits_ahead_suffix_rollback_to_anchor() {
        let fixture = LiveReconcileFixture::ahead_of_persisted_head(10, 15);
        let retained_anchor = fixture.canonical[10];

        let action =
            reconcile_live_storage_atomic(&fixture.storage, &fixture.factory, &fixture.proof_path)
                .expect("atomic live reconciliation succeeds");

        assert_eq!(
            action,
            ProofHistoryLiveAction::Unwind {
                first_removed: BlockWithParent::new(
                    retained_anchor.hash,
                    BlockNumHash::new(11, B256::ZERO),
                ),
            }
        );
        let window = fixture
            .storage
            .provider_ro()
            .expect("proof reader opens")
            .get_proof_window()
            .expect("proof window exists");
        assert_eq!(window.earliest, retained_anchor);
        assert_eq!(window.latest, retained_anchor);
    }

    #[test]
    fn commit_indexes_precomputed_notification_data() {
        let parent = hash(1);
        let block = test_block(11, parent, 11);
        let chain = test_chain(vec![block.clone()], &[11]);
        let engine = RecordingEngine::default();
        let readiness = ready_flag();

        route_chain_commit(
            &chain,
            canonical_commit_snapshot(BlockNumHash::new(10, parent), &chain),
            0,
            &engine,
            &readiness,
        )
        .expect("contiguous precomputed commit is indexed");

        assert_eq!(engine.calls(), vec![EngineCall::Index(block.block_with_parent())]);
        assert!(readiness.is_ready());
    }

    #[test]
    fn stale_buffered_commit_is_ignored_while_proof_head_is_canonical() {
        let committed_latest = BlockNumHash::new(10, hash(10));
        let stale = linear_chain(11, committed_latest.hash, &[11], &[11]);
        let engine = RecordingEngine::default();
        let readiness = ready_flag();

        route_chain_commit(
            &stale,
            commit_snapshot(committed_latest, Some(committed_latest.hash), Some(hash(99))),
            0,
            &engine,
            &readiness,
        )
        .expect("stale commit is harmless while committed proof history remains canonical");

        assert!(engine.calls().is_empty());
        assert!(readiness.is_ready());
    }

    #[test]
    fn covered_commit_with_noncanonical_proof_head_fails_closed() {
        let chain = linear_chain(10, hash(9), &[10, 11], &[10, 11]);
        let engine = RecordingEngine::default();
        let readiness = ready_flag();
        let committed_latest = BlockNumHash::new(12, hash(12));

        let _error = route_chain_commit(
            &chain,
            commit_snapshot(committed_latest, Some(hash(77)), Some(chain.tip().hash())),
            0,
            &engine,
            &readiness,
        )
        .expect_err("covered commit cannot hide a noncanonical committed proof head");

        assert!(engine.calls().is_empty());
        assert!(!readiness.is_ready());
    }

    #[test]
    fn current_canonical_commit_is_applied() {
        let committed_latest = BlockNumHash::new(10, hash(10));
        let current = linear_chain(11, committed_latest.hash, &[11], &[11]);
        let engine = RecordingEngine::default();

        route_chain_commit(
            &current,
            commit_snapshot(
                committed_latest,
                Some(committed_latest.hash),
                Some(current.tip().hash()),
            ),
            0,
            &engine,
            &ready_flag(),
        )
        .expect("exact canonical commit is applied");

        assert_eq!(engine.calls(), vec![EngineCall::Index(current.tip().block_with_parent())]);
    }

    #[test]
    fn commit_executes_at_verification_height() {
        let parent = hash(1);
        let block = test_block(12, parent, 12);
        let chain = test_chain(vec![block.clone()], &[12]);
        let engine = RecordingEngine::default();

        route_chain_commit(
            &chain,
            canonical_commit_snapshot(BlockNumHash::new(11, parent), &chain),
            3,
            &engine,
            &ready_flag(),
        )
        .expect("verification height is executed");

        assert_eq!(engine.calls(), vec![EngineCall::Execute(BlockNumHash::new(12, block.hash()))]);
    }

    #[test]
    fn commit_executes_when_trie_data_is_missing() {
        let parent = hash(1);
        let block = test_block(11, parent, 13);
        let chain = test_chain(vec![block.clone()], &[]);
        let engine = RecordingEngine::default();

        route_chain_commit(
            &chain,
            canonical_commit_snapshot(BlockNumHash::new(10, parent), &chain),
            0,
            &engine,
            &ready_flag(),
        )
        .expect("missing trie data falls back to exact execution");

        assert_eq!(engine.calls(), vec![EngineCall::Execute(BlockNumHash::new(11, block.hash()))]);
    }

    #[test]
    fn commit_execution_uses_exact_notification_block() {
        let parent = hash(1);
        let notification_block = test_block(11, parent, 14);
        let same_height_other_fork = test_block(11, parent, 15);
        let chain = test_chain(vec![notification_block.clone()], &[]);
        let engine = RecordingEngine::default();

        route_chain_commit(
            &chain,
            canonical_commit_snapshot(BlockNumHash::new(10, parent), &chain),
            0,
            &engine,
            &ready_flag(),
        )
        .expect("notification block executes");

        assert_ne!(notification_block.hash(), same_height_other_fork.hash());
        assert_eq!(
            engine.calls(),
            vec![EngineCall::Execute(BlockNumHash::new(11, notification_block.hash()))]
        );
    }

    #[test]
    fn overlapping_commit_processes_only_uncovered_suffix() {
        let chain = linear_chain(10, hash(9), &[10, 11, 12], &[10, 11, 12]);
        let stored = chain.blocks().get(&11).expect("stored block exists");
        let uncovered = chain.blocks().get(&12).expect("uncovered block exists");
        let engine = RecordingEngine::default();

        route_chain_commit(
            &chain,
            canonical_commit_snapshot(BlockNumHash::new(11, stored.hash()), &chain),
            0,
            &engine,
            &ready_flag(),
        )
        .expect("overlap processes only its suffix");

        assert_eq!(engine.calls(), vec![EngineCall::Index(uncovered.block_with_parent())]);
    }

    #[test]
    fn duplicate_commit_is_a_noop() {
        let chain = linear_chain(10, hash(9), &[10, 11], &[10, 11]);
        let tip = chain.tip();
        let engine = RecordingEngine::default();

        route_chain_commit(
            &chain,
            canonical_commit_snapshot(BlockNumHash::new(tip.number(), tip.hash()), &chain),
            0,
            &engine,
            &ready_flag(),
        )
        .expect("covered commit is ignored");

        assert!(engine.calls().is_empty());
    }

    #[test]
    fn gapped_commit_requests_engine_sync() {
        let chain = linear_chain(12, hash(11), &[12, 13], &[12, 13]);
        let engine = RecordingEngine::default();

        route_chain_commit(
            &chain,
            canonical_commit_snapshot(BlockNumHash::new(10, hash(10)), &chain),
            0,
            &engine,
            &ready_flag(),
        )
        .expect("gap is delegated to engine sync");

        assert_eq!(engine.calls(), vec![EngineCall::Sync(13)]);
    }

    #[test]
    fn gapped_commit_sync_failure_clears_readiness() {
        let chain = linear_chain(12, hash(11), &[12], &[12]);
        let engine = RecordingEngine::failing(EngineMethod::Sync);
        let readiness = ready_flag();

        let _error = route_chain_commit(
            &chain,
            canonical_commit_snapshot(BlockNumHash::new(10, hash(10)), &chain),
            0,
            &engine,
            &readiness,
        )
        .expect_err("sync failure propagates");

        assert!(!readiness.is_ready());
    }

    #[test]
    fn internally_gapped_commit_requests_engine_sync_before_indexing() {
        let parent = hash(10);
        let first = test_block(11, parent, 11);
        let third = test_block(13, first.hash(), 13);
        let chain = test_chain(vec![first, third], &[11, 13]);
        let engine = RecordingEngine::default();

        route_chain_commit(
            &chain,
            canonical_commit_snapshot(BlockNumHash::new(10, parent), &chain),
            0,
            &engine,
            &ready_flag(),
        )
        .expect("internal notification gap delegates the full suffix to sync");

        assert_eq!(engine.calls(), vec![EngineCall::Sync(13)]);
    }

    #[test]
    fn precomputed_reorg_uses_single_engine_reorg() {
        let parent = hash(10);
        let old = linear_chain(11, parent, &[21, 22], &[]);
        let new = linear_chain(11, parent, &[31, 32], &[11, 12]);
        let engine = RecordingEngine::default();
        let readiness = ready_flag();

        route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                BlockNumHash::new(10, parent),
                BlockNumHash::new(12, old.tip().hash()),
                Some(new.tip().hash()),
                Some(new.tip().hash()),
            ),
            0,
            &engine,
            &readiness,
        )
        .expect("fully precomputed replacement uses direct reorg");

        assert_eq!(
            engine.calls(),
            vec![EngineCall::Reorg(
                new.blocks().values().map(|block| block.block_with_parent()).collect()
            )]
        );
        assert!(!readiness.is_ready(), "reconciliation must run before readiness is restored");
    }

    #[test]
    fn verification_height_reorg_unwinds_then_replays_in_order() {
        let parent = hash(10);
        let old = linear_chain(11, parent, &[21, 22], &[]);
        let new = linear_chain(11, parent, &[31, 32], &[11, 12]);
        let engine = RecordingEngine::default();

        route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                BlockNumHash::new(10, parent),
                BlockNumHash::new(12, old.tip().hash()),
                Some(new.tip().hash()),
                Some(new.tip().hash()),
            ),
            2,
            &engine,
            &ready_flag(),
        )
        .expect("verification height forces unwind and replay");

        assert_eq!(
            engine.calls(),
            vec![
                EngineCall::Unwind(old.first().block_with_parent()),
                EngineCall::Index(new.first().block_with_parent()),
                EngineCall::Execute(BlockNumHash::new(12, new.tip().hash())),
            ]
        );
    }

    #[test]
    fn missing_trie_data_reorg_unwinds_then_replays_in_order() {
        let parent = hash(10);
        let old = linear_chain(11, parent, &[21, 22], &[]);
        let new = linear_chain(11, parent, &[31, 32], &[11]);
        let engine = RecordingEngine::default();

        route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                BlockNumHash::new(10, parent),
                BlockNumHash::new(12, old.tip().hash()),
                Some(new.tip().hash()),
                Some(new.tip().hash()),
            ),
            0,
            &engine,
            &ready_flag(),
        )
        .expect("missing data forces unwind and replay");

        assert_eq!(
            engine.calls(),
            vec![
                EngineCall::Unwind(old.first().block_with_parent()),
                EngineCall::Index(new.first().block_with_parent()),
                EngineCall::Execute(BlockNumHash::new(12, new.tip().hash())),
            ]
        );
    }

    #[test]
    fn reorg_replay_uses_exact_notification_blocks() {
        let parent = hash(10);
        let old = linear_chain(11, parent, &[21], &[]);
        let new = linear_chain(11, parent, &[31], &[]);
        let same_height_other_fork = test_block(11, parent, 41);
        let engine = RecordingEngine::default();

        route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                BlockNumHash::new(10, parent),
                BlockNumHash::new(11, old.tip().hash()),
                Some(new.tip().hash()),
                Some(new.tip().hash()),
            ),
            0,
            &engine,
            &ready_flag(),
        )
        .expect("replacement replays exact notification block");

        assert_ne!(new.tip().hash(), same_height_other_fork.hash());
        assert_eq!(
            engine.calls(),
            vec![
                EngineCall::Unwind(old.first().block_with_parent()),
                EngineCall::Execute(BlockNumHash::new(11, new.tip().hash())),
            ]
        );
    }

    #[test]
    fn revert_above_earliest_unwinds() {
        let parent = hash(10);
        let old = linear_chain(11, parent, &[21, 22], &[]);
        let engine = RecordingEngine::default();
        let readiness = ready_flag();

        route_chain_revert(&old, BlockNumHash::new(10, parent), &engine, &readiness)
            .expect("safe retained-range revert unwinds");

        assert_eq!(engine.calls(), vec![EngineCall::Unwind(old.first().block_with_parent())]);
        assert!(!readiness.is_ready());
    }

    #[tokio::test]
    async fn successful_reorg_reconciles_before_becoming_ready() {
        let readiness = ready_flag();
        readiness.set_not_ready();
        let reconciliation_observed_not_ready = Cell::new(false);

        let outcome = reconcile_installed_engine(
            ProofHistoryStartupAction::Ready,
            &readiness,
            || {
                reconciliation_observed_not_ready.set(!readiness.is_ready());
                Ok(())
            },
            || unreachable!("ready reconciliation must not replace the receiver"),
            || async { unreachable!("ready reconciliation must not rebuild the engine") },
        )
        .await
        .expect("successful reconciliation restores readiness");

        assert!(reconciliation_observed_not_ready.get());
        assert!(readiness.is_ready());
        assert_eq!(outcome, ReconciliationOutcome::Ready);
    }

    #[tokio::test]
    async fn reconciliation_failure_leaves_readiness_false() {
        let readiness = ready_flag();
        readiness.set_not_ready();

        let _error = reconcile_installed_engine(
            ProofHistoryStartupAction::Ready,
            &readiness,
            || Err(eyre::eyre!("injected reconciliation failure")),
            || unreachable!("failed ready reconciliation must not replace the receiver"),
            || async { unreachable!("failed ready reconciliation must not rebuild the engine") },
        )
        .await
        .expect_err("reconciliation failure propagates");

        assert!(!readiness.is_ready());
    }

    #[tokio::test]
    async fn ahead_proof_latest_waits_without_error_drop_or_readiness() {
        let readiness = ready_flag();
        readiness.set_not_ready();
        let events = Arc::new(Mutex::new(Vec::new()));
        let engine: EngineSlot<Block> = Some(lifecycle_engine(1, events.clone()));

        let startup_action = proof_history_startup_action(
            Some(proof_window((10, 10), (25, 25))),
            20,
            Some(hash(10)),
            None,
            Some(unwind_marker((10, 10), 11)),
            storage_path(),
        )
        .expect("persisted canonical state behind the proof head must classify as a wait");
        let outcome = reconcile_installed_engine(
            startup_action,
            &readiness,
            || unreachable!("waiting must not perform ready validation"),
            || unreachable!("waiting must not replace the receiver"),
            || async { unreachable!("waiting must not drop or rebuild the engine") },
        )
        .await
        .expect("persistence lag is a retryable reconciliation wait");

        assert!(!readiness.is_ready());
        assert!(engine.is_some());
        assert!(events.lock().expect("lifecycle event lock is available").is_empty());
        assert_eq!(outcome, ReconciliationOutcome::WaitingForPersistence);
        assert_eq!(ReconciliationState::from(outcome), ReconciliationState::Pending);
    }

    #[tokio::test]
    async fn same_branch_persistence_catch_up_restores_readiness_without_rebuild() {
        let readiness = ready_flag();
        readiness.set_not_ready();
        let events = Arc::new(Mutex::new(Vec::new()));
        let engine: EngineSlot<Block> = Some(lifecycle_engine(1, events.clone()));

        let waiting = reconcile_installed_engine(
            ProofHistoryStartupAction::WaitForCanonicalLatest { latest: 25, canonical_best: 20 },
            &readiness,
            || unreachable!("waiting must not perform ready validation"),
            || unreachable!("waiting must not replace the receiver"),
            || async { unreachable!("waiting must not rebuild the engine") },
        )
        .await
        .expect("the first persisted snapshot remains behind");
        assert_eq!(ReconciliationState::from(waiting), ReconciliationState::Pending);

        let ready = reconcile_installed_engine(
            ProofHistoryStartupAction::Ready,
            &readiness,
            || Ok(()),
            || unreachable!("same-branch catch-up must retain the receiver"),
            || async { unreachable!("same-branch catch-up must retain the engine") },
        )
        .await
        .expect("a later same-branch snapshot reconciles");

        assert_eq!(ready, ReconciliationOutcome::Ready);
        assert_eq!(ReconciliationState::from(ready), ReconciliationState::Reconciled);
        assert!(readiness.is_ready());
        assert!(engine.is_some());
        assert!(events.lock().expect("lifecycle event lock is available").is_empty());
    }

    #[tokio::test]
    async fn conflicting_persistence_catch_up_replaces_receiver_before_live_recovery() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut engine: EngineSlot<Block> = Some(lifecycle_engine(1, events.clone()));
        let readiness = ready_flag();
        readiness.set_not_ready();

        let outcome = reconcile_installed_engine(
            ProofHistoryStartupAction::Unwind { first_removed: unwind_marker((10, 10), 11) },
            &readiness,
            || unreachable!("divergence must not use startup ready validation"),
            {
                let events = events.clone();
                move || {
                    events
                        .lock()
                        .expect("lifecycle event lock is available")
                        .push(LifecycleEvent::ReceiverReplaced);
                }
            },
            || {
                recover_engine_after_lag(
                    &mut engine,
                    &readiness,
                    {
                        let events = events.clone();
                        move || async move {
                            events
                                .lock()
                                .expect("lifecycle event lock is available")
                                .push(LifecycleEvent::Reconcile);
                            Ok(())
                        }
                    },
                    {
                        let events = events.clone();
                        move || {
                            events
                                .lock()
                                .expect("lifecycle event lock is available")
                                .push(LifecycleEvent::Spawn(2));
                            Ok(lifecycle_engine(2, events.clone()))
                        }
                    },
                    {
                        let events = events.clone();
                        move || {
                            events
                                .lock()
                                .expect("lifecycle event lock is available")
                                .push(LifecycleEvent::ReadPersistedHead);
                            Ok(25)
                        }
                    },
                )
            },
        )
        .await
        .expect("conflicting catch-up uses live recovery");

        assert_eq!(outcome, ReconciliationOutcome::Recovered);
        assert_eq!(ReconciliationState::from(outcome), ReconciliationState::Reconciled);
        assert!(readiness.is_ready());
        assert!(engine.is_some());
        assert_eq!(
            *events.lock().expect("lifecycle event lock is available"),
            vec![
                LifecycleEvent::ReceiverReplaced,
                LifecycleEvent::Drop(1),
                LifecycleEvent::Reconcile,
                LifecycleEvent::Spawn(2),
                LifecycleEvent::ReadPersistedHead,
                LifecycleEvent::Sync(2, 25),
            ]
        );
    }

    #[test]
    fn pending_reconciliation_buffers_notifications_and_exposes_lag_after_resume() {
        use tokio::sync::broadcast::error::TryRecvError;

        let (notifications, mut receiver) = tokio::sync::broadcast::channel(1);
        let state = ReconciliationState::Pending;
        assert!(!state.should_receive_notifications());

        notifications.send(1_u64).expect("first notification buffers");
        notifications.send(2_u64).expect("second notification buffers");
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Lagged(1))));

        let state = ReconciliationState::Reconciled;
        assert!(state.should_receive_notifications());
        assert_eq!(receiver.try_recv().expect("latest buffered notification remains"), 2);
    }

    #[test]
    fn reorg_snapshot_failure_clears_readiness_before_routing() {
        let parent = hash(10);
        let notification = CanonStateNotification::Reorg {
            old: Arc::new(linear_chain(11, parent, &[21], &[])),
            new: Arc::new(linear_chain(11, parent, &[31], &[11])),
        };
        let readiness = ready_flag();

        prepare_notification_readiness(&notification, &readiness);
        let _error = read_notification_routing_snapshot(&readiness, || {
            assert!(!readiness.is_ready(), "reorg clears readiness before snapshot I/O");
            Err::<(), _>(eyre::eyre!("injected pinned snapshot failure"))
        })
        .expect_err("snapshot error propagates");

        assert!(!readiness.is_ready());
    }

    #[test]
    fn reorg_rejects_empty_old_chain_without_engine_calls() {
        let old = Chain::<EthPrimitives>::default();
        let new = linear_chain(11, hash(10), &[31], &[11]);
        let engine = RecordingEngine::default();
        let readiness = ready_flag();

        let _error = route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                BlockNumHash::new(10, hash(10)),
                BlockNumHash::new(10, hash(10)),
                Some(hash(10)),
                Some(new.tip().hash()),
            ),
            0,
            &engine,
            &readiness,
        )
        .expect_err("empty old branch is malformed");

        assert!(engine.calls().is_empty());
        assert!(!readiness.is_ready());
    }

    #[test]
    fn reorg_rejects_mismatched_fork_blocks() {
        let old = linear_chain(11, hash(10), &[21], &[]);
        let new = linear_chain(11, hash(9), &[31], &[11]);
        let engine = RecordingEngine::default();
        let readiness = ready_flag();

        let error = route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                BlockNumHash::new(10, hash(10)),
                BlockNumHash::new(11, old.tip().hash()),
                Some(new.tip().hash()),
                Some(new.tip().hash()),
            ),
            0,
            &engine,
            &readiness,
        )
        .expect_err("different fork blocks must fail before engine mutation");

        assert!(error.to_string().contains("fork blocks do not match"));
        assert!(engine.calls().is_empty());
        assert!(!readiness.is_ready());
    }

    #[test]
    fn reorg_rejects_non_contiguous_replacement() {
        let parent = hash(10);
        let old = linear_chain(11, parent, &[21, 22, 23], &[]);
        let first = test_block(11, parent, 31);
        let third = test_block(13, first.hash(), 33);
        let new = test_chain(vec![first, third], &[11, 13]);
        let engine = RecordingEngine::default();
        let readiness = ready_flag();

        let error = route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                BlockNumHash::new(10, parent),
                BlockNumHash::new(13, old.tip().hash()),
                Some(new.tip().hash()),
                Some(new.tip().hash()),
            ),
            0,
            &engine,
            &readiness,
        )
        .expect_err("replacement number gap must fail before engine mutation");

        assert!(error.to_string().contains("not contiguous"));
        assert!(engine.calls().is_empty());
        assert!(!readiness.is_ready());
    }

    #[test]
    fn reorg_rejects_broken_replacement_parent_link() {
        let parent = hash(10);
        let old = linear_chain(11, parent, &[21, 22], &[]);
        let first = test_block(11, parent, 31);
        let second = test_block(12, hash(99), 32);
        let new = test_chain(vec![first, second], &[11, 12]);
        let engine = RecordingEngine::default();
        let readiness = ready_flag();

        let error = route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                BlockNumHash::new(10, parent),
                BlockNumHash::new(12, old.tip().hash()),
                Some(new.tip().hash()),
                Some(new.tip().hash()),
            ),
            0,
            &engine,
            &readiness,
        )
        .expect_err("broken parent link must fail before engine mutation");

        assert!(error.to_string().contains("has parent"));
        assert!(engine.calls().is_empty());
        assert!(!readiness.is_ready());
    }

    #[test]
    fn reorg_with_common_ancestor_at_earliest_is_allowed() {
        let earliest = BlockNumHash::new(10, hash(10));
        let old = linear_chain(11, earliest.hash, &[21], &[]);
        let new = linear_chain(11, earliest.hash, &[31], &[11]);
        let engine = RecordingEngine::default();

        route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                earliest,
                BlockNumHash::new(11, old.tip().hash()),
                Some(new.tip().hash()),
                Some(new.tip().hash()),
            ),
            0,
            &engine,
            &ready_flag(),
        )
        .expect("reorg retaining the earliest anchor is recoverable");

        assert_eq!(engine.calls().len(), 1);
    }

    #[test]
    fn boundary_reorg_must_descend_from_retained_earliest_hash() {
        let earliest = BlockNumHash::new(10, hash(10));
        let wrong_parent = hash(99);
        let old = linear_chain(11, wrong_parent, &[21], &[]);
        let new = linear_chain(11, wrong_parent, &[31], &[11]);
        let engine = RecordingEngine::default();
        let readiness = ready_flag();

        let _error = route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                earliest,
                BlockNumHash::new(11, old.tip().hash()),
                Some(new.tip().hash()),
                Some(new.tip().hash()),
            ),
            0,
            &engine,
            &readiness,
        )
        .expect_err("boundary reorg must descend from the retained anchor identity");

        assert!(engine.calls().is_empty());
        assert!(!readiness.is_ready());
    }

    #[test]
    fn boundary_revert_must_descend_from_retained_earliest_hash() {
        let earliest = BlockNumHash::new(10, hash(10));
        let old = linear_chain(11, hash(99), &[21], &[]);
        let engine = RecordingEngine::default();
        let readiness = ready_flag();

        let _error = route_chain_revert(&old, earliest, &engine, &readiness)
            .expect_err("boundary revert must descend from the retained anchor identity");

        assert!(engine.calls().is_empty());
        assert!(!readiness.is_ready());
    }

    #[test]
    fn reorg_replacing_earliest_fails_closed() {
        let earliest = BlockNumHash::new(10, hash(10));
        let old = linear_chain(10, hash(9), &[21], &[]);
        let new = linear_chain(10, hash(9), &[31], &[10]);
        let engine = RecordingEngine::default();
        let readiness = ready_flag();

        let _error = route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                earliest,
                BlockNumHash::new(10, old.tip().hash()),
                Some(new.tip().hash()),
                Some(new.tip().hash()),
            ),
            0,
            &engine,
            &readiness,
        )
        .expect_err("replacing retained earliest must fail closed");

        assert!(engine.calls().is_empty());
        assert!(!readiness.is_ready());
    }

    #[test]
    fn revert_replacing_earliest_fails_closed() {
        let earliest = BlockNumHash::new(10, hash(10));
        let old = linear_chain(10, hash(9), &[21], &[]);
        let engine = RecordingEngine::default();
        let readiness = ready_flag();

        let _error = route_chain_revert(&old, earliest, &engine, &readiness)
            .expect_err("reverting retained earliest must fail closed");

        assert!(engine.calls().is_empty());
        assert!(!readiness.is_ready());
    }

    #[test]
    fn engine_failure_after_unwind_leaves_readiness_false() {
        let parent = hash(10);
        let old = linear_chain(11, parent, &[21], &[]);
        let new = linear_chain(11, parent, &[31], &[]);
        let engine = RecordingEngine::failing(EngineMethod::Execute);
        let readiness = ready_flag();

        let _error = route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                BlockNumHash::new(10, parent),
                BlockNumHash::new(11, old.tip().hash()),
                Some(new.tip().hash()),
                Some(new.tip().hash()),
            ),
            0,
            &engine,
            &readiness,
        )
        .expect_err("replay failure propagates");

        assert_eq!(engine.calls().len(), 2, "one unwind precedes the failed replay");
        assert!(!readiness.is_ready());
    }

    #[test]
    fn commit_engine_failure_clears_readiness() {
        let parent = hash(10);
        let new = linear_chain(11, parent, &[31], &[11]);
        let engine = RecordingEngine::failing(EngineMethod::Index);
        let readiness = ready_flag();

        let _error = route_chain_commit(
            &new,
            canonical_commit_snapshot(BlockNumHash::new(10, parent), &new),
            0,
            &engine,
            &readiness,
        )
        .expect_err("commit engine failure propagates");

        assert!(!readiness.is_ready());
    }

    #[test]
    fn duplicate_reorg_covered_by_committed_canonical_suffix_is_skipped() {
        let parent = hash(10);
        let old = linear_chain(11, parent, &[21, 22], &[]);
        let new = linear_chain(11, parent, &[31, 32], &[11, 12]);
        let committed_latest = BlockNumHash::new(13, hash(13));
        let engine = RecordingEngine::default();

        route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                BlockNumHash::new(10, parent),
                committed_latest,
                Some(committed_latest.hash),
                Some(new.tip().hash()),
            ),
            0,
            &engine,
            &ready_flag(),
        )
        .expect("canonical committed suffix consumes duplicate reorg");

        assert!(engine.calls().is_empty());
    }

    #[test]
    fn duplicate_reorg_at_committed_common_ancestor_is_forwarded() {
        let common = BlockNumHash::new(10, hash(10));
        let old = linear_chain(11, common.hash, &[21, 22], &[]);
        let new = linear_chain(11, common.hash, &[31, 32], &[11, 12]);
        let engine = RecordingEngine::default();

        route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(common, common, Some(common.hash), Some(new.tip().hash())),
            0,
            &engine,
            &ready_flag(),
        )
        .expect("common-ancestor proof head does not consume private-buffer reorg");

        assert_eq!(engine.calls().len(), 1);
    }

    #[test]
    fn same_height_old_fork_hash_does_not_skip_reorg() {
        let parent = hash(10);
        let old = linear_chain(11, parent, &[21, 22], &[]);
        let new = linear_chain(11, parent, &[31, 32], &[11, 12]);
        let committed_old_tip = BlockNumHash::new(12, old.tip().hash());
        let engine = RecordingEngine::default();

        route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                BlockNumHash::new(10, parent),
                committed_old_tip,
                Some(new.tip().hash()),
                Some(new.tip().hash()),
            ),
            0,
            &engine,
            &ready_flag(),
        )
        .expect("same-height old-fork proof head forwards replacement");

        assert_eq!(engine.calls().len(), 1);
    }

    #[test]
    fn stale_canonical_snapshot_does_not_skip_same_height_old_fork() {
        let parent = hash(10);
        let old = linear_chain(11, parent, &[21, 22], &[]);
        let new = linear_chain(11, parent, &[31, 32], &[11, 12]);
        let committed_old_tip = BlockNumHash::new(12, old.tip().hash());
        let engine = RecordingEngine::default();

        route_chain_reorg(
            &old,
            &new,
            reorg_snapshot(
                BlockNumHash::new(10, parent),
                committed_old_tip,
                Some(committed_old_tip.hash),
                Some(committed_old_tip.hash),
            ),
            0,
            &engine,
            &ready_flag(),
        )
        .expect("stale old-fork snapshot cannot consume the replacement notification");

        assert_eq!(engine.calls().len(), 1);
    }

    #[test]
    fn duplicate_revert_at_committed_common_ancestor_is_forwarded() {
        let common = BlockNumHash::new(10, hash(10));
        let old = linear_chain(11, common.hash, &[21], &[]);
        let engine = RecordingEngine::default();

        route_chain_revert(&old, common, &engine, &ready_flag())
            .expect("safe revert clears a possible private old-fork buffer");

        assert_eq!(engine.calls(), vec![EngineCall::Unwind(old.first().block_with_parent())]);
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
    fn startup_action_ready_when_both_window_endpoints_are_canonical() {
        let action = proof_history_startup_action(
            Some(proof_window((10, 10), (20, 20))),
            20,
            Some(hash(10)),
            Some(hash(20)),
            Some(unwind_marker((10, 10), 11)),
            storage_path(),
        )
        .expect("canonical endpoints should be ready");

        assert_eq!(action, ProofHistoryStartupAction::Ready);
    }

    #[test]
    fn startup_action_refuses_noncanonical_earliest() {
        let error = proof_history_startup_action(
            Some(proof_window((10, 11), (20, 20))),
            20,
            Some(hash(10)),
            Some(hash(20)),
            Some(unwind_marker((10, 11), 12)),
            storage_path(),
        )
        .expect_err("noncanonical earliest must fail even when latest is canonical");

        let message = error.to_string();
        assert!(message.contains("earliest stored block"));
        assert!(message.contains(&storage_path().display().to_string()));
        assert!(message.contains("wipe"));
        assert!(message.contains("fresh path"));
    }

    #[test]
    fn startup_action_waits_when_latest_is_above_snapshot_best() {
        let action = proof_history_startup_action(
            Some(proof_window((10, 10), (25, 25))),
            20,
            Some(hash(10)),
            None,
            Some(unwind_marker((10, 10), 11)),
            storage_path(),
        )
        .expect("latest above the persisted best should wait");

        assert_eq!(
            action,
            ProofHistoryStartupAction::WaitForCanonicalLatest { latest: 25, canonical_best: 20 }
        );
    }

    #[test]
    fn startup_action_unwinds_when_latest_mismatches() {
        let first_removed = unwind_marker((10, 10), 11);
        let action = proof_history_startup_action(
            Some(proof_window((10, 10), (20, 21))),
            20,
            Some(hash(10)),
            Some(hash(20)),
            Some(first_removed),
            storage_path(),
        )
        .expect("canonical earliest should allow a retained-window unwind");

        assert_eq!(action, ProofHistoryStartupAction::Unwind { first_removed });
    }

    #[test]
    fn startup_reconciliation_opens_one_canonical_snapshot() {
        let opens = Cell::new(0);
        let action = proof_history_startup_reconciliation(
            || Ok(Some(proof_window((10, 10), (20, 20)))),
            |_| {
                opens.set(opens.get() + 1);
                Ok(ProofHistoryCanonicalSnapshot {
                    canonical_best: 20,
                    canonical_earliest_hash: Some(hash(10)),
                    canonical_latest_hash: Some(hash(20)),
                    first_removed: Some(unwind_marker((10, 10), 11)),
                })
            },
            storage_path(),
        )
        .expect("canonical proof window should reconcile");

        assert_eq!(action, ProofHistoryStartupAction::Ready);
        assert_eq!(opens.get(), 1, "reconciliation must open one canonical snapshot");
    }

    #[test]
    fn startup_unwind_uses_child_hash_from_reconciliation_snapshot() {
        let opens = Cell::new(0);
        let first_snapshot_child = unwind_marker((10, 10), 11);
        let later_snapshot_child = unwind_marker((10, 10), 12);
        let action = proof_history_startup_reconciliation(
            || Ok(Some(proof_window((10, 10), (20, 21)))),
            |_| {
                let snapshot = if opens.replace(opens.get() + 1) == 0 {
                    first_snapshot_child
                } else {
                    later_snapshot_child
                };
                Ok(ProofHistoryCanonicalSnapshot {
                    canonical_best: 20,
                    canonical_earliest_hash: Some(hash(10)),
                    canonical_latest_hash: Some(hash(20)),
                    first_removed: Some(snapshot),
                })
            },
            storage_path(),
        )
        .expect("canonical earliest should allow an unwind");

        assert_eq!(
            action,
            ProofHistoryStartupAction::Unwind { first_removed: first_snapshot_child }
        );
        assert_eq!(opens.get(), 1, "unwind must not reopen the canonical provider");
    }

    #[test]
    fn post_init_reconciliation_detects_noncanonical_anchor_before_readiness() {
        let readiness = ProofHistoryReadiness::new();
        let result = proof_history_startup_reconciliation(
            || Ok(Some(proof_window((20, 21), (20, 21)))),
            |_| {
                Ok(ProofHistoryCanonicalSnapshot {
                    canonical_best: 20,
                    canonical_earliest_hash: Some(hash(20)),
                    canonical_latest_hash: Some(hash(20)),
                    first_removed: None,
                })
            },
            storage_path(),
        )
        .and_then(|action| {
            if action == ProofHistoryStartupAction::Ready {
                readiness.set_ready();
            }
            Ok(action)
        });

        let error = result.expect_err("a moved initialization anchor must fail closed").to_string();
        assert!(!readiness.is_ready());
        assert!(error.contains(&storage_path().display().to_string()));
        assert!(error.contains("wipe"));
        assert!(error.contains("fresh path"));
    }
}
