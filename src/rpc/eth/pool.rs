use std::{convert::Infallible, sync::Arc};

use alloy_consensus::BlockHeader;
use alloy_eips::Encodable2718;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_json_rpc::RpcObject;
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use op_alloy_flz::tx_estimated_size_fjord_bytes;
use reth::{
    primitives::InvalidTransactionError,
    revm::primitives::Address,
    rpc::compat::TransactionCompat,
    transaction_pool::{
        BestTransactionsAttributes, PoolConsensusTx, PoolTransaction, TransactionPool,
        error::InvalidPoolTransactionError,
    },
};
use reth_ethereum::{EthPrimitives, TransactionSigned};
use reth_evm::{
    ConfigureEvm,
    block::{BlockExecutionError, BlockValidationError},
    execute::BlockBuilder,
};
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_api::{Block, NodePrimitives};
use reth_provider::{BlockReaderIdExt, StateProviderFactory};
use reth_revm::State;
use reth_revm::database::StateProviderDatabase;
use reth_rpc_eth_types::EthApiError;
use serde::{Deserialize, Serialize};
use tracing::{info, trace};

use crate::{
    chainspec::spec::TaikoChainSpec,
    factory::{
        assembler::TaikoBlockAssembler, block::TaikoBlockExecutorFactory,
        config::TaikoNextBlockEnvAttributes, factory::TaikoEvmFactory,
    },
};

/// A pre-built transaction list that contains the mempool content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct PreBuiltTxList<T> {
    pub tx_list: Vec<T>,
    pub estimated_gas_used: u64,
    pub bytes_length: u64,
}

impl<T> Default for PreBuiltTxList<T> {
    fn default() -> Self {
        Self {
            tx_list: vec![],
            estimated_gas_used: 0,
            bytes_length: 0,
        }
    }
}

/// Trait interface for a custom auth rpc namespace: `taikoAuth`
///
/// This defines the Taiko namespace where all methods are configured as trait functions.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "taikoAuth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "taikoAuth"))]
pub trait TaikoAuthTxPoolExtApi<T: RpcObject> {
    #[method(name = "txPoolContentWithMinTip")]
    async fn tx_pool_content_with_min_tip(
        &self,
        beneficiary: Address,
        base_fee: u64,
        block_max_gas_limit: u64,
        max_bytes_per_tx_list: u64,
        locals: Option<Vec<Address>>,
        max_transactions_lists: u64,
        min_tip: u64,
    ) -> RpcResult<Vec<PreBuiltTxList<T>>>;

    #[method(name = "txPoolContent")]
    async fn tx_pool_content(
        &self,
        beneficiary: Address,
        base_fee: u64,
        block_max_gas_limit: u64,
        max_bytes_per_tx_list: u64,
        locals: Option<Vec<Address>>,
        max_transactions_lists: u64,
    ) -> RpcResult<Vec<PreBuiltTxList<T>>>;
}

/// A concrete implementation of the `TaikoAuthTxPoolExtApi` trait.
#[derive(Clone)]
pub struct TaikoAuthTxPoolExt<Pool, Eth, Evm, Provider: BlockReaderIdExt + StateProviderFactory> {
    pool: Pool,
    tx_resp_builder: Eth,
    provider: Provider,
    evm_config: Evm,
}

impl<Pool, Eth, Evm, Provider: BlockReaderIdExt + StateProviderFactory>
    TaikoAuthTxPoolExt<Pool, Eth, Evm, Provider>
{
    /// Creates a new instance of `TaikoAuthTxPoolExt` with the given pool and transaction response builder.
    pub fn new(pool: Pool, tx_resp_builder: Eth, provider: Provider, evm_config: Evm) -> Self {
        Self {
            pool,
            tx_resp_builder,
            provider,
            evm_config,
        }
    }
}

#[async_trait]
impl<Pool, Eth, Evm, Provider> TaikoAuthTxPoolExtApiServer<Eth::Transaction>
    for TaikoAuthTxPoolExt<Pool, Eth, Evm, Provider>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>> + 'static,
    Eth: TransactionCompat<Primitives: NodePrimitives<SignedTx = PoolConsensusTx<Pool>>> + 'static,
    Provider: BlockReaderIdExt<Header = alloy_consensus::Header> + StateProviderFactory + 'static,
    Evm: ConfigureEvm<
            Primitives = EthPrimitives,
            Error = Infallible,
            NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
            BlockExecutorFactory = TaikoBlockExecutorFactory<
                RethReceiptBuilder,
                Arc<TaikoChainSpec>,
                TaikoEvmFactory,
            >,
            BlockAssembler = TaikoBlockAssembler,
        > + 'static,
{
    /// Retrieves the transaction pool content with the given limits.
    async fn tx_pool_content(
        &self,
        beneficiary: Address,
        base_fee: u64,
        block_max_gas_limit: u64,
        max_bytes_per_tx_list: u64,
        locals: Option<Vec<Address>>,
        max_transactions_lists: u64,
    ) -> RpcResult<Vec<PreBuiltTxList<Eth::Transaction>>> {
        self.tx_pool_content_with_min_tip(
            beneficiary,
            base_fee,
            block_max_gas_limit,
            max_bytes_per_tx_list,
            locals,
            max_transactions_lists,
            0,
        )
        .await
    }

    /// Retrieves the transaction pool content with the given limits and minimum tip.
    async fn tx_pool_content_with_min_tip(
        &self,
        beneficiary: Address,
        base_fee: u64,
        block_max_gas_limit: u64,
        max_bytes_per_tx_list: u64,
        locals: Option<Vec<Address>>,
        max_transactions_lists: u64,
        min_tip: u64,
    ) -> RpcResult<Vec<PreBuiltTxList<Eth::Transaction>>> {
        if max_transactions_lists == 0 {
            return Err(EthApiError::InvalidParams(
                "`maxBytesPerTxList` must not be `0`".to_string(),
            )
            .into());
        }

        let parent_block = self
            .provider
            .block_by_number_or_tag(BlockNumberOrTag::Latest)
            .map_err(|e| EthApiError::Internal(e.into()))?
            .ok_or(EthApiError::HeaderNotFound(BlockId::Number(
                BlockNumberOrTag::Latest,
            )))?;
        let sealed_parent = parent_block.seal();
        let parent = sealed_parent.sealed_header();

        let state_provider = self.provider.state_by_block_hash(parent.hash()).unwrap();
        let mut db = State::builder()
            .with_database(StateProviderDatabase::new(&state_provider))
            .with_bundle_update()
            .build();
        let mut prebuilt_lists = vec![PreBuiltTxList::default()];

        info!(target: "taiko_rpc_payload_builder", ?parent, "Building prebuilt transaction based on the parent block");

        let mut builder = self
            .evm_config
            .builder_for_next_block(
                &mut db,
                parent,
                TaikoNextBlockEnvAttributes {
                    timestamp: parent.timestamp(),
                    suggested_fee_recipient: beneficiary,
                    prev_randao: parent.mix_hash().unwrap_or_default(),
                    gas_limit: block_max_gas_limit * max_transactions_lists,
                    extra_data: parent.extra_data().clone(),
                    base_fee_per_gas: base_fee,
                },
            )
            .map_err(|_| {
                EthApiError::EvmCustom("failed to create block builder from EVM config".to_string())
            })?;

        let mut best_txs =
            self.pool
                .best_transactions_with_attributes(BestTransactionsAttributes {
                    basefee: base_fee,
                    blob_fee: None,
                });

        info!(target: "taiko_rpc_payload_builder", ?base_fee, ?block_max_gas_limit, ?max_bytes_per_tx_list, ?locals, ?max_transactions_lists, "Building prebuilt transaction lists from the pool");

        while let Some(pool_tx) = best_txs.next() {
            // ensure if the local accounts are provided, the transaction is from a local account.
            if let Some(local_accounts) = locals.as_ref() {
                if !local_accounts.is_empty() && !local_accounts.contains(&pool_tx.sender()) {
                    // NOTE: we simply mark the transaction as underpriced if it is not from a local account.
                    best_txs.mark_invalid(&pool_tx, InvalidPoolTransactionError::Underpriced);
                    continue;
                }
            }
            // ensure we only pick the transactions that meet the minimum tip.
            if pool_tx.effective_tip_per_gas(base_fee).is_none()
                || pool_tx.effective_tip_per_gas(base_fee).unwrap_or_default() < min_tip as u128
            {
                // skip transactions that do not meet the minimum tip requirement
                trace!(target: "taiko_rpc_payload_builder", ?pool_tx, "skipping transaction with insufficient tip");
                best_txs.mark_invalid(&pool_tx, InvalidPoolTransactionError::Underpriced);
                continue;
            }
            // ensure we still have capacity for this transaction
            if prebuilt_lists.last().unwrap().estimated_gas_used + pool_tx.gas_limit()
                > block_max_gas_limit
            {
                // we can't fit this transaction into the block, so we need to mark it as invalid
                // which also removes all dependent transaction from the iterator before we can
                // continue
                if prebuilt_lists.len() == max_transactions_lists as usize {
                    best_txs.mark_invalid(
                        &pool_tx,
                        InvalidPoolTransactionError::ExceedsGasLimit(
                            pool_tx.gas_limit(),
                            block_max_gas_limit,
                        ),
                    );
                    continue;
                }

                prebuilt_lists.push(PreBuiltTxList::default());
            }

            // convert tx to a signed transaction
            let tx = pool_tx.to_consensus();
            let estimated_compressed_size = tx_estimated_size_fjord_bytes(&tx.encoded_2718());

            if prebuilt_lists.last().unwrap().bytes_length + estimated_compressed_size
                > max_bytes_per_tx_list
            {
                if prebuilt_lists.len() == max_transactions_lists as usize {
                    // NOTE: we simply mark the transaction as underpriced if it is not fitting into the DA blob.
                    best_txs.mark_invalid(&pool_tx, InvalidPoolTransactionError::Underpriced);
                    continue;
                }
                prebuilt_lists.push(PreBuiltTxList::default());
            }

            let gas_used = match builder.execute_transaction(tx.clone()) {
                Ok(gas_used) => gas_used,
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) => {
                    if error.is_nonce_too_low() {
                        // if the nonce is too low, we can skip this transaction
                        trace!(target: "taiko_rpc_payload_builder", %error, ?tx, "skipping nonce too low transaction");
                    } else {
                        // if the transaction is invalid, we can skip it and all of its
                        // descendants
                        trace!(target: "taiko_rpc_payload_builder", %error, ?tx, "skipping invalid transaction and its descendants");
                        best_txs.mark_invalid(
                            &pool_tx,
                            InvalidPoolTransactionError::Consensus(
                                InvalidTransactionError::TxTypeNotSupported,
                            ),
                        );
                    }
                    continue;
                }
                // this is an error that we should treat as fatal for this attempt
                Err(err) => {
                    return Err(EthApiError::Internal(err.into()).into());
                }
            };

            prebuilt_lists.last_mut().unwrap().estimated_gas_used += gas_used;
            prebuilt_lists.last_mut().unwrap().bytes_length += estimated_compressed_size;
            prebuilt_lists.last_mut().unwrap().tx_list.push(
                self.tx_resp_builder
                    .fill_pending(tx)
                    .map_err(|e| EthApiError::Other(Box::new(e.into())))?,
            );
        }

        Ok(prebuilt_lists)
    }
}
