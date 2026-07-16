//! Proof-history backed state provider factory for RPC execution witnesses.

use alloy_eips::{BlockId, BlockNumHash};
use alloy_primitives::B256;
use reth_optimism_trie::{
    OpProofsStorage, OpProofsStore, ProofWindowRange, api::OpProofsProviderRO,
    provider::OpProofsStateProviderRef,
};
use reth_provider::{
    BlockHashReader, BlockNumReader, BlockReaderIdExt, ProviderError, ProviderResult,
    StateProvider, StateProviderBox, StateProviderFactory,
};
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;
use std::{
    fmt,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tracing::{debug, warn};

/// Shared flag tracking whether proof-history storage is reconciled against canonical state.
///
/// The sidecar sets the flag once startup reconciliation (or lag recovery) has validated the
/// stored bounds against canonical block hashes, and clears it while reconciliation is pending.
/// The RPC layer refuses to serve proof-history state while the flag is clear: after an
/// ungraceful restart the stored head can describe a branch the canonical chain no longer
/// follows, and bounds alone cannot tell. The flag cannot cover a divergence that has not been
/// *detected* yet (in-flight snapshots, un-received notifications); it closes the known-waiting
/// window where the sidecar itself knows storage is unvalidated.
#[derive(Debug, Clone, Default)]
pub struct ProofHistoryReadiness(Arc<AtomicBool>);

impl ProofHistoryReadiness {
    /// Creates a readiness handle that starts not ready.
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks proof-history storage as reconciled with canonical state.
    pub fn set_ready(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Marks proof-history storage as pending reconciliation.
    pub fn set_not_ready(&self) {
        self.0.store(false, Ordering::Release);
    }

    /// Returns whether proof-history storage is reconciled with canonical state.
    pub fn is_ready(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Maximum distance behind the canonical tip for which an uncovered proof request may fall back
/// to canonical (revert-overlay) state instead of erroring.
///
/// The canonical fallback materializes per-block changesets from the requested block up to the
/// tip, so its cost grows without bound as the request moves into deep history. Near-tip misses
/// (sidecar catch-up lag, persistence gap) stay well within this bound; anything beyond it is
/// rejected instead of risking minutes of CPU and an OOM-sized overlay per request.
const PROOF_HISTORY_CANONICAL_FALLBACK_MAX_DISTANCE: u64 = 1024;

/// A proof request that proof-history cannot serve and that is too deep for canonical fallback.
#[derive(Debug, thiserror::Error)]
#[error(
    "block {block_number} cannot be served from proof-history storage (reconciled: {reconciled}, \
     retained bounds: {bounds:?}) and lies {distance} blocks behind the canonical tip; \
     deep-history proofs cannot be served from canonical fallback state"
)]
struct ProofHistoryDeepHistoryError {
    /// Requested block number.
    block_number: u64,
    /// Whether stored bounds were reconciled against canonical state at request time.
    reconciled: bool,
    /// Retained proof-history bounds at request time.
    bounds: Option<(u64, u64)>,
    /// Distance between the canonical tip and the requested block.
    distance: u64,
}

/// A transient error indicating that an exact canonical identity changed during an RPC request.
#[derive(Debug, thiserror::Error)]
#[error("canonical state changed; retry")]
struct CanonicalStateChangedError;

/// State resolved for the exact block identity requested by an RPC method.
pub(crate) enum ResolvedBlockState {
    /// Pending state, which has no stable canonical block identity.
    Pending(
        /// Pending provider returned by Reth's normal pending-state path.
        StateProviderBox,
    ),
    /// Canonical state pinned to the resolved block number and hash.
    Canonical {
        /// Exact canonical block whose post-state is exposed by `state`.
        block: BlockNumHash,
        /// Canonical state provider loaded by `block.hash`.
        state: StateProviderBox,
    },
}

impl fmt::Debug for ResolvedBlockState {
    /// Formats the resolution kind and exact canonical identity without formatting the provider.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending(_) => formatter.debug_tuple("Pending").field(&"<state>").finish(),
            Self::Canonical { block, .. } => formatter
                .debug_struct("Canonical")
                .field("block", block)
                .field("state", &"<state>")
                .finish(),
        }
    }
}

/// State selected for proof work, plus the identities that must remain canonical until return.
pub(crate) enum ProofStateProvider<'a> {
    /// Pending state served directly without consulting proof-history storage.
    Pending(
        /// Pending provider returned unchanged from exact block resolution.
        StateProviderBox,
    ),
    /// Canonical state, optionally overlaid by one proof-history read snapshot.
    Canonical {
        /// State provider used for the proof or witness computation.
        state: Box<dyn StateProvider + Send + 'a>,
        /// Exact identities that protect the computation from concurrent reorgs.
        guard: ProofStateGuard,
    },
}

impl fmt::Debug for ProofStateProvider<'_> {
    /// Formats the selection kind and guard without formatting the state provider.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending(_) => formatter.debug_tuple("Pending").field(&"<state>").finish(),
            Self::Canonical { guard, .. } => formatter
                .debug_struct("Canonical")
                .field("state", &"<state>")
                .field("guard", guard)
                .finish(),
        }
    }
}

/// Canonical identities captured before proof work and checked immediately before returning it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProofStateGuard {
    /// Exact block whose post-state was used as the proof computation base.
    state_block: BlockNumHash,
    /// Exact latest block visible in the proof-history snapshot, if an overlay was used.
    snapshot_latest: Option<BlockNumHash>,
}

impl ProofStateGuard {
    /// Creates a guard for one exact state block and optional proof-history snapshot endpoint.
    const fn new(state_block: BlockNumHash, snapshot_latest: Option<BlockNumHash>) -> Self {
        Self { state_block, snapshot_latest }
    }

    /// Returns the exact canonical block whose state backs the computation.
    #[cfg(test)]
    const fn state_block(self) -> BlockNumHash {
        self.state_block
    }

    /// Returns the captured proof-history endpoint, if an overlay backs the computation.
    #[cfg(test)]
    const fn snapshot_latest(self) -> Option<BlockNumHash> {
        self.snapshot_latest
    }

    /// Revalidates captured identities in linearization order after proof work completes.
    ///
    /// The optional debug target is deliberately checked last. Equal identities are not
    /// deduplicated because each check protects a distinct phase of the request.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "RPC consumers install final postvalidation in the next change"
        )
    )]
    pub(crate) fn validate_with<CanonicalHash>(
        &self,
        target: Option<BlockNumHash>,
        mut canonical_hash: CanonicalHash,
    ) -> ProviderResult<()>
    where
        CanonicalHash: FnMut(u64) -> ProviderResult<Option<B256>>,
    {
        if let Some(snapshot_latest) = self.snapshot_latest {
            ensure_canonical_identity(snapshot_latest, &mut canonical_hash)?;
        }
        ensure_canonical_identity(self.state_block, &mut canonical_hash)?;
        if let Some(target) = target {
            ensure_canonical_identity(target, &mut canonical_hash)?;
        }
        Ok(())
    }
}

/// Returns the stable transient error used whenever a captured canonical identity changes.
fn canonical_state_changed() -> ProviderError {
    ProviderError::other(CanonicalStateChangedError)
}

/// Verifies that `block` is still the canonical hash at its exact height.
fn ensure_canonical_identity<CanonicalHash>(
    block: BlockNumHash,
    canonical_hash: &mut CanonicalHash,
) -> ProviderResult<()>
where
    CanonicalHash: FnMut(u64) -> ProviderResult<Option<B256>>,
{
    match canonical_hash(block.number)? {
        Some(hash) if hash == block.hash => Ok(()),
        Some(_) | None => Err(canonical_state_changed()),
    }
}

/// Resolves pending state without entering the canonical-resolution path.
async fn resolve_block_state_with<PendingState, PendingFuture, CanonicalState>(
    block_id: BlockId,
    pending_state: PendingState,
    canonical_state: CanonicalState,
) -> ProviderResult<ResolvedBlockState>
where
    PendingState: FnOnce(BlockId) -> PendingFuture,
    PendingFuture: Future<Output = ProviderResult<StateProviderBox>>,
    CanonicalState: FnOnce(BlockId) -> ProviderResult<ResolvedBlockState>,
{
    if block_id.is_pending() {
        return pending_state(block_id).await.map(ResolvedBlockState::Pending)
    }
    canonical_state(block_id)
}

/// Resolves canonical state once and rejects identity changes around state-provider loading.
fn resolve_exact_canonical_state_with<ResolveHeader, CanonicalHash, LoadState>(
    block_id: BlockId,
    resolve_header: ResolveHeader,
    mut canonical_hash: CanonicalHash,
    load_state: LoadState,
) -> ProviderResult<ResolvedBlockState>
where
    ResolveHeader: FnOnce(BlockId) -> ProviderResult<Option<BlockNumHash>>,
    CanonicalHash: FnMut(u64) -> ProviderResult<Option<B256>>,
    LoadState: FnOnce(B256) -> ProviderResult<StateProviderBox>,
{
    let block = resolve_header(block_id)?
        .ok_or(EthApiError::HeaderNotFound(block_id))
        .map_err(ProviderError::other)?;
    ensure_canonical_identity(block, &mut canonical_hash)?;

    let state = load_state(block.hash);
    // The second identity check must run even if state loading failed. Otherwise a reorg can be
    // reported as an unrelated storage failure and callers may retry against the stale branch.
    ensure_canonical_identity(block, &mut canonical_hash)?;

    Ok(ResolvedBlockState::Canonical { block, state: state? })
}

/// Selects pending state directly or delegates exact canonical state to proof-history selection.
fn select_resolved_state_with<'a, SelectCanonical>(
    resolved: ResolvedBlockState,
    select_canonical: SelectCanonical,
) -> ProviderResult<ProofStateProvider<'a>>
where
    SelectCanonical:
        FnOnce(BlockNumHash, StateProviderBox) -> ProviderResult<ProofStateProvider<'a>>,
{
    match resolved {
        ResolvedBlockState::Pending(state) => Ok(ProofStateProvider::Pending(state)),
        ResolvedBlockState::Canonical { block, state } => select_canonical(block, state),
    }
}

/// Selects one exact canonical state from a pinned proof snapshot or bounded canonical fallback.
fn select_canonical_state_with<'a, ProofProvider, CanonicalHash, CanonicalTip>(
    canonical_state: StateProviderBox,
    state_block: BlockNumHash,
    provider_ro: ProofProvider,
    proof_window: Option<ProofWindowRange>,
    reconciled: bool,
    mut canonical_hash: CanonicalHash,
    canonical_tip: CanonicalTip,
) -> ProviderResult<ProofStateProvider<'a>>
where
    ProofProvider: OpProofsProviderRO + Clone + 'a,
    CanonicalHash: FnMut(u64) -> ProviderResult<Option<B256>>,
    CanonicalTip: FnOnce() -> ProviderResult<u64>,
{
    ensure_canonical_identity(state_block, &mut canonical_hash)?;

    let bounds = proof_window.map(|window| (window.earliest.number, window.latest.number));
    let covered = proof_history_covers(state_block.number, bounds);
    if covered {
        let window = proof_window.expect("covered proof window is present");
        let snapshot_latest = BlockNumHash::new(window.latest.number, window.latest.hash);
        // A numerically covered snapshot from another branch is unsafe even if canonical fallback
        // would be cheap. Fail closed so stale proof storage cannot be silently ignored.
        ensure_canonical_identity(snapshot_latest, &mut canonical_hash)?;

        if reconciled {
            let state = Box::new(OpProofsStateProviderRef::new(
                canonical_state,
                provider_ro,
                state_block.number,
            ));
            return Ok(ProofStateProvider::Canonical {
                state,
                guard: ProofStateGuard::new(state_block, Some(snapshot_latest)),
            })
        }
    }

    let canonical_tip = canonical_tip()?;
    if !proof_history_fallback_allowed(
        state_block.number,
        canonical_tip,
        PROOF_HISTORY_CANONICAL_FALLBACK_MAX_DISTANCE,
    ) {
        let distance = canonical_tip.saturating_sub(state_block.number);
        warn!(
            target: "reth::taiko::proof_history",
            block_number = state_block.number,
            reconciled,
            ?bounds,
            distance,
            "refusing canonical-state fallback for deep-history proof request"
        );
        return Err(ProviderError::other(ProofHistoryDeepHistoryError {
            block_number: state_block.number,
            reconciled,
            bounds,
            distance,
        }))
    }

    if !reconciled {
        debug!(
            target: "reth::taiko::proof_history",
            block_number = state_block.number,
            ?bounds,
            "proof-history is awaiting reconciliation; serving from canonical state"
        );
    } else if proof_history_miss_is_pruned(state_block.number, bounds) {
        warn!(
            target: "reth::taiko::proof_history",
            block_number = state_block.number,
            ?bounds,
            "proof-history has pruned requested block; serving from canonical state"
        );
    } else {
        debug!(
            target: "reth::taiko::proof_history",
            block_number = state_block.number,
            ?bounds,
            "proof-history does not yet cover requested block; serving from canonical state"
        );
    }

    Ok(ProofStateProvider::Canonical {
        state: canonical_state,
        guard: ProofStateGuard::new(state_block, None),
    })
}

/// Flattens a `spawn_blocking` join result, preserving panics and surfacing join failures.
pub(crate) fn flatten_blocking_task<T>(
    result: Result<Result<T, EthApiError>, tokio::task::JoinError>,
) -> Result<T, EthApiError> {
    match result {
        Ok(inner) => inner,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(EthApiError::EvmCustom(format!("blocking task failed to join: {error}"))),
    }
}

/// Returns whether proof-history storage can serve `block_number` given its retained bounds.
///
/// `bounds` is `Some((earliest, latest))` when storage is initialized, `None` when it is empty.
fn proof_history_covers(block_number: u64, bounds: Option<(u64, u64)>) -> bool {
    matches!(bounds, Some((earliest, latest)) if block_number >= earliest && block_number <= latest)
}

/// Returns whether an uncovered `block_number` falls below the retained window (pruned), as opposed
/// to ahead of it (the sidecar is still catching up) or with storage not yet initialized.
///
/// A pruned miss is a genuine gap worth a `WARN`; an ahead/uninitialized miss is expected and
/// transient, so it should not spam `WARN` on every `eth_getProof` call while proof-history lags.
fn proof_history_miss_is_pruned(block_number: u64, bounds: Option<(u64, u64)>) -> bool {
    matches!(bounds, Some((earliest, _)) if block_number < earliest)
}

/// Returns whether an uncovered `block_number` is close enough to the canonical tip to fall back
/// to canonical (revert-overlay) state instead of erroring.
fn proof_history_fallback_allowed(
    block_number: u64,
    canonical_tip: u64,
    max_distance: u64,
) -> bool {
    canonical_tip.saturating_sub(block_number) <= max_distance
}

/// Creates state providers that overlay OP Proofs history on top of canonical state.
#[derive(Debug, Clone)]
pub struct ProofHistoryStateProviderFactory<Eth, Storage> {
    /// Ethereum RPC API used to resolve block ids and canonical historical state.
    eth_api: Eth,
    /// Proof-history storage containing retained trie nodes and hashed leaves.
    storage: OpProofsStorage<Storage>,
    /// Whether the sidecar has reconciled the stored bounds against canonical state.
    readiness: ProofHistoryReadiness,
}

impl<Eth, Storage> ProofHistoryStateProviderFactory<Eth, Storage> {
    /// Creates a new proof-history state provider factory.
    pub const fn new(
        eth_api: Eth,
        storage: OpProofsStorage<Storage>,
        readiness: ProofHistoryReadiness,
    ) -> Self {
        Self { eth_api, storage, readiness }
    }
}

impl<Eth, Storage> ProofHistoryStateProviderFactory<Eth, Storage>
where
    Eth: FullEthApi + Send + Sync + 'static,
    Storage: OpProofsStore + Clone + 'static,
{
    /// Resolves a block id to pending state or one exact canonical number-and-hash identity.
    pub(crate) async fn resolve_block_state(
        &self,
        block_id: BlockId,
    ) -> ProviderResult<ResolvedBlockState> {
        resolve_block_state_with(
            block_id,
            |pending_id| async move {
                self.eth_api.state_at_block_id(pending_id).await.map_err(ProviderError::other)
            },
            |canonical_id| {
                let provider = self.eth_api.provider();
                resolve_exact_canonical_state_with(
                    canonical_id,
                    |id| {
                        provider.sealed_header_by_id(id).map(|header| header.map(|h| h.num_hash()))
                    },
                    |number| provider.block_hash(number),
                    |hash| provider.state_by_block_hash(hash),
                )
            },
        )
        .await
    }

    /// Selects pending state directly or wraps exact canonical state with proof-history state.
    ///
    /// The returned provider serves account and storage reads from proof-history storage while
    /// delegating non-state lookups, such as bytecode and block hashes, to the canonical provider.
    /// An uncovered block near the canonical tip falls back to the canonical provider (its
    /// revert overlay is a few blocks deep), so a lagging sidecar does not block proofs the node
    /// can still serve cheaply; an uncovered block deep in history is rejected instead of
    /// materializing an unbounded revert overlay.
    ///
    /// Opens a proof-history read snapshot and performs trie I/O: call from a blocking context.
    pub(crate) fn state_provider_at(
        &self,
        resolved: ResolvedBlockState,
    ) -> ProviderResult<ProofStateProvider<'_>> {
        select_resolved_state_with(resolved, |state_block, canonical_state| {
            let provider_ro = self.storage.provider_ro().map_err(ProviderError::from)?;
            let proof_window = match provider_ro.get_proof_window() {
                Ok(window) => Some(window),
                Err(reth_optimism_trie::OpProofsStorageError::NoBlocksFound) => None,
                Err(error) => return Err(ProviderError::from(error)),
            };
            select_canonical_state_with(
                canonical_state,
                state_block,
                provider_ro,
                proof_window,
                self.readiness.is_ready(),
                |number| self.eth_api.provider().block_hash(number),
                || self.eth_api.provider().best_block_number(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProofHistoryReadiness, ProofStateProvider, ResolvedBlockState, proof_history_covers,
        proof_history_fallback_allowed, proof_history_miss_is_pruned, resolve_block_state_with,
        resolve_exact_canonical_state_with, select_canonical_state_with,
        select_resolved_state_with,
    };
    use alloy_eips::{BlockId, BlockNumHash, BlockNumberOrTag, eip1898::BlockWithParent};
    use alloy_primitives::{B256, map::HashMap};
    use reth_optimism_trie::{
        BlockStateDiff, MdbxProofsStorageV2, OpProofsInitProvider, OpProofsProviderRO,
        OpProofsProviderRw, OpProofsStore,
    };
    use reth_provider::{
        ProviderError, ProviderResult, StateProviderBox, test_utils::MockEthProvider,
    };
    use std::{
        cell::{Cell, RefCell},
        sync::Arc,
    };

    fn test_state() -> StateProviderBox {
        Box::new(MockEthProvider::default())
    }

    fn hash(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    fn canonical_hashes(blocks: &[BlockNumHash]) -> HashMap<u64, B256> {
        blocks.iter().map(|block| (block.number, block.hash)).collect()
    }

    #[test]
    fn exact_hash_match_is_accepted() {
        let block = BlockNumHash::new(7, hash(0x77));
        let mut header_calls = 0;
        let mut state_calls = 0;

        let resolved = resolve_exact_canonical_state_with(
            BlockId::from(block.hash),
            |_| {
                header_calls += 1;
                Ok(Some(block))
            },
            |number| Ok((number == block.number).then_some(block.hash)),
            |requested_hash| {
                state_calls += 1;
                assert_eq!(requested_hash, block.hash);
                Ok(test_state())
            },
        )
        .expect("matching canonical identity resolves");

        assert!(
            matches!(resolved, ResolvedBlockState::Canonical { block: actual, .. } if actual == block)
        );
        assert_eq!(header_calls, 1);
        assert_eq!(state_calls, 1);
    }

    #[test]
    fn changed_or_missing_canonical_hash_returns_retry_error() {
        let block = BlockNumHash::new(8, hash(0x88));
        for canonical in [Some(hash(0x99)), None] {
            let err = resolve_exact_canonical_state_with(
                BlockId::from(block.hash),
                |_| Ok(Some(block)),
                |_| Ok(canonical),
                |_| Ok(test_state()),
            )
            .expect_err("a changed or missing canonical identity must fail");
            assert_eq!(err.to_string(), "canonical state changed; retry");
        }

        let canonical_reads = RefCell::new(vec![Some(block.hash), Some(hash(0xaa))].into_iter());
        let err = resolve_exact_canonical_state_with(
            BlockId::from(block.hash),
            |_| Ok(Some(block)),
            |_| Ok(canonical_reads.borrow_mut().next().expect("two canonical reads")),
            |_| Err(ProviderError::other(std::io::Error::other("state load failed"))),
        )
        .expect_err("the post-state-load identity check wins over a stale state error");
        assert_eq!(err.to_string(), "canonical state changed; retry");
    }

    #[test]
    fn postvalidation_checks_snapshot_then_state_then_target() {
        let snapshot_latest = BlockNumHash::new(5, hash(0x05));
        let state_block = BlockNumHash::new(4, hash(0x04));
        let target = state_block;
        let guard = super::ProofStateGuard::new(state_block, Some(snapshot_latest));
        let calls = RefCell::new(Vec::new());

        guard
            .validate_with(Some(target), |number| {
                calls.borrow_mut().push(number);
                Ok(match number {
                    5 => Some(snapshot_latest.hash),
                    4 => Some(state_block.hash),
                    _ => None,
                })
            })
            .expect("unchanged identities validate");

        assert_eq!(*calls.borrow(), vec![5, 4, 4]);
    }

    #[test]
    fn reorg_above_snapshot_latest_does_not_invalidate_guard() {
        let snapshot_latest = BlockNumHash::new(5, hash(0x05));
        let state_block = BlockNumHash::new(3, hash(0x03));
        let guard = super::ProofStateGuard::new(state_block, Some(snapshot_latest));
        let canonical =
            canonical_hashes(&[state_block, snapshot_latest, BlockNumHash::new(6, hash(0xb6))]);

        guard
            .validate_with(None, |number| Ok(canonical.get(&number).copied()))
            .expect("a branch change strictly above the snapshot is irrelevant");
    }

    #[test]
    fn latest_resolution_retains_exact_num_hash() {
        let latest = BlockNumHash::new(11, hash(0x11));
        let header_calls = RefCell::new(Vec::new());

        let resolved = resolve_exact_canonical_state_with(
            BlockId::Number(BlockNumberOrTag::Latest),
            |id| {
                header_calls.borrow_mut().push(id);
                Ok(Some(latest))
            },
            |_| Ok(Some(latest.hash)),
            |_| Ok(test_state()),
        )
        .expect("latest resolves");

        assert!(matches!(resolved, ResolvedBlockState::Canonical { block, .. } if block == latest));
        assert_eq!(*header_calls.borrow(), vec![BlockId::Number(BlockNumberOrTag::Latest)]);
    }

    #[tokio::test]
    async fn pending_resolution_returns_pending_variant() {
        let canonical_called = RefCell::new(false);
        let resolved = resolve_block_state_with(
            BlockId::Number(BlockNumberOrTag::Pending),
            |_| async { Ok(test_state()) },
            |_| {
                *canonical_called.borrow_mut() = true;
                unreachable!("pending resolution must not enter canonical lookup")
            },
        )
        .await
        .expect("pending state resolves");

        assert!(matches!(resolved, ResolvedBlockState::Pending(_)));
        assert!(!*canonical_called.borrow());
    }

    #[tokio::test]
    async fn pending_bypasses_empty_proof_store_and_tip_lookup() {
        let resolved = resolve_block_state_with(
            BlockId::Number(BlockNumberOrTag::Pending),
            |_| async { Ok(test_state()) },
            |_| unreachable!("pending must bypass canonical header lookup"),
        )
        .await
        .expect("pending state resolves");

        let selected = select_resolved_state_with(resolved, |_, _| {
            unreachable!("pending must bypass proof storage and canonical tip lookup")
        })
        .expect("pending state is returned directly");

        assert!(matches!(selected, ProofStateProvider::Pending(_)));
    }

    #[test]
    fn covered_request_captures_exact_snapshot_latest() {
        let storage = initialized_memory_storage();
        let provider_ro = storage.provider_ro().expect("proof snapshot opens");
        let window = provider_ro.get_proof_window().expect("proof window exists");
        let state_block = BlockNumHash::new(3, hash(0x03));
        let canonical = canonical_hashes(&[
            state_block,
            BlockNumHash::new(window.latest.number, window.latest.hash),
        ]);

        let selected = select_canonical_state_with(
            test_state(),
            state_block,
            provider_ro,
            Some(window),
            true,
            |number| Ok(canonical.get(&number).copied()),
            || unreachable!("a covered request must not read the fallback tip"),
        )
        .expect("covered canonical snapshot is selected");

        match selected {
            ProofStateProvider::Canonical { guard, .. } => {
                assert_eq!(guard.state_block(), state_block);
                assert_eq!(
                    guard.snapshot_latest(),
                    Some(BlockNumHash::new(window.latest.number, window.latest.hash))
                );
            }
            ProofStateProvider::Pending(_) => panic!("canonical input cannot become pending"),
        }
    }

    #[test]
    fn covered_request_falls_back_before_reconciliation() {
        let storage = initialized_memory_storage();
        let provider_ro = storage.provider_ro().expect("proof snapshot opens");
        let window = provider_ro.get_proof_window().expect("proof window exists");
        let state_block = BlockNumHash::new(3, hash(0x03));
        let canonical = canonical_hashes(&[
            state_block,
            BlockNumHash::new(window.latest.number, window.latest.hash),
        ]);
        let tip_called = Cell::new(false);

        let selected = select_canonical_state_with(
            test_state(),
            state_block,
            provider_ro,
            Some(window),
            false,
            |number| Ok(canonical.get(&number).copied()),
            || {
                tip_called.set(true);
                Ok(window.latest.number)
            },
        )
        .expect("unreconciled storage falls back to exact canonical state");

        assert!(tip_called.get());
        assert!(matches!(
            selected,
            ProofStateProvider::Canonical { guard, .. }
                if guard.state_block() == state_block && guard.snapshot_latest().is_none()
        ));
    }

    #[test]
    fn canonical_fallback_guard_has_no_snapshot_hash() {
        let storage = initialized_memory_storage();
        let provider_ro = storage.provider_ro().expect("proof snapshot opens");
        let window = provider_ro.get_proof_window().expect("proof window exists");
        let state_block = BlockNumHash::new(window.latest.number + 1, hash(0x06));

        let selected = select_canonical_state_with(
            test_state(),
            state_block,
            provider_ro,
            Some(window),
            true,
            |_| Ok(Some(state_block.hash)),
            || Ok(state_block.number),
        )
        .expect("near-tip miss falls back to exact canonical state");

        match selected {
            ProofStateProvider::Canonical { guard, .. } => {
                assert_eq!(guard.state_block(), state_block);
                assert_eq!(guard.snapshot_latest(), None);
            }
            ProofStateProvider::Pending(_) => panic!("canonical input cannot become pending"),
        }
    }

    #[test]
    fn covered_noncanonical_snapshot_fails_closed() {
        let storage = initialized_memory_storage();
        let provider_ro = storage.provider_ro().expect("proof snapshot opens");
        let window = provider_ro.get_proof_window().expect("proof window exists");
        let state_block = BlockNumHash::new(3, hash(0x03));

        let err = select_canonical_state_with(
            test_state(),
            state_block,
            provider_ro,
            Some(window),
            true,
            |number| {
                Ok(Some(if number == window.latest.number { hash(0xee) } else { state_block.hash }))
            },
            || -> ProviderResult<u64> {
                unreachable!("a covered branch mismatch must never fall back")
            },
        )
        .expect_err("noncanonical proof snapshot must fail closed");

        assert_eq!(err.to_string(), "canonical state changed; retry");
    }

    #[test]
    fn old_v2_snapshot_fails_after_live_store_reorg() {
        let dir = tempfile::tempdir().expect("proof tempdir opens");
        let storage =
            Arc::new(MdbxProofsStorageV2::new(dir.path()).expect("V2 proof-history storage opens"));
        let chain_a = initialize_chain(&storage, 5, 0x10);
        let old_ro = storage.provider_ro().expect("old V2 snapshot opens");
        let old_window = old_ro.get_proof_window().expect("old proof window exists");
        let state_block = chain_a[3];
        let canonical_a = canonical_hashes(&chain_a);

        let selected = select_canonical_state_with(
            test_state(),
            state_block,
            old_ro.clone(),
            Some(old_window),
            true,
            |number| Ok(canonical_a.get(&number).copied()),
            || unreachable!("covered V2 state must not use fallback"),
        )
        .expect("old canonical V2 snapshot selects");
        let guard = match &selected {
            ProofStateProvider::Canonical { guard, .. } => *guard,
            ProofStateProvider::Pending(_) => panic!("canonical input cannot become pending"),
        };

        let block_b5 = BlockNumHash::new(5, hash(0xb5));
        let rw = storage.provider_rw().expect("V2 reorg writer opens");
        rw.replace_updates(
            chain_a[4],
            vec![(BlockWithParent::new(chain_a[4].hash, block_b5), BlockStateDiff::default())],
        )
        .expect("live V2 suffix is replaced");
        OpProofsProviderRw::commit(rw).expect("live V2 reorg commits");

        assert_eq!(old_ro.get_proof_window().expect("old snapshot remains readable"), old_window);
        let mut canonical_b = canonical_a.clone();
        canonical_b.insert(5, block_b5.hash);
        let err = guard
            .validate_with(None, |number| Ok(canonical_b.get(&number).copied()))
            .expect_err("old proof snapshot must be rejected after the live branch changes");
        assert_eq!(err.to_string(), "canonical state changed; retry");

        canonical_b.insert(5, chain_a[5].hash);
        canonical_b.insert(6, hash(0xb6));
        guard
            .validate_with(None, |number| Ok(canonical_b.get(&number).copied()))
            .expect("a reorg only above the captured latest remains valid");

        drop(selected);
    }

    fn initialized_memory_storage() -> reth_optimism_trie::InMemoryProofsStorage {
        let storage = reth_optimism_trie::InMemoryProofsStorage::new();
        let _ = initialize_chain(&storage, 5, 0x00);
        storage
    }

    fn initialize_chain<Storage: OpProofsStore>(
        storage: &Storage,
        latest: u64,
        hash_offset: u8,
    ) -> Vec<BlockNumHash> {
        let chain = (0..=latest)
            .map(|number| BlockNumHash::new(number, hash(hash_offset.wrapping_add(number as u8))))
            .collect::<Vec<_>>();
        let init = storage.initialization_provider().expect("proof initializer opens");
        init.set_initial_state_anchor(chain[0]).expect("proof anchor stores");
        init.commit_initial_state().expect("proof anchor completes");
        OpProofsInitProvider::commit(init).expect("proof anchor commits");

        if latest > 0 {
            let rw = storage.provider_rw().expect("proof writer opens");
            for number in 1..=latest as usize {
                rw.store_trie_updates(
                    BlockWithParent::new(chain[number - 1].hash, chain[number]),
                    BlockStateDiff::default(),
                )
                .expect("proof block stores");
            }
            OpProofsProviderRw::commit(rw).expect("proof suffix commits");
        }
        chain
    }

    #[test]
    fn covers_block_within_bounds() {
        assert!(proof_history_covers(150, Some((100, 200))));
    }

    #[test]
    fn covers_inclusive_boundaries() {
        assert!(proof_history_covers(100, Some((100, 200))));
        assert!(proof_history_covers(200, Some((100, 200))));
    }

    #[test]
    fn does_not_cover_block_above_latest() {
        // The incident: requested block sits above latest_stored -> fall back to canonical state.
        assert!(!proof_history_covers(8_109_699, Some((7_503_971, 8_108_771))));
    }

    #[test]
    fn does_not_cover_block_below_earliest() {
        assert!(!proof_history_covers(50, Some((100, 200))));
    }

    #[test]
    fn does_not_cover_when_storage_empty() {
        assert!(!proof_history_covers(150, None));
    }

    #[test]
    fn miss_below_earliest_is_pruned() {
        // Below the retained window: a genuine gap worth a WARN.
        assert!(proof_history_miss_is_pruned(50, Some((100, 200))));
    }

    #[test]
    fn miss_above_latest_is_not_pruned() {
        // Ahead of the cursor while the sidecar catches up: expected and transient, not a WARN.
        assert!(!proof_history_miss_is_pruned(8_109_699, Some((7_503_971, 8_108_771))));
    }

    #[test]
    fn miss_with_empty_storage_is_not_pruned() {
        // Uninitialized storage: transient startup state, not a WARN.
        assert!(!proof_history_miss_is_pruned(150, None));
    }

    #[test]
    fn fallback_allowed_near_tip() {
        // Sidecar catch-up lag: cheap revert overlay, serve from canonical state.
        assert!(proof_history_fallback_allowed(8_109_000, 8_109_500, 1024));
        // Exactly at the distance bound stays allowed.
        assert!(proof_history_fallback_allowed(100, 1124, 1024));
    }

    #[test]
    fn fallback_rejected_deep_in_history() {
        // A proof for a block far below the retained window would materialize changesets for
        // every block up to the tip: reject instead of risking an unbounded overlay.
        assert!(!proof_history_fallback_allowed(1, 8_109_500, 1024));
        assert!(!proof_history_fallback_allowed(100, 1125, 1024));
    }

    #[test]
    fn fallback_allowed_for_future_blocks() {
        // A requested block above the tip has zero look-back distance; resolution already
        // failed earlier if the block does not exist.
        assert!(proof_history_fallback_allowed(1000, 900, 1024));
    }

    #[test]
    fn readiness_starts_not_ready_and_toggles() {
        let readiness = ProofHistoryReadiness::new();

        assert!(!readiness.is_ready());
        readiness.set_ready();
        assert!(readiness.is_ready());
        readiness.set_not_ready();
        assert!(!readiness.is_ready());
    }

    #[test]
    fn readiness_is_shared_across_clones() {
        let sidecar_handle = ProofHistoryReadiness::new();
        let rpc_handle = sidecar_handle.clone();

        sidecar_handle.set_ready();

        assert!(rpc_handle.is_ready());
    }
}
