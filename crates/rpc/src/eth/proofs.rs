//! Proof-history backed override for `eth_getProof`.

use crate::proof_state::{ProofHistoryStateProviderFactory, flatten_blocking_task};
use alloy_eips::BlockId;
use alloy_primitives::Address;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_serde::JsonStorageKey;
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_optimism_trie::{OpProofsStorage, OpProofsStore};
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Maximum number of concurrent `eth_getProof` computations.
///
/// Each request walks proof-history tries on the blocking pool; the cap keeps a proof storm from
/// starving the pool (stock reth similarly gates `eth_getProof` behind a shared permit).
const MAX_CONCURRENT_PROOF_REQUESTS: usize = 32;

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
    /// Semaphore limiting concurrent proof computations.
    semaphore: Arc<Semaphore>,
}

impl<Eth, Storage> TaikoEthProofExt<Eth, Storage>
where
    Eth: FullEthApi + Send + Sync + 'static,
    Storage: OpProofsStore + Clone + 'static,
{
    /// Creates a new proof-history backed `eth_getProof` override.
    pub fn new(eth_api: Eth, storage: OpProofsStorage<Storage>) -> Self {
        Self {
            state_provider_factory: ProofHistoryStateProviderFactory::new(eth_api, storage),
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_PROOF_REQUESTS)),
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
        let _permit =
            self.semaphore.acquire().await.expect("proof request semaphore is never closed");
        let storage_keys = keys.iter().map(JsonStorageKey::as_b256).collect::<Vec<_>>();
        let factory = self.state_provider_factory.clone();
        let (block_number, canonical_state) = factory
            .resolve_block_state(block_id.unwrap_or_default())
            .await
            .map_err(EthApiError::from)?;

        // The trie walk is synchronous MDBX I/O; keep it off the async RPC workers.
        let proof_task = tokio::task::spawn_blocking(move || {
            let state_provider = factory
                .state_provider_at(canonical_state, block_number)
                .map_err(EthApiError::from)?;
            state_provider
                .proof(Default::default(), address, &storage_keys)
                .map_err(EthApiError::from)
        })
        .await;
        let proof = flatten_blocking_task(proof_task)?;

        Ok(proof.into_eip1186_response(keys))
    }
}
