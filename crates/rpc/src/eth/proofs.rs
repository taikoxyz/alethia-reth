//! Proof-history backed override for `eth_getProof`.

use crate::proof_state::{
    ProofHistoryReadiness, ProofHistoryStateProviderFactory, ProofStateProvider, run_proof_task,
};
use alloy_eips::BlockId;
use alloy_primitives::Address;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_serde::JsonStorageKey;
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_optimism_trie::{OpProofsStorage, OpProofsStore};
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;

/// RPC server trait for Taiko proof-history backed `eth_getProof`.
#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait TaikoEthProofApi {
    /// Returns account and storage values with Merkle proofs at a block.
    #[method(name = "getProof")]
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block_id: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse>;
}

/// `eth_` namespace override that uses proof-history state for account proofs.
#[derive(Debug, Clone)]
pub struct TaikoEthProofExt<Eth, Storage> {
    /// Factory for sidecar-backed state providers.
    state_provider_factory: ProofHistoryStateProviderFactory<Eth, Storage>,
}

impl<Eth, Storage> TaikoEthProofExt<Eth, Storage>
where
    Eth: FullEthApi + Send + Sync + 'static,
    Storage: OpProofsStore + Clone + 'static,
{
    /// Creates a new proof-history backed `eth_getProof` override.
    pub fn new(
        eth_api: Eth,
        storage: OpProofsStorage<Storage>,
        readiness: ProofHistoryReadiness,
    ) -> Self {
        Self {
            state_provider_factory: ProofHistoryStateProviderFactory::new(
                eth_api, storage, readiness,
            ),
        }
    }
}

#[async_trait]
impl<Eth, Storage> TaikoEthProofApiServer for TaikoEthProofExt<Eth, Storage>
where
    Eth: FullEthApi + Send + Sync + 'static,
    Storage: OpProofsStore + Clone + 'static,
{
    /// Handles `eth_getProof` with proof-history backed state.
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block_id: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse> {
        let storage_keys = keys.iter().map(JsonStorageKey::as_b256).collect::<Vec<_>>();
        let factory = self.state_provider_factory.clone();
        let resolved = factory
            .resolve_block_state(block_id.unwrap_or_default())
            .await
            .map_err(EthApiError::from)?;

        // The trie walk is synchronous MDBX I/O; keep it off the async RPC workers and share
        // Reth's configured proof execution limit with the upstream `eth_getProof` path.
        let proof = run_proof_task(self.state_provider_factory.eth_api(), move || {
            let selected = factory.state_provider_at(resolved).map_err(EthApiError::from)?;
            let state_provider = match selected {
                ProofStateProvider::Pending(state) => state,
                ProofStateProvider::Canonical { state, guard: _guard } => state,
            };
            let proof = state_provider
                .proof(Default::default(), address, &storage_keys)
                .map_err(EthApiError::from)?;
            Ok(proof)
        })
        .await
        .map_err(Into::into)?;

        Ok(proof.into_eip1186_response(keys))
    }
}
