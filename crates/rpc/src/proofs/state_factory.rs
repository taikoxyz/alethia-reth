//! Routes state-provider requests between op-reth's sidecar and the node's native provider.
//!
//! Given a [`BlockId`], returns a [`StateProvider`] that reads historical state from the
//! op-reth proofs-history sidecar when the block falls inside the retention window, and
//! otherwise surfaces [`ProviderError::StateForNumberNotFound`] so the caller can fall
//! back to the node's native state provider.

use alloy_eips::BlockId;
use jsonrpsee_types::ErrorObject;
use reth_optimism_trie::{
    OpProofsProviderRO, OpProofsStorage, OpProofsStore, provider::OpProofsStateProviderRef,
};
use reth_provider::{BlockIdReader, ProviderError, ProviderResult, StateProvider};
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;

/// Factory that routes `state_provider(BlockId)` requests to op-reth's sidecar
/// when the block is historical and covered by the retention window, or returns
/// [`ProviderError::StateForNumberNotFound`] so the caller can fall back to the
/// node's native state provider.
#[derive(Debug)]
pub struct ProofsStateProviderFactory<Eth, Storage> {
    /// Upstream eth-api; used for block-id resolution and for obtaining the
    /// backing historical [`StateProvider`] (used for block-hash and bytecode
    /// lookups by [`OpProofsStateProviderRef`]).
    eth_api: Eth,
    /// The imported op-reth sidecar storage handle.
    storage: OpProofsStorage<Storage>,
}

impl<Eth, Storage> ProofsStateProviderFactory<Eth, Storage> {
    /// Create a new factory wrapping an eth-api + an op-reth sidecar storage handle.
    pub const fn new(eth_api: Eth, storage: OpProofsStorage<Storage>) -> Self {
        Self { eth_api, storage }
    }
}

impl<Eth, Storage> Clone for ProofsStateProviderFactory<Eth, Storage>
where
    Eth: Clone,
    OpProofsStorage<Storage>: Clone,
{
    fn clone(&self) -> Self {
        Self { eth_api: self.eth_api.clone(), storage: self.storage.clone() }
    }
}

impl<'a, Eth, Storage> ProofsStateProviderFactory<Eth, Storage>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    Storage: OpProofsStore + 'a,
{
    /// Resolve a [`BlockId`] to a state provider. If the block is covered by the
    /// sidecar's retention window, returns a sidecar-backed
    /// [`OpProofsStateProviderRef`]; otherwise returns
    /// [`ProviderError::StateForNumberNotFound`] so the caller can fall back to
    /// the node's native state provider.
    pub async fn state_provider(
        &'a self,
        block_id: Option<BlockId>,
    ) -> ProviderResult<Box<dyn StateProvider + 'a>> {
        let block_id = block_id.unwrap_or_default();
        // Resolve the block number for the requested id.
        let block_number = self
            .eth_api
            .provider()
            .block_number_for_id(block_id)?
            .ok_or(EthApiError::HeaderNotFound(block_id))
            .map_err(ProviderError::other)?;

        // Underlying historical state provider, used by the sidecar-backed
        // provider for block-hash and bytecode lookups.
        let historical_provider =
            self.eth_api.state_at_block_id(block_id).await.map_err(ProviderError::other)?;

        let provider_ro = self.storage.provider_ro().map_err(ProviderError::from)?;

        let (Some((latest_block_number, _)), Some((earliest_block_number, _))) = (
            provider_ro.get_latest_block_number().map_err(ProviderError::from)?,
            provider_ro.get_earliest_block_number().map_err(ProviderError::from)?,
        ) else {
            // Sidecar is empty — no historical coverage.
            return Err(ProviderError::StateForNumberNotFound(block_number));
        };

        if block_number < earliest_block_number || block_number > latest_block_number {
            return Err(ProviderError::StateForNumberNotFound(block_number));
        }

        let sidecar_provider =
            OpProofsStateProviderRef::new(historical_provider, provider_ro, block_number);

        Ok(Box::new(sidecar_provider))
    }
}

#[cfg(test)]
mod tests {
    // End-to-end routing behavior is covered by `tests/proofs_history_e2e.rs`.
    // A standalone unit test here would require mocking the full FullEthApi
    // trait family, which is disproportionately expensive.
}
