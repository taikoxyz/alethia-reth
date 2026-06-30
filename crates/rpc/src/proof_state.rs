//! Proof-history backed state provider factory for RPC execution witnesses.

use alloy_eips::BlockId;
use reth_optimism_trie::{
    OpProofsStorage, OpProofsStore, api::OpProofsProviderRO, provider::OpProofsStateProviderRef,
};
use reth_provider::{BlockIdReader, ProviderError, ProviderResult, StateProvider};
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;
use tracing::warn;

/// Returns whether proof-history storage can serve `block_number` given its retained bounds.
///
/// `bounds` is `Some((earliest, latest))` when storage is initialized, `None` when it is empty.
fn proof_history_covers(block_number: u64, bounds: Option<(u64, u64)>) -> bool {
    matches!(bounds, Some((earliest, latest)) if block_number >= earliest && block_number <= latest)
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

impl<'a, Eth, Storage> ProofHistoryStateProviderFactory<Eth, Storage>
where
    Eth: FullEthApi + Send + Sync + 'static,
    Storage: OpProofsStore + Clone + 'a,
{
    /// Creates a state provider for the given canonical block id.
    ///
    /// The returned provider serves account and storage reads from proof-history storage while
    /// delegating non-state lookups, such as bytecode and block hashes, to the canonical provider.
    /// Falls back to the canonical historical state provider (logging a warning) when the requested
    /// block is outside the retained proof-history window, so a lagging sidecar does not block proofs
    /// the node can still serve from canonical state.
    pub async fn state_provider(
        &'a self,
        block_id: BlockId,
    ) -> ProviderResult<Box<dyn StateProvider + 'a>> {
        let block_number = self
            .eth_api
            .provider()
            .block_number_for_id(block_id)?
            .ok_or(EthApiError::HeaderNotFound(block_id))
            .map_err(ProviderError::other)?;

        let historical_provider =
            self.eth_api.state_at_block_id(block_id).await.map_err(ProviderError::other)?;
        let provider_ro = self.storage.provider_ro().map_err(ProviderError::from)?;

        let latest = provider_ro.get_latest_block_number().map_err(ProviderError::from)?;
        let earliest = provider_ro.get_earliest_block_number().map_err(ProviderError::from)?;
        let bounds = latest
            .zip(earliest)
            .map(|((latest_number, _), (earliest_number, _))| (earliest_number, latest_number));

        if !proof_history_covers(block_number, bounds) {
            warn!(
                target: "reth::taiko::proof_history",
                block_number,
                ?bounds,
                "proof-history does not cover requested block; serving from canonical state"
            );
            return Ok(historical_provider);
        }

        Ok(Box::new(OpProofsStateProviderRef::new(historical_provider, provider_ro, block_number)))
    }
}

#[cfg(test)]
mod tests {
    use super::proof_history_covers;

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
}
