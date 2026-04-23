//! `eth_getProof` RPC override routed through the proofs-history sidecar.
//!
//! Ported from op-reth's `crates/rpc/src/eth/proofs.rs` `get_proof` handler.
//! Unlike reth's native implementation — which walks the node's own state
//! history — this override resolves historical state via
//! [`ProofsStateProviderFactory`] and serves the proof directly from the
//! sidecar when the requested block is within the retention window.
//!
//! Out-of-window blocks surface a clear RPC error rather than silently
//! falling back to the native (slow) implementation.
use crate::proofs::state_factory::ProofsStateProviderFactory;
use alethia_reth_proofs_trie::ProofsStore;
use alloy_eips::BlockId;
use alloy_primitives::Address;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_serde::JsonStorageKey;
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use jsonrpsee_types::error::ErrorObject;
use reth_provider::StateProofProvider;
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;

/// `eth` namespace override exposing the sidecar-backed `eth_getProof`
/// method.
#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EthApiProofsOverride {
    /// Returns the account and storage values of the specified account
    /// including the Merkle proof, routed through the proofs-history
    /// sidecar.
    #[method(name = "getProof")]
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse>;
}

/// `eth` RPC override backed by the proofs-history sidecar.
#[derive(Debug)]
pub struct ProofsEthApi<Eth, Storage> {
    /// Routes historical state lookups through the sidecar.
    factory: ProofsStateProviderFactory<Eth, Storage>,
    /// Taiko `eth` API retained for parity with other proofs overrides; the
    /// factory already holds a reference to the same API internally.
    #[allow(dead_code)]
    eth_api: Eth,
}

impl<Eth, Storage> ProofsEthApi<Eth, Storage> {
    /// Creates a new [`ProofsEthApi`].
    pub const fn new(eth_api: Eth, factory: ProofsStateProviderFactory<Eth, Storage>) -> Self {
        Self { factory, eth_api }
    }
}

#[async_trait]
impl<Eth, Storage> EthApiProofsOverrideServer for ProofsEthApi<Eth, Storage>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    Storage: ProofsStore + Clone + Send + Sync + 'static,
{
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse> {
        // Collect the B256 view of the storage keys once — the trie layer
        // consumes `&[B256]` while the response helper consumes the
        // original `JsonStorageKey`s (so the original casing / formatting
        // is preserved in the RPC response).
        let storage_keys = keys.iter().map(|key| key.as_b256()).collect::<Vec<_>>();

        // Resolve the historical state provider from the sidecar. The
        // factory propagates `StateForNumberNotFound` / sidecar errors when
        // the block falls outside the retention window so callers receive a
        // deterministic error instead of silently falling back to the
        // (slow) native implementation.
        let state_provider = self.factory.state_provider(block).await.map_err(EthApiError::from)?;

        let proof = state_provider
            .proof(Default::default(), address, &storage_keys)
            .map_err(EthApiError::from)?;

        Ok(proof.into_eip1186_response(keys))
    }
}

#[cfg(test)]
mod tests {
    // End-to-end coverage for `eth_getProof` — driving the full
    // [`FullEthApi`] stack against a sidecar-backed state provider and
    // asserting the resulting proof — lives in Task 21. Unit-testing the
    // handler in isolation would require mocking `EthApiSpec`,
    // `EthTransactions`, `EthBlocks`, `EthState`, `EthCall`, `EthFees`,
    // `Trace`, `LoadReceipt`, `GetBlockAccessList`, `FullEthApiTypes` —
    // prohibitively expensive for a unit test.
}
