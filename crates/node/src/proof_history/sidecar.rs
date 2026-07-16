//! Proof-history sidecar: notification handling, sync loop, pruner task.

use super::{
    config::ProofHistoryConfig,
    engine::{ProofHistoryEngine, ReorgBlockUpdates},
    live::LiveTrieCollector,
    opt_block,
    prune::{FinalityProofHistoryPruner, FinalityPruneOutcome, startup_prune_exposure},
    storage_init::{
        initialize_finalized_window_proof_history_storage, initialize_proof_history_storage,
        proof_history_sync_target,
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
        StageCheckpointReader, StateProviderFactory, StateReader, TransactionVariant,
    },
    tasks::{TaskExecutor, shutdown::GracefulShutdown},
};
use reth_db::Database;
use reth_evm::ConfigureEvm;
use reth_execution_types::Chain;
use reth_node_api::{FullNodeComponents, NodePrimitives, NodeTypes};
use reth_optimism_trie::{
    OpProofsBackfillStore, OpProofsStorage, OpProofsStorageError, OpProofsStore,
    api::{OpProofsProviderRO, OpProofsProviderRw, ProofWindowRange},
};
use reth_storage_api::{
    ChainStateBlockReader, ChangeSetReader, StorageChangeSetReader, StorageSettingsCache,
};
use reth_trie_common::{HashedPostStateSorted, SortedTrieData, updates::TrieUpdatesSorted};
use std::{panic, path::Path, sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, Notify, broadcast},
    task,
    time::{self, MissedTickBehavior},
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

/// Number of blocks the proof-history sync task executes in one batch.
const PROOF_HISTORY_SYNC_BATCH_SIZE: usize = 50;

/// Delay used when proof-history has no locally executable backfill work.
const PROOF_HISTORY_SYNC_IDLE_SLEEP: Duration = Duration::from_secs(5);

/// Delay used while waiting for delayed proof-history initialization to become possible.
const PROOF_HISTORY_DELAYED_START_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Delay between polls of the node's executed head while proof-history is caught up, so a
/// staged-sync gap is backfilled even when no live canonical notification arrives.
const PROOF_HISTORY_HEAD_POLL_INTERVAL: Duration = Duration::from_secs(5);

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

/// Temporary bridge that applies the new notification policy through the existing synchronous
/// collector until the upstream engine becomes the sole owner in the next migration step.
///
/// This adapter deliberately owns clones of every dependency so it satisfies the same `'static`
/// contract as [`ProofHistoryEngine`] without changing the current sidecar worker lifecycle.
struct LegacyNotificationEngine<Evm, Provider, Storage> {
    /// EVM configuration used by the legacy exact-block executor.
    evm_config: Evm,
    /// Canonical state provider used to execute exact notification blocks.
    provider: Provider,
    /// Proof-history store updated synchronously by legacy collector operations.
    storage: OpProofsStorage<Storage>,
    /// Existing background worker wake-up used to bridge engine sync requests.
    sync_wake: Arc<Notify>,
}

impl<Evm, Provider, Storage> LegacyNotificationEngine<Evm, Provider, Storage> {
    /// Creates the narrow notification bridge without spawning another storage owner.
    fn new(
        evm_config: Evm,
        provider: Provider,
        storage: OpProofsStorage<Storage>,
        sync_wake: Arc<Notify>,
    ) -> Self {
        Self { evm_config, provider, storage, sync_wake }
    }
}

impl<Evm, Provider, Storage> ProofHistoryEngine<<Evm::Primitives as NodePrimitives>::Block>
    for LegacyNotificationEngine<Evm, Provider, Storage>
where
    Evm: ConfigureEvm + Clone + Send + 'static,
    Evm::Primitives: NodePrimitives,
    Provider: StateReader + DatabaseProviderFactory + StateProviderFactory + Clone + Send + 'static,
    Storage: OpProofsStore + Clone + Send + 'static,
{
    /// Executes and commits the exact notification block through the legacy collector.
    fn execute_block(
        &self,
        block: &reth_primitives_traits::RecoveredBlock<<Evm::Primitives as NodePrimitives>::Block>,
    ) -> eyre::Result<()> {
        LiveTrieCollector::new(self.evm_config.clone(), self.provider.clone(), &self.storage)
            .execute_and_store_block_updates(block)?;
        Ok(())
    }

    /// Commits precomputed updates through the legacy collector.
    fn index_block(
        &self,
        block: BlockWithParent,
        trie_updates: TrieUpdatesSorted,
        post_state: HashedPostStateSorted,
    ) -> eyre::Result<()> {
        LiveTrieCollector::new(self.evm_config.clone(), self.provider.clone(), &self.storage)
            .store_block_updates(block, trie_updates, post_state)?;
        Ok(())
    }

    /// Applies one precomputed replacement suffix through the legacy collector transaction.
    fn reorg(&self, updates: ReorgBlockUpdates) -> eyre::Result<()> {
        LiveTrieCollector::new(self.evm_config.clone(), self.provider.clone(), &self.storage)
            .unwind_and_store_block_updates(updates)?;
        Ok(())
    }

    /// Removes the supplied block and all descendants through the legacy collector.
    fn unwind(&self, from: BlockWithParent) -> eyre::Result<()> {
        LiveTrieCollector::new(self.evm_config.clone(), self.provider.clone(), &self.storage)
            .unwind_history(from)?;
        Ok(())
    }

    /// Wakes the existing sync worker, which intentionally ignores this numeric target because
    /// it re-reads the persisted database head before every batch.
    fn sync_to(&self, _target: u64) -> eyre::Result<()> {
        self.sync_wake.notify_one();
        Ok(())
    }
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
#[derive(Debug)]
pub(super) struct ProofHistorySidecar<Node, Storage>
where
    Node: FullNodeComponents,
{
    /// Canonical provider used for state notifications and block reads.
    provider: Node::Provider,
    /// EVM configuration used to execute blocks for proof-history updates.
    evm_config: Node::Evm,
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
    /// Serializes proof-history writers across live notifications, background sync, and pruning.
    write_lock: Arc<Mutex<()>>,
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
    Engine: ProofHistoryEngine<Primitives::Block>,
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

/// Routes a commit notification, processing only the suffix not already present in committed
/// proof-history storage.
///
/// A gap is delegated to the engine's canonical sync path. Engine failures clear readiness so
/// RPC callers never observe a sidecar that knows notification processing failed.
fn route_chain_commit<Primitives, Engine>(
    new: &Chain<Primitives>,
    committed_latest: BlockNumHash,
    verification_interval: u64,
    engine: &Engine,
    readiness: &ProofHistoryReadiness,
) -> eyre::Result<()>
where
    Primitives: NodePrimitives,
    Engine: ProofHistoryEngine<Primitives::Block>,
{
    if new.is_empty() {
        readiness.set_not_ready();
        return Err(eyre!("canonical commit notification contains no blocks"));
    }

    if new.tip().number() <= committed_latest.number {
        return Ok(())
    }

    if !committed_chain_is_contiguous(new.first().number(), committed_latest.number) ||
        !commit_notification_covers_suffix(new, committed_latest)
            .inspect_err(|_| readiness.set_not_ready())?
    {
        return engine.sync_to(new.tip().number()).inspect_err(|_| readiness.set_not_ready())
    }

    for block_number in committed_latest.number.saturating_add(1)..=new.tip().number() {
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
/// storage with [`complete_canonical_update`].
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
    Engine: ProofHistoryEngine<Primitives::Block>,
{
    readiness.set_not_ready();
    validate_replacement_chain(old, new)?;

    let first_old = BlockNumHash::new(old.first().number(), old.first().hash());
    ensure_canonical_update_above_earliest("reorg", snapshot.committed_earliest, first_old)?;

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
    Engine: ProofHistoryEngine<Primitives::Block>,
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
    engine.unwind(old.first().block_with_parent())
}

/// Restores readiness only after committed proof-history storage reconciles successfully.
fn complete_canonical_update<Reconcile>(
    readiness: &ProofHistoryReadiness,
    reconcile: Reconcile,
) -> eyre::Result<()>
where
    Reconcile: FnOnce() -> eyre::Result<bool>,
{
    if !reconcile()? {
        return Err(eyre!(
            "proof-history canonical update completed but committed storage could not be reconciled"
        ));
    }
    readiness.set_ready();
    Ok(())
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

impl<Node, Storage> ProofHistorySidecar<Node, Storage>
where
    Node: FullNodeComponents,
{
    /// Creates a proof-history sidecar with Taiko backfill guards.
    pub(super) fn new(
        provider: Node::Provider,
        evm_config: Node::Evm,
        task_executor: TaskExecutor,
        storage: OpProofsStorage<Storage>,
        init_storage: Storage,
        config: ProofHistoryConfig,
        readiness: ProofHistoryReadiness,
    ) -> Self {
        Self {
            provider,
            evm_config,
            task_executor,
            storage,
            init_storage,
            config,
            readiness,
            write_lock: Arc::new(Mutex::new(())),
        }
    }
}

impl<Node, Storage, Primitives> ProofHistorySidecar<Node, Storage>
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
    Storage: OpProofsBackfillStore + Clone + Send + 'static,
{
    /// Runs proof-history indexing until the node shuts down.
    pub(super) async fn run(self, mut shutdown: GracefulShutdown) -> eyre::Result<()> {
        let mut notifications = self.provider.subscribe_to_canonical_state();
        let mut sync_wake = self.try_start().await?;
        let mut retry_interval = time::interval(PROOF_HISTORY_DELAYED_START_RETRY_INTERVAL);
        retry_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                notification = notifications.recv() => {
                    let notification = match notification {
                        Ok(notification) => notification,
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                target: "reth::taiko::proof_history",
                                skipped,
                                "proof-history sidecar lagged canonical notifications; reconciling storage"
                            );
                            // Replace the lagged receiver before reconciliation. The old
                            // receiver's retained suffix is no longer useful after storage is
                            // reconciled, and the fresh receiver buffers commits published while
                            // recovery brings storage back in line with canonical state.
                            notifications = self.provider.subscribe_to_canonical_state();
                            if let Some(wake) = sync_wake.as_ref() {
                                self.recover_from_lag(wake).await?;
                            } else {
                                sync_wake = self.try_start().await?;
                            }
                            continue;
                        }
                    };
                    if sync_wake.is_none() {
                        sync_wake = self.try_start().await?;
                    }

                    let Some(wake) = sync_wake.as_ref() else {
                        continue;
                    };

                    let engine = LegacyNotificationEngine::new(
                        self.evm_config.clone(),
                        self.provider.clone(),
                        self.storage.clone(),
                        Arc::clone(wake),
                    );
                    self.handle_notification(notification, &engine).await?;
                }
                _ = &mut shutdown => break,
                _ = retry_interval.tick(), if sync_wake.is_none() => {
                    sync_wake = self.try_start().await?;
                }
            }
        }

        Ok(())
    }

    /// Reconciles storage if possible and spawns the sync and pruner tasks on first success.
    async fn try_start(&self) -> eyre::Result<Option<Arc<Notify>>> {
        if !self.prepare_storage_or_wait().await? {
            return Ok(None);
        }
        if !self.reconcile_or_wait().await? {
            return Ok(None);
        }
        // Storage bounds are now validated against canonical hashes: allow the RPC layer to
        // serve from proof-history. Workers only extend storage consistently from here on.
        self.readiness.set_ready();
        let sync_wake = self.spawn_sync_task();
        self.spawn_pruner_task();
        Ok(Some(sync_wake))
    }

    /// Reconciles proof-history storage after missing canonical notifications.
    async fn recover_from_lag(&self, sync_wake: &Notify) -> eyre::Result<()> {
        let _write_guard = self.write_lock.lock().await;
        // Notifications were missed, so the stored bounds are unvalidated until reconciliation
        // succeeds; stop serving proof-history state in the meantime.
        self.readiness.set_not_ready();
        if !self.reconcile_or_wait().await? {
            return Err(eyre!(
                "proof-history reconciliation cannot proceed while sync workers are running \
                 (canonical state moved backwards during a notification lag); restart the node to \
                 recover"
            ));
        }
        self.readiness.set_ready();
        sync_wake.notify_one();
        Ok(())
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
        let write_lock = self.write_lock.clone();

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
                                let _write_guard = write_lock.lock().await;
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

    /// Spawns the guarded proof-history backfill task and returns its wake-up handle.
    fn spawn_sync_task(&self) -> Arc<Notify> {
        let sync_wake = Arc::new(Notify::new());
        let task_wake = sync_wake.clone();
        let task_storage = self.storage.clone();
        let task_provider = self.provider.clone();
        let task_evm_config = self.evm_config.clone();
        let task_write_lock = self.write_lock.clone();

        self.task_executor.spawn_critical_with_graceful_shutdown_signal(
            "taiko::proof_history::sync_loop",
            move |shutdown| {
                Box::pin(async move {
                    Self::sync_loop(
                        shutdown,
                        task_wake,
                        task_storage,
                        task_provider,
                        task_evm_config,
                        task_write_lock,
                    )
                    .await;
                })
            },
        );

        sync_wake
    }

    /// Backfills proof-history only through blocks the node has locally executed.
    ///
    /// Live notifications only wake this loop up; the backfill target is always re-derived from
    /// the node's on-disk executed head, so re-execution never reads unpersisted blocks and a
    /// staged-sync gap is caught up even when no notification arrives.
    async fn sync_loop(
        mut shutdown: GracefulShutdown,
        wake: Arc<Notify>,
        storage: OpProofsStorage<Storage>,
        provider: Node::Provider,
        evm_config: Node::Evm,
        write_lock: Arc<Mutex<()>>,
    ) {
        debug!(target: "reth::taiko::proof_history", "starting proof-history sync loop");

        // Whether the current divergence episode (stored head ahead of executed head) has already
        // been warned about, so the 5s idle poll does not re-emit the warning on every tick.
        let mut divergence_logged = false;

        loop {
            let write_guard = write_lock.lock().await;
            let latest = match storage.provider_ro().and_then(|p| p.get_latest_block()) {
                Ok(numhash) => numhash.number,
                Err(OpProofsStorageError::NoBlocksFound) => {
                    error!(target: "reth::taiko::proof_history", "proof-history sync loop found no stored blocks; stopping sync loop");
                    return;
                }
                Err(error) => {
                    error!(target: "reth::taiko::proof_history", ?error, "failed to read proof-history latest block");
                    drop(write_guard);
                    if Self::sleep_or_shutdown(&mut shutdown, PROOF_HISTORY_SYNC_IDLE_SLEEP).await {
                        return;
                    }
                    continue;
                }
            };

            // Track the node's on-disk executed head, not just the last notified canonical tip.
            // Using the on-disk head (rather than the in-memory tip) guarantees the blocks the
            // backfill re-executes are persisted, and lets proof-history catch up across a
            // staged-sync gap even when no live notification arrives.
            let executed_head = match provider
                .database_provider_ro()
                .and_then(|p| p.best_block_number())
            {
                Ok(number) => number,
                Err(error) => {
                    error!(target: "reth::taiko::proof_history", ?error, "failed to read executed head for proof-history sync");
                    drop(write_guard);
                    if Self::sleep_or_shutdown(&mut shutdown, PROOF_HISTORY_SYNC_IDLE_SLEEP).await {
                        return;
                    }
                    continue;
                }
            };

            // Surface divergence: proof-history's stored head sits above the node's executed head.
            // This only arises after the on-disk head regresses (a reorg/unwind) and is normally
            // repaired by the notification-driven reorg/revert handlers — which never run when no
            // live notification arrives, the staged-sync-gap case this loop guards. Warn once per
            // episode (the idle poll would otherwise re-warn every tick) so a stuck/diverged
            // sidecar is observable instead of indistinguishable from healthy idle.
            if latest > executed_head {
                if !divergence_logged {
                    warn!(
                        target: "reth::taiko::proof_history",
                        latest,
                        executed_head,
                        "proof-history stored head is ahead of the node's executed head; awaiting a canonical notification to reconcile"
                    );
                    divergence_logged = true;
                }
            } else {
                divergence_logged = false;
            }

            let Some(target) = proof_history_sync_target(latest, executed_head) else {
                // Caught up to the locally executed head. Wake on the next live notification (fast
                // path) or after a poll delay, so a staged-sync gap is still picked up with no
                // notifications.
                drop(write_guard);
                tokio::select! {
                    _ = &mut shutdown => {
                        info!(target: "reth::taiko::proof_history", "proof-history sync loop stopped");
                        return;
                    }
                    _ = wake.notified() => {}
                    _ = time::sleep(PROOF_HISTORY_HEAD_POLL_INTERVAL) => {}
                }
                continue;
            };

            let batch_provider = provider.clone();
            let batch_storage = storage.clone();
            let batch_evm_config = evm_config.clone();
            // Each block write commits independently; if this batch fails part-way through, the
            // next loop rereads `latest` and resumes after the last committed block.
            let mut batch_task = task::spawn_blocking(move || {
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
            let batch_result = tokio::select! {
                result = &mut batch_task => {
                    blocking_join_result(result, "proof-history batch worker")
                        .and_then(|result| result)
                }
                _ = &mut shutdown => {
                    // `spawn_blocking` workers cannot be aborted; wait for the in-flight batch so
                    // its per-block commits finish cleanly before stopping.
                    info!(
                        target: "reth::taiko::proof_history",
                        "shutdown requested while proof-history backfill batch is running; waiting for batch to finish"
                    );
                    let result = blocking_join_result(batch_task.await, "proof-history batch worker")
                        .and_then(|result| result);
                    drop(write_guard);
                    if let Err(error) = result {
                        error!(target: "reth::taiko::proof_history", ?error, "proof-history batch processing failed");
                    }
                    info!(target: "reth::taiko::proof_history", "proof-history sync loop stopped");
                    return;
                }
            };
            drop(write_guard);

            match batch_result {
                Ok(backfilled_to) => {
                    info!(
                        target: "reth::taiko::proof_history",
                        backfilled_to,
                        head = executed_head,
                        "proof-history backfill batch committed"
                    );
                }
                Err(error) => {
                    error!(target: "reth::taiko::proof_history", ?error, "proof-history batch processing failed");
                    if Self::sleep_or_shutdown(&mut shutdown, PROOF_HISTORY_SYNC_IDLE_SLEEP).await {
                        return;
                    }
                }
            }

            task::yield_now().await;
        }
    }

    /// Sleeps for `duration` unless shutdown is requested first; returns whether to stop.
    async fn sleep_or_shutdown(shutdown: &mut GracefulShutdown, duration: Duration) -> bool {
        tokio::select! {
            _ = shutdown => {
                info!(target: "reth::taiko::proof_history", "proof-history sync loop stopped");
                true
            }
            _ = time::sleep(duration) => false,
        }
    }

    /// Processes a bounded batch of canonical blocks into proof-history storage.
    ///
    /// Returns the highest block number processed in this batch.
    fn process_batch(
        start: u64,
        target: u64,
        provider: &Node::Provider,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
        batch_size: usize,
    ) -> eyre::Result<u64> {
        let end = start.saturating_add(batch_size as u64).min(target);
        debug!(target: "reth::taiko::proof_history", start, end, "processing proof-history batch");

        for block_num in (start + 1)..=end {
            let block = provider
                .recovered_block(block_num.into(), TransactionVariant::NoHash)?
                .ok_or_else(|| eyre!("missing block {block_num}"))?;
            collector.execute_and_store_block_updates(&block)?;
        }

        Ok(end)
    }

    /// Handles one canonical notification using exact notification blocks and the engine seam.
    async fn handle_notification<Engine>(
        &self,
        notification: CanonStateNotification<Primitives>,
        engine: &Engine,
    ) -> eyre::Result<()>
    where
        Engine: ProofHistoryEngine<Primitives::Block>,
    {
        let _write_guard = self.write_lock.lock().await;
        let provider_ro = self.storage.provider_ro()?;
        let (earliest_number, earliest_hash) = opt_block(provider_ro.get_earliest_block())?
            .ok_or_else(|| eyre!("no earliest proof-history block stored"))?;
        let (latest_number, latest_hash) = opt_block(provider_ro.get_latest_block())?
            .ok_or_else(|| eyre!("no latest proof-history block stored"))?;
        let earliest_stored = BlockNumHash::new(earliest_number, earliest_hash);
        let latest_stored = BlockNumHash::new(latest_number, latest_hash);
        drop(provider_ro);

        // Pin the duplicate decision to one persisted canonical snapshot. In particular, a proof
        // head at the replacement tip is not considered covered merely because the heights
        // match: its exact hash must still be canonical in this snapshot.
        let canonical_snapshot = self.provider.database_provider_ro()?;
        let canonical_latest_hash =
            canonical_snapshot.sealed_header(latest_stored.number)?.map(|header| header.hash());
        let canonical_replacement_tip_hash = match &notification {
            CanonStateNotification::Reorg { new, .. } if !new.is_empty() => {
                canonical_snapshot.sealed_header(new.tip().number())?.map(|header| header.hash())
            }
            _ => None,
        };
        drop(canonical_snapshot);

        match &notification {
            CanonStateNotification::Commit { new } => route_chain_commit(
                new,
                latest_stored,
                self.config.verification_interval,
                engine,
                &self.readiness,
            )?,
            // A reorg that replaces the old blocks with nothing is a plain revert.
            CanonStateNotification::Reorg { old, new } if new.is_empty() => {
                route_chain_revert(old, earliest_stored, engine, &self.readiness)?;
                self.complete_notification_reconciliation().await?;
            }
            CanonStateNotification::Reorg { old, new } => {
                route_chain_reorg(
                    old,
                    new,
                    ReorgRoutingSnapshot {
                        committed_earliest: earliest_stored,
                        committed_latest: latest_stored,
                        canonical_latest_hash,
                        canonical_replacement_tip_hash,
                    },
                    self.config.verification_interval,
                    engine,
                    &self.readiness,
                )?;
                self.complete_notification_reconciliation().await?;
            }
        }

        Ok(())
    }

    /// Reconciles committed proof storage after a reorg or revert, then restores RPC readiness.
    async fn complete_notification_reconciliation(&self) -> eyre::Result<()> {
        let reconciled = self.reconcile_or_wait().await?;
        complete_canonical_update(&self.readiness, || Ok(reconciled))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProofHistoryCanonicalSnapshot, ProofHistoryStartupAction, ReorgRoutingSnapshot,
        committed_chain_is_contiguous, complete_canonical_update,
        ensure_canonical_update_above_earliest, proof_history_startup_action,
        proof_history_startup_reconciliation, route_chain_commit, route_chain_reorg,
        route_chain_revert,
    };
    use crate::proof_history::engine::{ProofHistoryEngine, ReorgBlockUpdates};
    use alethia_reth_rpc::proof_state::ProofHistoryReadiness;
    use alloy_consensus::{BlockHeader, Header};
    use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
    use alloy_primitives::B256;
    use reth_ethereum_primitives::{Block, BlockBody, EthPrimitives};
    use reth_execution_types::Chain;
    use reth_optimism_trie::api::ProofWindowRange;
    use reth_primitives_traits::{Block as _, RecoveredBlock};
    use reth_trie_common::{
        ComputedTrieData, HashedPostStateSorted, LazyTrieData, updates::TrieUpdatesSorted,
    };
    use std::{
        cell::Cell,
        collections::BTreeMap,
        path::Path,
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

    #[test]
    fn commit_indexes_precomputed_notification_data() {
        let parent = hash(1);
        let block = test_block(11, parent, 11);
        let chain = test_chain(vec![block.clone()], &[11]);
        let engine = RecordingEngine::default();
        let readiness = ready_flag();

        route_chain_commit(&chain, BlockNumHash::new(10, parent), 0, &engine, &readiness)
            .expect("contiguous precomputed commit is indexed");

        assert_eq!(engine.calls(), vec![EngineCall::Index(block.block_with_parent())]);
        assert!(readiness.is_ready());
    }

    #[test]
    fn commit_executes_at_verification_height() {
        let parent = hash(1);
        let block = test_block(12, parent, 12);
        let chain = test_chain(vec![block.clone()], &[12]);
        let engine = RecordingEngine::default();

        route_chain_commit(&chain, BlockNumHash::new(11, parent), 3, &engine, &ready_flag())
            .expect("verification height is executed");

        assert_eq!(engine.calls(), vec![EngineCall::Execute(BlockNumHash::new(12, block.hash()))]);
    }

    #[test]
    fn commit_executes_when_trie_data_is_missing() {
        let parent = hash(1);
        let block = test_block(11, parent, 13);
        let chain = test_chain(vec![block.clone()], &[]);
        let engine = RecordingEngine::default();

        route_chain_commit(&chain, BlockNumHash::new(10, parent), 0, &engine, &ready_flag())
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

        route_chain_commit(&chain, BlockNumHash::new(10, parent), 0, &engine, &ready_flag())
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

        route_chain_commit(&chain, BlockNumHash::new(11, stored.hash()), 0, &engine, &ready_flag())
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
            BlockNumHash::new(tip.number(), tip.hash()),
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

        route_chain_commit(&chain, BlockNumHash::new(10, hash(10)), 0, &engine, &ready_flag())
            .expect("gap is delegated to engine sync");

        assert_eq!(engine.calls(), vec![EngineCall::Sync(13)]);
    }

    #[test]
    fn gapped_commit_sync_failure_clears_readiness() {
        let chain = linear_chain(12, hash(11), &[12], &[12]);
        let engine = RecordingEngine::failing(EngineMethod::Sync);
        let readiness = ready_flag();

        let _error =
            route_chain_commit(&chain, BlockNumHash::new(10, hash(10)), 0, &engine, &readiness)
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

        route_chain_commit(&chain, BlockNumHash::new(10, parent), 0, &engine, &ready_flag())
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

    #[test]
    fn successful_reorg_reconciles_before_becoming_ready() {
        let readiness = ready_flag();
        readiness.set_not_ready();
        let reconciliation_observed_not_ready = Cell::new(false);

        complete_canonical_update(&readiness, || {
            reconciliation_observed_not_ready.set(!readiness.is_ready());
            Ok(true)
        })
        .expect("successful reconciliation restores readiness");

        assert!(reconciliation_observed_not_ready.get());
        assert!(readiness.is_ready());
    }

    #[test]
    fn reconciliation_failure_leaves_readiness_false() {
        let readiness = ready_flag();
        readiness.set_not_ready();

        let _error = complete_canonical_update(&readiness, || {
            Err(eyre::eyre!("injected reconciliation failure"))
        })
        .expect_err("reconciliation failure propagates");

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

        let _error =
            route_chain_commit(&new, BlockNumHash::new(10, parent), 0, &engine, &readiness)
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
