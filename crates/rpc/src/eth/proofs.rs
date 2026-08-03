//! Proof-history backed override for `eth_getProof`.

use crate::proof_state::{
    ProofHistoryReadiness, ProofHistoryStateProviderFactory, ProofStateProvider,
    acquire_proof_permit, complete_guarded_work, run_proof_task,
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

/// Completes the full account trie walk before validating its canonical-state guard.
fn complete_account_proof<Work, Validate, Proof, Error>(
    work: Work,
    validate: Validate,
) -> Result<Proof, Error>
where
    Work: FnOnce() -> Result<Proof, Error>,
    Validate: FnOnce() -> Result<(), Error>,
{
    complete_guarded_work(work, validate)
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
        // The permit comes first: resolution opens a historical state provider, and a request
        // queued for capacity must never pin that read transaction while it waits.
        let permit = acquire_proof_permit(self.state_provider_factory.eth_api())
            .await
            .map_err(Into::into)?;
        let resolved = factory
            .resolve_block_state(block_id.unwrap_or_default())
            .await
            .map_err(EthApiError::from)?;

        // The trie walk is synchronous MDBX I/O; keep it off the async RPC workers and share
        // Reth's configured proof execution limit with the upstream `eth_getProof` path.
        let proof = run_proof_task(self.state_provider_factory.eth_api(), permit, move || {
            let selected = factory.state_provider_at(resolved).map_err(EthApiError::from)?;
            match selected {
                ProofStateProvider::Pending(state) => state
                    .proof(Default::default(), address, &storage_keys)
                    .map_err(EthApiError::from)
                    .map_err(Into::into),
                ProofStateProvider::Canonical { state, guard } => complete_account_proof(
                    || {
                        state
                            .proof(Default::default(), address, &storage_keys)
                            .map_err(EthApiError::from)
                            .map_err(Into::into)
                    },
                    || {
                        factory
                            .validate_after_work(guard, None)
                            .map_err(EthApiError::from)
                            .map_err(Into::into)
                    },
                ),
            }
        })
        .await
        .map_err(Into::into)?;

        Ok(proof.into_eip1186_response(keys))
    }
}

#[cfg(test)]
mod tests {
    use super::{TaikoEthProofApiServer, TaikoEthProofExt, complete_account_proof};
    use crate::proof_state::ProofHistoryReadiness;
    use alloy_primitives::Address;
    use reth_provider::ProviderError;
    use std::cell::RefCell;

    #[tokio::test]
    async fn get_proof_waits_for_the_shared_permit_before_resolving_state() {
        use reth::{chainspec::ChainSpecProvider, network::noop::NoopNetwork};
        use reth_evm_ethereum::EthEvmConfig;
        use reth_optimism_trie::{MdbxProofsStorageV2, OpProofsStorage};
        use reth_provider::test_utils::MockEthProvider;
        use reth_rpc::EthApiBuilder;
        use reth_rpc_eth_api::helpers::SpawnBlocking;
        use reth_transaction_pool::test_utils::testing_pool;
        use std::{sync::Arc, time::Duration};
        use tokio::time::timeout;

        let provider = MockEthProvider::default();
        let eth_api = EthApiBuilder::new(
            provider.clone(),
            testing_pool(),
            NoopNetwork::default(),
            EthEvmConfig::new(provider.chain_spec()),
        )
        .proof_permits(1)
        .build();
        let path = tempfile::tempdir().expect("proof tempdir").keep();
        let storage: OpProofsStorage<Arc<MdbxProofsStorageV2>> =
            Arc::new(MdbxProofsStorageV2::new(&path).expect("proof storage opens")).into();
        let ext = TaikoEthProofExt::new(eth_api.clone(), storage, ProofHistoryReadiness::new());

        let held = eth_api.acquire_owned_tracing().await.expect("hold the only proof permit");
        // While the pool is exhausted the request must stay queued without touching state;
        // completing here means block resolution ran before the permit was acquired and the
        // queued request would pin a database read transaction while it waits.
        let queued =
            timeout(Duration::from_millis(200), ext.get_proof(Address::ZERO, Vec::new(), None))
                .await;
        assert!(queued.is_err(), "get_proof must acquire the proof permit before resolving state");

        drop(held);
        let _ = timeout(Duration::from_secs(5), ext.get_proof(Address::ZERO, Vec::new(), None))
            .await
            .expect("get_proof completes once the permit frees");
    }

    #[test]
    fn get_proof_postvalidates_after_trie_walk() {
        let calls = RefCell::new(Vec::new());

        let proof = complete_account_proof(
            || {
                calls.borrow_mut().push("trie");
                Ok::<_, ProviderError>(7)
            },
            || {
                calls.borrow_mut().push("validate");
                Ok(())
            },
        )
        .expect("a completed proof validates");
        calls.borrow_mut().push("convert");

        assert_eq!(proof, 7);
        assert_eq!(*calls.borrow(), vec!["trie", "validate", "convert"]);
    }
}
