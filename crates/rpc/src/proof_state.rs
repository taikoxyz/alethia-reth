//! Proof-history backed state provider factory for RPC execution witnesses.

use alloy_eips::BlockId;
use reth_optimism_trie::{
    OpProofsStorage, OpProofsStore, api::OpProofsProviderRO, provider::OpProofsStateProviderRef,
};
use reth_provider::{BlockIdReader, ProviderError, ProviderResult, StateProvider};
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;

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
    /// Returns [`ProviderError::StateForNumberNotFound`] when the requested block is outside the
    /// retained proof-history window.
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

        let (Some((latest_block_number, _)), Some((earliest_block_number, _))) = (
            provider_ro.get_latest_block_number().map_err(ProviderError::from)?,
            provider_ro.get_earliest_block_number().map_err(ProviderError::from)?,
        ) else {
            return Err(ProviderError::StateForNumberNotFound(block_number));
        };

        if block_number < earliest_block_number || block_number > latest_block_number {
            return Err(ProviderError::StateForNumberNotFound(block_number));
        }

        Ok(Box::new(OpProofsStateProviderRef::new(historical_provider, provider_ro, block_number)))
    }
}
