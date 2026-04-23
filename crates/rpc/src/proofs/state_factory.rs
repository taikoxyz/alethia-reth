//! State provider factory for the Taiko proofs-history sidecar.
//!
//! Given a [`BlockId`], returns a [`StateProvider`] that reads historical
//! state from the sidecar when the block falls inside the retention window,
//! and otherwise surfaces [`ProviderError::StateForNumberNotFound`] so callers
//! fall back to the node's native state provider.
//!
//! Ported from op-reth's `crates/rpc/src/state.rs`; only the bounds and
//! imports differ.

use alethia_reth_proofs_trie::{
    ProofsProviderRO, ProofsStateProviderRef, ProofsStorage, ProofsStore,
};
use alloy_eips::BlockId;
use jsonrpsee_types::error::ErrorObject;
use reth_provider::{BlockIdReader, ProviderError, ProviderResult, StateProvider};
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;

/// Creates a factory for state providers backed by the proofs-history sidecar.
///
/// The factory routes a [`BlockId`] either to a sidecar-backed
/// [`ProofsStateProviderRef`] (when the block is inside the retention window)
/// or returns [`ProviderError::StateForNumberNotFound`] so the caller can fall
/// back to the node's native state provider.
#[derive(Debug)]
pub struct ProofsStateProviderFactory<Eth, Storage> {
    /// Taiko-flavored `eth` API used to resolve block ids and obtain the
    /// backing historical [`StateProvider`] (used for block-hash and bytecode
    /// lookups by [`ProofsStateProviderRef`]).
    eth_api: Eth,
    /// Sidecar storage wrapped with metrics recording. Provides the versioned
    /// account / storage / trie state for the requested block number.
    preimage_store: ProofsStorage<Storage>,
}

impl<Eth, Storage> ProofsStateProviderFactory<Eth, Storage> {
    /// Creates a new [`ProofsStateProviderFactory`].
    pub const fn new(eth_api: Eth, preimage_store: ProofsStorage<Storage>) -> Self {
        Self { eth_api, preimage_store }
    }
}

impl<'a, Eth, Storage> ProofsStateProviderFactory<Eth, Storage>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    Storage: ProofsStore + Clone + 'a,
{
    /// Returns a [`StateProvider`] for the given [`BlockId`].
    ///
    /// When the resolved block number lies inside the sidecar's retention
    /// window, this returns a [`ProofsStateProviderRef`] that serves trie /
    /// account / storage reads from the sidecar. Otherwise a
    /// [`ProviderError::StateForNumberNotFound`] is returned so the caller
    /// can fall back to the node's native state provider.
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

        let provider_ro = self.preimage_store.provider_ro().map_err(ProviderError::from)?;

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

        let external_overlay_provider =
            ProofsStateProviderRef::new(historical_provider, provider_ro, block_number);

        Ok(Box::new(external_overlay_provider))
    }
}

#[cfg(test)]
mod tests {
    // Unit-testing the routing logic would require mocking the full
    // `FullEthApi` surface (`EthApiSpec + EthTransactions + EthBlocks +
    // EthState + EthCall + EthFees + Trace + LoadReceipt +
    // GetBlockAccessList + FullEthApiTypes`), which is prohibitively
    // expensive for a unit test. The routing behavior — sidecar path for
    // in-window blocks, `StateForNumberNotFound` for empty / out-of-window
    // cases — is covered by the end-to-end test in Task 21.
}
