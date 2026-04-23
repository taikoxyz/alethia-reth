//! `debug_executionWitness` RPC override routed through the proofs-history
//! sidecar.
//!
//! Ported from op-reth's `crates/rpc/src/debug.rs` `execution_witness`
//! handler. Unlike reth's native implementation, which rebuilds historical
//! state by replaying reverts, this override resolves historical state via
//! [`ProofsStateProviderFactory`] — serving the state directly from the
//! sidecar when the requested block is within the retention window.
//!
//! Out-of-window blocks surface a clear RPC error rather than falling back
//! to the native (slow) implementation.
use crate::proofs::state_factory::ProofsStateProviderFactory;
use alloy_consensus::BlockHeader;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_rlp::Encodable;
use alloy_rpc_types_debug::ExecutionWitness;
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use jsonrpsee_types::error::ErrorObject;
use reth_evm::{ConfigureEvm, execute::Executor};
use reth_optimism_trie::OpProofsStore;
use reth_provider::HeaderProvider;
use reth_revm::{database::StateProviderDatabase, db::State, witness::ExecutionWitnessRecord};
use reth_rpc_eth_api::{RpcNodeCore, helpers::FullEthApi};
use reth_rpc_eth_types::EthApiError;
use reth_storage_api::StateProofProvider;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Maximum number of concurrent `debug_executionWitness` invocations.
///
/// Witness generation is CPU-bound and allocates substantial memory; the
/// semaphore caps fan-out so a flood of RPC calls cannot exhaust resources.
const MAX_CONCURRENT_WITNESSES: usize = 3;

/// `debug` namespace override exposing the sidecar-backed
/// `debug_executionWitness` method.
#[cfg_attr(not(test), rpc(server, namespace = "debug"))]
#[cfg_attr(test, rpc(server, client, namespace = "debug"))]
pub trait DebugApiProofsOverride {
    /// Re-executes the given block and returns the resulting execution
    /// witness, including the hashed trie nodes, contract bytecodes, key
    /// preimages, and ancestor headers referenced by `BLOCKHASH` opcodes.
    #[method(name = "executionWitness")]
    async fn execution_witness(&self, block: BlockNumberOrTag) -> RpcResult<ExecutionWitness>;
}

/// `debug` RPC override backed by the proofs-history sidecar.
#[derive(Debug)]
pub struct ProofsDebugApi<Eth, Storage, Provider, EvmConfig> {
    /// Routes historical state lookups through the sidecar.
    factory: ProofsStateProviderFactory<Eth, Storage>,
    /// Node provider used for ancestor-header lookups during witness assembly.
    provider: Provider,
    /// Taiko `eth` API used to resolve the recovered block for execution.
    eth_api: Eth,
    /// EVM configuration used to instantiate the block executor.
    evm_config: EvmConfig,
    /// Caps the number of concurrent witness generations.
    semaphore: Arc<Semaphore>,
}

impl<Eth, Storage, Provider, EvmConfig> ProofsDebugApi<Eth, Storage, Provider, EvmConfig> {
    /// Creates a new [`ProofsDebugApi`].
    pub fn new(
        provider: Provider,
        eth_api: Eth,
        factory: ProofsStateProviderFactory<Eth, Storage>,
        evm_config: EvmConfig,
    ) -> Self {
        Self {
            factory,
            provider,
            eth_api,
            evm_config,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_WITNESSES)),
        }
    }
}

#[async_trait]
impl<Eth, Storage, Provider, EvmConfig> DebugApiProofsOverrideServer
    for ProofsDebugApi<Eth, Storage, Provider, EvmConfig>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    Storage: OpProofsStore + Clone + Send + Sync + 'static,
    Provider: HeaderProvider + Send + Sync + 'static,
    <Provider as HeaderProvider>::Header: Encodable,
    EvmConfig: ConfigureEvm<Primitives = <Eth as RpcNodeCore>::Primitives> + Send + Sync + 'static,
{
    async fn execution_witness(&self, block_id: BlockNumberOrTag) -> RpcResult<ExecutionWitness> {
        // Cap the number of concurrent CPU-bound witness generations.
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|err| ErrorObject::owned(-32603, err.to_string(), None::<()>))?;

        // Resolve the recovered block via the eth API so we use the same
        // decoded form that the rest of the RPC stack sees.
        let block = self
            .eth_api
            .recovered_block(block_id.into())
            .await?
            .ok_or(EthApiError::HeaderNotFound(block_id.into()))?;

        let block_number = block.header().number();
        let parent_number = block.parent_num_hash().number;

        // Obtain the historical state provider at the parent block. The
        // factory propagates `StateForNumberNotFound` / sidecar errors when
        // the block falls outside the retention window so callers receive a
        // deterministic error instead of silently falling back to the
        // (slow) native implementation.
        let state_provider = self
            .factory
            .state_provider(Some(BlockId::Number(parent_number.into())))
            .await
            .map_err(EthApiError::from)?;

        let db = StateProviderDatabase::new(&state_provider);
        let block_executor = self.evm_config.executor(db);

        let mut witness_record = ExecutionWitnessRecord::default();
        let _ = block_executor
            .execute_with_state_closure(block.as_ref(), |statedb: &State<_>| {
                // op-reth v2.0's `record_executed_state` takes only the
                // state DB (v2.1 added an `ExecutionWitnessMode` argument
                // that is not present here).
                witness_record.record_executed_state(statedb);
            })
            .map_err(|err| EthApiError::Internal(err.into()))?;

        let ExecutionWitnessRecord { hashed_state, codes, keys, lowest_block_number } =
            witness_record;

        // v2.0 `witness` signature: `(TrieInput, HashedPostState)` — no
        // `ExecutionWitnessMode` parameter.
        let state =
            state_provider.witness(Default::default(), hashed_state).map_err(EthApiError::from)?;
        let mut exec_witness = ExecutionWitness { state, codes, keys, ..Default::default() };

        // If no `BLOCKHASH` was invoked during execution, include only the
        // parent header; otherwise include all ancestors back to the lowest
        // block number observed during execution.
        let smallest = lowest_block_number.unwrap_or_else(|| block_number.saturating_sub(1));
        let range = smallest..block_number;
        exec_witness.headers = self
            .provider
            .headers_range(range)
            .map_err(EthApiError::from)?
            .into_iter()
            .map(|header| {
                let mut serialized_header = Vec::new();
                header.encode(&mut serialized_header);
                serialized_header.into()
            })
            .collect();

        Ok(exec_witness)
    }
}

#[cfg(test)]
mod tests {
    // End-to-end coverage for `debug_executionWitness` — driving the full
    // [`FullEthApi`] stack, spinning up a sidecar-backed state provider, and
    // asserting the resulting witness — is deferred to Task 9. Unit-testing
    // the handler in isolation would require mocking `EthApiSpec`,
    // `EthTransactions`, `EthBlocks`, `EthState`, `EthCall`, `EthFees`,
    // `Trace`, `LoadReceipt`, `GetBlockAccessList`, `FullEthApiTypes`, plus
    // a full EVM configuration — prohibitively expensive for a unit test.
}
