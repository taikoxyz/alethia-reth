//! Proof-history backed overrides for selected `debug_` RPC methods.

use crate::proof_state::ProofHistoryStateProviderFactory;
use alethia_reth_block::{
    config::{TaikoNextBlockEnvAttributes, attributes_from_derived_block},
    executor::is_zk_gas_limit_exceeded,
};
use alethia_reth_primitives::payload::builder::decode_transactions;
use alloy_consensus::{BlockHeader, transaction::Recovered};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, B256, Bytes};
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
use reth_primitives_traits::{RecoveredBlock, SealedHeader, SignedTransaction};
use reth_provider::HeaderProvider;
use reth_revm::{
    State, database::StateProviderDatabase, db::states::bundle_state::BundleRetention,
    witness::ExecutionWitnessRecord,
};
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

    /// Replays an explicit transaction list on top of the requested block's parent state and
    /// returns the generated execution witness.
    ///
    /// The list is executed as the `parent + 1` block (matching the prover's derived-block
    /// environment), so the resulting witness reflects a stateless re-execution of the supplied
    /// transactions rather than the canonical block's transactions.
    #[method(name = "executionWitnessForTxList")]
    async fn execution_witness_for_tx_list(
        &self,
        block: BlockId,
        tx_list: Bytes,
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
    Eth: FullEthApi<Primitives = EthPrimitives> + Send + Sync + 'static,
    Eth::Evm: ConfigureEvm<NextBlockEnvCtx = TaikoNextBlockEnvAttributes>,
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
    ) -> Result<ExecutionWitness, Eth::Error> {
        let block = self
            .eth_api
            .recovered_block(block_id)
            .await?
            .ok_or(EthApiError::HeaderNotFound(block_id))?;
        let txs = decode_recovered_tx_list(tx_list)?;
        let block = block_with_tx_list(block.as_ref().clone(), txs);
        self.execution_witness_for_tx_list_block(&block, mode).await
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
    ) -> Result<ExecutionWitness, Eth::Error> {
        let block_number = block.header().number();
        let parent_block = BlockId::Hash(block.parent_hash().into());

        // Execute the replayed list as the `parent + 1` block, mirroring the prover's derived-block
        // environment (`difficulty = 0`, no committed `expected_difficulty`) rather than the
        // canonical block's environment. Reusing the canonical block env would expose the original
        // block's zk-gas difficulty through the `DIFFICULTY` opcode and force a difficulty mismatch
        // for any list other than the original one. Both async loads happen before the non-`Send`
        // execution state is built so the RPC future stays `Send`.
        let parent = self
            .eth_api
            .recovered_block(parent_block)
            .await?
            .ok_or(EthApiError::HeaderNotFound(parent_block))?;
        let parent_header = SealedHeader::new(parent.header().clone(), block.parent_hash());
        let attributes = attributes_from_derived_block(block).map_err(EthApiError::from)?;

        let state_provider = self
            .state_provider_factory
            .state_provider(parent_block)
            .await
            .map_err(EthApiError::from)?;
        let db = StateProviderDatabase::new(&*state_provider);
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let mut witness_record = ExecutionWitnessRecord::default();

        {
            let evm_config = self.eth_api.evm_config();
            let evm_env = evm_config
                .next_evm_env(parent_header.header(), &attributes)
                .map_err(|err| EthApiError::EvmCustom(err.to_string()))?;
            let evm = evm_config.evm_with_env(&mut state, evm_env);
            let execution_ctx = evm_config
                .context_for_next_block(&parent_header, attributes)
                .map_err(|err| EthApiError::EvmCustom(err.to_string()))?;
            let mut block_executor = evm_config.create_executor(evm, execution_ctx);

            block_executor.apply_pre_execution_changes().map_err(EthApiError::from)?;

            for (idx, tx) in block.transactions_recovered().enumerate() {
                let is_anchor_transaction = idx == 0;
                // Non-anchor transactions whose signer could not be recovered are kept as
                // zero-signer entries by the decoder to preserve list ordering; skip them exactly
                // as the prover's tx-list filtering does.
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

            block_executor.apply_post_execution_changes().map_err(EthApiError::from)?;
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
    Eth::Evm: ConfigureEvm<NextBlockEnvCtx = TaikoNextBlockEnvAttributes>,
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
    ) -> RpcResult<ExecutionWitness> {
        let _permit = self.semaphore.acquire().await;
        self.execution_witness_for_tx_list_for_id(block, tx_list, mode.unwrap_or_default())
            .await
            .map_err(Into::into)
    }
}

/// Decode an RLP transaction list, recovering each signer for EVM execution.
///
/// Mirrors the canonical Taiko tx-list handling ([`decode_transactions`]): the list is decoded as a
/// whole, and transactions whose signer cannot be recovered are kept as zero-signer entries rather
/// than failing the entire request. Keeping them preserves list ordering (the anchor stays at index
/// 0) and lets the executor skip them, matching the prover's tx-list filtering.
fn decode_recovered_tx_list(
    tx_list: Bytes,
) -> Result<Vec<Recovered<TransactionSigned>>, EthApiError> {
    let txs = decode_transactions(tx_list.as_ref())
        .map_err(|err| EthApiError::EvmCustom(format!("failed to decode tx list: {err}")))?;
    Ok(txs
        .into_iter()
        .map(|tx| {
            let signer = tx.try_recover().unwrap_or(Address::ZERO);
            Recovered::new_unchecked(tx, signer)
        })
        .collect())
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
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_primitives::{Bytes, Signature, TxKind, U256};
    use alloy_rlp::Encodable;

    #[test]
    fn decode_recovered_tx_list_accepts_empty_rlp_list() {
        let txs = decode_recovered_tx_list(Bytes::from_static(&[0xc0])).unwrap();

        assert!(txs.is_empty());
    }

    #[test]
    fn decode_recovered_tx_list_keeps_unrecoverable_tx_as_zero_signer() {
        // A signature with `r = 0` cannot be recovered, but it must not fail the whole list.
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let signature = Signature::new(U256::ZERO, U256::from(1u64), false);
        let signed: TransactionSigned = Signed::new_unchecked(tx, signature, B256::ZERO).into();
        let mut encoded = Vec::new();
        vec![signed].encode(&mut encoded);

        let decoded = decode_recovered_tx_list(Bytes::from(encoded))
            .expect("list must decode despite bad sig");

        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].signer(), Address::ZERO);
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
}
