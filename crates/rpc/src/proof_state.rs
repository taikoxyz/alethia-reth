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
use tracing::{debug, warn};

/// Maximum distance behind the canonical tip for which an uncovered proof request may fall back
/// to canonical (revert-overlay) state instead of erroring.
///
/// The canonical fallback materializes per-block changesets from the requested block up to the
/// tip, so its cost grows without bound as the request moves into deep history. Near-tip misses
/// (sidecar catch-up lag, persistence gap) stay well within this bound; anything beyond it is
/// rejected instead of risking minutes of CPU and an OOM-sized overlay per request.
const PROOF_HISTORY_CANONICAL_FALLBACK_MAX_DISTANCE: u64 = 1024;

/// A proof request outside the proof-history window and too deep to serve from canonical state.
#[derive(Debug, thiserror::Error)]
#[error(
    "block {block_number} is not covered by proof-history storage (retained bounds: {bounds:?}) \
     and lies {distance} blocks behind the canonical tip; deep-history proofs cannot be served \
     from canonical fallback state"
)]
struct ProofHistoryDeepHistoryError {
    /// Requested block number.
    block_number: u64,
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

/// Creates state providers that overlay OP Proofs history on top of canonical state.
#[derive(Debug, Clone)]
pub struct ProofHistoryStateProviderFactory<Eth, Storage> {
    /// Ethereum RPC API used to resolve block ids and canonical historical state.
    eth_api: Eth,
    /// Proof-history storage containing retained trie nodes and hashed leaves.
    storage: OpProofsStorage<Storage>,
}

impl<Eth, Storage> ProofHistoryStateProviderFactory<Eth, Storage> {
    /// Creates a new proof-history state provider factory.
    pub const fn new(eth_api: Eth, storage: OpProofsStorage<Storage>) -> Self {
        Self { eth_api, storage }
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

        let latest = provider_ro.get_latest_block_number().map_err(ProviderError::from)?;
        let earliest = provider_ro.get_earliest_block_number().map_err(ProviderError::from)?;
        let bounds = latest
            .zip(earliest)
            .map(|((latest_number, _), (earliest_number, _))| (earliest_number, latest_number));

        if !proof_history_covers(block_number, bounds) {
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
                    ?bounds,
                    distance,
                    "refusing canonical-state fallback for deep-history proof request"
                );
                return Err(ProviderError::other(ProofHistoryDeepHistoryError {
                    block_number,
                    bounds,
                    distance,
                }));
            }
            if proof_history_miss_is_pruned(block_number, bounds) {
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
        proof_history_covers, proof_history_fallback_allowed, proof_history_miss_is_pruned,
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
}
