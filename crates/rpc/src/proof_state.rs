//! Proof-history backed state provider factory for RPC execution witnesses.

use alloy_eips::BlockId;
use reth_optimism_trie::{
    OpProofsStorage, OpProofsStore, api::OpProofsProviderRO, provider::OpProofsStateProviderRef,
};
use reth_provider::{
    BlockIdReader, BlockNumReader, ProviderError, ProviderResult, StateProvider, StateProviderBox,
};
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
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

/// Returns whether proof-history storage may serve `block_number`: it must be within the retained
/// bounds *and* the bounds must have been reconciled against canonical state.
fn proof_history_can_serve(
    reconciled: bool,
    block_number: u64,
    bounds: Option<(u64, u64)>,
) -> bool {
    reconciled && proof_history_covers(block_number, bounds)
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
    /// Resolves a block id to its canonical block number and base canonical state provider.
    pub async fn resolve_block_state(
        &self,
        block_id: BlockId,
    ) -> ProviderResult<(u64, StateProviderBox)> {
        let block_number = self
            .eth_api
            .provider()
            .block_number_for_id(block_id)?
            .ok_or(EthApiError::HeaderNotFound(block_id))
            .map_err(ProviderError::other)?;
        let canonical_state =
            self.eth_api.state_at_block_id(block_id).await.map_err(ProviderError::other)?;
        Ok((block_number, canonical_state))
    }

    /// Wraps a resolved canonical state provider with proof-history state for `block_number`.
    ///
    /// The returned provider serves account and storage reads from proof-history storage while
    /// delegating non-state lookups, such as bytecode and block hashes, to the canonical provider.
    /// An uncovered block near the canonical tip falls back to the canonical provider (its
    /// revert overlay is a few blocks deep), so a lagging sidecar does not block proofs the node
    /// can still serve cheaply; an uncovered block deep in history is rejected instead of
    /// materializing an unbounded revert overlay.
    ///
    /// Opens a proof-history read snapshot and performs trie I/O: call from a blocking context.
    pub fn state_provider_at(
        &self,
        canonical_state: StateProviderBox,
        block_number: u64,
    ) -> ProviderResult<Box<dyn StateProvider + '_>> {
        let provider_ro = self.storage.provider_ro().map_err(ProviderError::from)?;

        let bounds = match provider_ro.get_proof_window() {
            Ok(window) => Some((window.earliest.number, window.latest.number)),
            Err(reth_optimism_trie::OpProofsStorageError::NoBlocksFound) => None,
            Err(err) => return Err(ProviderError::from(err)),
        };

        let reconciled = self.readiness.is_ready();
        if !proof_history_can_serve(reconciled, block_number, bounds) {
            let canonical_tip = self.eth_api.provider().best_block_number()?;
            if !proof_history_fallback_allowed(
                block_number,
                canonical_tip,
                PROOF_HISTORY_CANONICAL_FALLBACK_MAX_DISTANCE,
            ) {
                let distance = canonical_tip.saturating_sub(block_number);
                warn!(
                    target: "reth::taiko::proof_history",
                    block_number,
                    reconciled,
                    ?bounds,
                    distance,
                    "refusing canonical-state fallback for deep-history proof request"
                );
                return Err(ProviderError::other(ProofHistoryDeepHistoryError {
                    block_number,
                    reconciled,
                    bounds,
                    distance,
                }));
            }
            if !reconciled {
                // Storage exists but has not been validated against canonical state yet (startup
                // reconciliation or lag recovery in flight): expected and transient.
                debug!(
                    target: "reth::taiko::proof_history",
                    block_number,
                    ?bounds,
                    "proof-history is awaiting reconciliation; serving from canonical state"
                );
            } else if proof_history_miss_is_pruned(block_number, bounds) {
                // Below the retained window: the block is pruned from proof-history, a genuine gap.
                warn!(
                    target: "reth::taiko::proof_history",
                    block_number,
                    ?bounds,
                    "proof-history has pruned requested block; serving from canonical state"
                );
            } else {
                // Ahead of the cursor (sidecar catching up) or storage not yet initialized:
                // expected and transient, so log at debug to avoid spamming under RPC load.
                debug!(
                    target: "reth::taiko::proof_history",
                    block_number,
                    ?bounds,
                    "proof-history does not yet cover requested block; serving from canonical state"
                );
            }
            return Ok(canonical_state);
        }

        Ok(Box::new(OpProofsStateProviderRef::new(canonical_state, provider_ro, block_number)))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProofHistoryReadiness, proof_history_can_serve, proof_history_covers,
        proof_history_fallback_allowed, proof_history_miss_is_pruned,
    };

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

    #[test]
    fn cannot_serve_covered_block_before_reconciliation() {
        // The stored bounds may describe a branch the canonical chain no longer follows until
        // reconciliation validates them; a covered block must not be served while not ready.
        assert!(!proof_history_can_serve(false, 150, Some((100, 200))));
    }

    #[test]
    fn serves_covered_block_when_reconciled() {
        assert!(proof_history_can_serve(true, 150, Some((100, 200))));
    }

    #[test]
    fn cannot_serve_uncovered_block_even_when_reconciled() {
        assert!(!proof_history_can_serve(true, 250, Some((100, 200))));
        assert!(!proof_history_can_serve(true, 150, None));
    }
}
