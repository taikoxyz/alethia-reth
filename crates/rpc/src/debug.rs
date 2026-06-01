//! Proof-history backed overrides for selected `debug_` RPC methods.

use crate::proof_state::ProofHistoryStateProviderFactory;
use alethia_reth_block::executor::{is_zk_gas_difficulty_mismatch, is_zk_gas_limit_exceeded};
use alloy_consensus::{BlockHeader, transaction::Recovered};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, B256, Bytes};
use alloy_rlp::Decodable;
use alloy_rpc_types_debug::ExecutionWitness;
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_ethereum::{EthPrimitives, TransactionSigned};
use reth_ethereum_primitives::Block;
use reth_evm::{
    ConfigureEvm,
    execute::{BlockExecutionError, BlockExecutor, BlockValidationError, Executor},
};
use reth_optimism_trie::{OpProofsStorage, OpProofsStore};
use reth_primitives_traits::RecoveredBlock;
use reth_provider::HeaderProvider;
use reth_revm::{
    State, database::StateProviderDatabase, db::states::bundle_state::BundleRetention,
    witness::ExecutionWitnessRecord,
};
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;
use reth_trie_common::ExecutionWitnessMode;
use serde::{Deserialize, Serialize};
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

    /// Replays an explicit transaction list on top of the requested block's parent state and
    /// returns the generated execution witness.
    #[method(name = "executionWitnessForTxList")]
    async fn execution_witness_for_tx_list(
        &self,
        block: BlockId,
        tx_list: Bytes,
        mode: Option<ExecutionWitnessMode>,
        options: Option<TxListWitnessOptions>,
    ) -> RpcResult<ExecutionWitness>;
}

/// Options for `debug_executionWitnessForTxList`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxListWitnessOptions {
    /// Skip matching recomputed zk gas against `header.difficulty`.
    ///
    /// This is only useful for debug experiments that replay non-canonical transaction lists on
    /// top of a canonical parent state.
    #[serde(default)]
    pub skip_zk_gas_difficulty_check: bool,
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
    Eth: FullEthApi<Primitives = EthPrimitives> + Send + Sync + 'static,
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

    /// Re-executes the requested canonical block and returns the generated witness.
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
        self.execution_witness_for_block(block.as_ref(), mode).await
    }

    /// Replays the explicit transaction list on top of the requested block's parent state.
    async fn execution_witness_for_tx_list_for_id(
        &self,
        block_id: BlockId,
        tx_list: Bytes,
        mode: ExecutionWitnessMode,
        options: TxListWitnessOptions,
    ) -> Result<ExecutionWitness, Eth::Error> {
        let block = self
            .eth_api
            .recovered_block(block_id)
            .await?
            .ok_or(EthApiError::HeaderNotFound(block_id))?;
        let txs = decode_recovered_tx_list(tx_list)?;
        let block = block_with_tx_list(block.as_ref().clone(), txs);
        self.execution_witness_for_tx_list_block(&block, mode, options).await
    }

    /// Re-executes the provided block against proof-history backed parent state and returns the
    /// generated witness.
    async fn execution_witness_for_block(
        &self,
        block: &RecoveredBlock<Block>,
        mode: ExecutionWitnessMode,
    ) -> Result<ExecutionWitness, Eth::Error> {
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
            .execute_with_state_closure(block, |statedb: &State<_>| {
                witness_record.record_executed_state(statedb, mode);
            })
            .map_err(EthApiError::from)?;

        Ok(witness_record
            .into_execution_witness(&*state_provider, &self.provider, block_number, mode)
            .map_err(EthApiError::from)?)
    }

    /// Replays the explicit transaction-list block with prover-style filtering, without enabling
    /// the block crate's global `prover` feature for normal node execution.
    async fn execution_witness_for_tx_list_block(
        &self,
        block: &RecoveredBlock<Block>,
        mode: ExecutionWitnessMode,
        options: TxListWitnessOptions,
    ) -> Result<ExecutionWitness, Eth::Error> {
        let block_number = block.header().number();
        let parent_block = BlockId::Hash(block.parent_hash().into());
        let state_provider = self
            .state_provider_factory
            .state_provider(parent_block)
            .await
            .map_err(EthApiError::from)?;
        let db = StateProviderDatabase::new(&*state_provider);
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let mut witness_record = ExecutionWitnessRecord::default();

        {
            let mut block_executor = self
                .eth_api
                .evm_config()
                .executor_for_block(&mut state, block.sealed_block())
                .map_err(|err| EthApiError::EvmCustom(err.to_string()))?;

            block_executor.apply_pre_execution_changes().map_err(EthApiError::from)?;

            for (idx, tx) in block.transactions_recovered().enumerate() {
                let is_anchor_transaction = idx == 0;
                if !is_anchor_transaction && tx.signer() == Address::ZERO {
                    continue;
                }

                match block_executor.execute_transaction(tx) {
                    Ok(_) => {}
                    Err(err) if is_recoverable_tx_list_error(&err, is_anchor_transaction) => {
                        if is_zk_gas_limit_exceeded(&err) {
                            break;
                        }
                    }
                    Err(err) => return Err(EthApiError::from(err).into()),
                }
            }

            match block_executor.apply_post_execution_changes() {
                Ok(_) => {}
                Err(err)
                    if options.skip_zk_gas_difficulty_check &&
                        is_zk_gas_difficulty_mismatch(&err) => {}
                Err(err) => return Err(EthApiError::from(err).into()),
            }
        }

        state.merge_transitions(BundleRetention::Reverts);
        witness_record.record_executed_state(&state, mode);

        Ok(witness_record
            .into_execution_witness(&*state_provider, &self.provider, block_number, mode)
            .map_err(EthApiError::from)?)
    }
}

#[async_trait]
impl<Eth, Storage, Provider> TaikoDebugWitnessApiServer
    for TaikoDebugWitnessExt<Eth, Storage, Provider>
where
    Eth: FullEthApi<Primitives = EthPrimitives> + Send + Sync + 'static,
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

    /// Handles `debug_executionWitnessForTxList` with proof-history backed parent state.
    async fn execution_witness_for_tx_list(
        &self,
        block: BlockId,
        tx_list: Bytes,
        mode: Option<ExecutionWitnessMode>,
        options: Option<TxListWitnessOptions>,
    ) -> RpcResult<ExecutionWitness> {
        let _permit = self.semaphore.acquire().await;
        self.execution_witness_for_tx_list_for_id(
            block,
            tx_list,
            mode.unwrap_or_default(),
            options.unwrap_or_default(),
        )
        .await
        .map_err(Into::into)
    }
}

/// Decode an RLP transaction list and recover each transaction signer for EVM execution.
fn decode_recovered_tx_list(
    tx_list: Bytes,
) -> Result<Vec<Recovered<TransactionSigned>>, EthApiError> {
    let mut tx_list = tx_list.as_ref();
    Vec::<Recovered<TransactionSigned>>::decode(&mut tx_list)
        .map_err(|err| EthApiError::EvmCustom(format!("failed to decode tx list: {err}")))
}

/// Replace a canonical block's transactions with the explicit replay transaction list.
fn block_with_tx_list(
    block: RecoveredBlock<Block>,
    txs: Vec<Recovered<TransactionSigned>>,
) -> RecoveredBlock<Block> {
    let block_hash = block.hash();
    let mut block = block.into_block();
    let mut senders = Vec::with_capacity(txs.len());
    let mut transactions = Vec::with_capacity(txs.len());

    for tx in txs {
        let (tx, sender) = tx.into_parts();
        transactions.push(tx);
        senders.push(sender);
    }

    block.body.transactions = transactions;
    RecoveredBlock::new(block, senders, block_hash)
}

/// Return whether a transaction-list replay error should be tolerated for prover witness parity.
fn is_recoverable_tx_list_error(err: &BlockExecutionError, is_anchor_transaction: bool) -> bool {
    if is_anchor_transaction {
        return false;
    }

    is_zk_gas_limit_exceeded(err) ||
        matches!(
            err,
            BlockExecutionError::Validation(
                BlockValidationError::InvalidTx { .. } |
                    BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. }
            )
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use serde_json::json;

    #[test]
    fn decode_recovered_tx_list_accepts_empty_rlp_list() {
        let txs = decode_recovered_tx_list(Bytes::from_static(&[0xc0])).unwrap();

        assert!(txs.is_empty());
    }

    #[test]
    fn tx_list_replay_recovers_non_anchor_block_gas_errors() {
        let err = BlockExecutionError::Validation(
            BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: 2,
                block_available_gas: 1,
            },
        );

        assert!(is_recoverable_tx_list_error(&err, false));
        assert!(!is_recoverable_tx_list_error(&err, true));
    }

    #[test]
    fn tx_list_witness_options_validate_difficulty_by_default() {
        let options = TxListWitnessOptions::default();

        assert!(!options.skip_zk_gas_difficulty_check);
    }

    #[test]
    fn tx_list_witness_options_accept_camel_case_skip_flag() {
        let options: TxListWitnessOptions =
            serde_json::from_value(json!({ "skipZkGasDifficultyCheck": true })).unwrap();

        assert!(options.skip_zk_gas_difficulty_check);
    }
}
