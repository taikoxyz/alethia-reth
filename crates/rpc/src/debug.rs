//! Proof-history backed overrides for selected `debug_` RPC methods.

use crate::proof_state::ProofHistoryStateProviderFactory;
use alloy_consensus::BlockHeader;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::B256;
use alloy_rpc_types_debug::ExecutionWitness;
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_evm::{ConfigureEvm, execute::Executor};
use reth_optimism_trie::{OpProofsStorage, OpProofsStore};
use reth_provider::HeaderProvider;
use reth_revm::{State, database::StateProviderDatabase, witness::ExecutionWitnessRecord};
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;
use reth_trie_common::ExecutionWitnessMode;
use tokio::sync::Semaphore;

/// Maximum number of concurrent proof-history witness requests.
const MAX_CONCURRENT_WITNESS_REQUESTS: usize = 3;

/// RPC server trait for Taiko proof-history backed `debug_` witness methods.
#[cfg_attr(not(test), rpc(server, namespace = "debug"))]
#[cfg_attr(test, rpc(server, client, namespace = "debug"))]
pub trait TaikoDebugWitnessApi {
    /// Returns an execution witness for a canonical block number or tag.
    #[method(name = "executionWitness")]
    async fn execution_witness(
        &self,
        block: BlockNumberOrTag,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness>;

    /// Returns an execution witness for a block hash.
    #[method(name = "executionWitnessByBlockHash")]
    async fn execution_witness_by_block_hash(
        &self,
        hash: B256,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness>;
}

/// `debug_` namespace overrides that use proof-history state for witness generation.
#[derive(Debug)]
pub struct TaikoDebugWitnessExt<Eth, Storage, Provider> {
    /// Provider used to fetch ancestor block headers for the returned witness.
    provider: Provider,
    /// Ethereum RPC API used to load blocks, state, and the EVM configuration.
    eth_api: Eth,
    /// Factory for sidecar-backed state providers.
    state_provider_factory: ProofHistoryStateProviderFactory<Eth, Storage>,
    /// Semaphore limiting concurrent witness generation.
    semaphore: Semaphore,
}

impl<Eth, Storage, Provider> TaikoDebugWitnessExt<Eth, Storage, Provider>
where
    Eth: FullEthApi + Send + Sync + 'static,
    Storage: OpProofsStore + Clone + 'static,
    Provider: HeaderProvider + Clone + Send + Sync + 'static,
    Provider::Header: BlockHeader + alloy_rlp::Encodable,
{
    /// Creates a new proof-history backed debug witness override.
    pub fn new(provider: Provider, eth_api: Eth, storage: OpProofsStorage<Storage>) -> Self {
        Self {
            provider,
            state_provider_factory: ProofHistoryStateProviderFactory::new(eth_api.clone(), storage),
            eth_api,
            semaphore: Semaphore::new(MAX_CONCURRENT_WITNESS_REQUESTS),
        }
    }

    /// Re-executes the requested block and returns the generated witness.
    async fn execution_witness_for_id(
        &self,
        block_id: BlockId,
        mode: ExecutionWitnessMode,
    ) -> Result<ExecutionWitness, Eth::Error> {
        let block = self
            .eth_api
            .recovered_block(block_id)
            .await?
            .ok_or(EthApiError::HeaderNotFound(block_id))?;
        let block_number = block.header().number();
        let parent_block = BlockId::Hash(block.parent_hash().into());
        let state_provider = self
            .state_provider_factory
            .state_provider(parent_block)
            .await
            .map_err(EthApiError::from)?;
        let db = StateProviderDatabase::new(&*state_provider);
        let block_executor = self.eth_api.evm_config().executor(db);
        let mut witness_record = ExecutionWitnessRecord::default();

        block_executor
            .execute_with_state_closure(&block, |statedb: &State<_>| {
                witness_record.record_executed_state(statedb, mode);
            })
            .map_err(EthApiError::from)?;

        Ok(witness_record
            .into_execution_witness(&*state_provider, &self.provider, block_number, mode)
            .map_err(EthApiError::from)?)
    }
}

#[async_trait]
impl<Eth, Storage, Provider> TaikoDebugWitnessApiServer
    for TaikoDebugWitnessExt<Eth, Storage, Provider>
where
    Eth: FullEthApi + Send + Sync + 'static,
    Storage: OpProofsStore + Clone + 'static,
    Provider: HeaderProvider + Clone + Send + Sync + 'static,
    Provider::Header: BlockHeader + alloy_rlp::Encodable,
{
    /// Handles `debug_executionWitness` with proof-history backed state.
    async fn execution_witness(
        &self,
        block: BlockNumberOrTag,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness> {
        let _permit = self.semaphore.acquire().await;
        self.execution_witness_for_id(block.into(), mode.unwrap_or_default())
            .await
            .map_err(Into::into)
    }

    /// Handles `debug_executionWitnessByBlockHash` with proof-history backed state.
    async fn execution_witness_by_block_hash(
        &self,
        hash: B256,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness> {
        let _permit = self.semaphore.acquire().await;
        self.execution_witness_for_id(hash.into(), mode.unwrap_or_default())
            .await
            .map_err(Into::into)
    }
}
