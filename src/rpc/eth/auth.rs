use std::{convert::Infallible, sync::Arc};

use alloy_consensus::BlockHeader;
use alloy_eips::{BlockId, BlockNumberOrTag, Encodable2718};
use alloy_json_rpc::RpcObject;
use alloy_primitives::{Bytes, U256};
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use op_alloy_flz::tx_estimated_size_fjord_bytes;
use reth::{
    primitives::InvalidTransactionError,
    revm::primitives::Address,
    transaction_pool::{
        BestTransactionsAttributes, PoolConsensusTx, PoolTransaction, TransactionPool,
        error::InvalidPoolTransactionError,
    },
};
use reth_db::transaction::DbTx;
use reth_db_api::transaction::DbTxMut;
use reth_ethereum::{EthPrimitives, TransactionSigned};
use reth_evm::{
    ConfigureEvm,
    block::{BlockExecutionError, BlockValidationError},
    execute::BlockBuilder,
};
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_api::{Block, NodePrimitives};
use reth_provider::{BlockReaderIdExt, DBProvider, DatabaseProviderFactory, StateProviderFactory};
use reth_revm::{State, database::StateProviderDatabase};
use reth_rpc_eth_api::{RpcConvert, RpcTransaction};
use reth_rpc_eth_types::EthApiError;
use serde::{Deserialize, Serialize};
use tracing::{info, trace};

use crate::{
    block::{assembler::TaikoBlockAssembler, factory::TaikoBlockExecutorFactory},
    chainspec::spec::TaikoChainSpec,
    evm::{config::TaikoNextBlockEnvAttributes, factory::TaikoEvmFactory},
};

use crate::{
    db::model::{STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1OriginTable},
    payload::attributes::RpcL1Origin,
    rpc::eth::error::TaikoApiError,
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
        Self { tx_list: vec![], estimated_gas_used: 0, bytes_length: 0 }
    }
}

/// trait interface for a custom auth rpc namespace: `taikoAuth`
///
/// This defines the Taiko namespace where all methods are configured as trait functions.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "taikoAuth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "taikoAuth"))]
pub trait TaikoAuthExtApi<T: RpcObject> {
    #[method(name = "setHeadL1Origin")]
    async fn set_head_l1_origin(&self, id: U256) -> RpcResult<u64>;
    #[method(name = "updateL1Origin")]
    async fn update_l1_origin(&self, l1_origin: RpcL1Origin) -> RpcResult<Option<RpcL1Origin>>;
    #[method(name = "setL1OriginSignature")]
    async fn set_l1_origin_signature(&self, id: U256, signature: Bytes) -> RpcResult<RpcL1Origin>;
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

/// A concrete implementation of the `TaikoAuthExtApi` trait.
#[derive(Clone)]
pub struct TaikoAuthExt<Pool, Eth, Evm, Provider: DatabaseProviderFactory> {
    provider: Provider,
    pool: Pool,
    tx_resp_builder: Eth,
    evm_config: Evm,
}

impl<Pool, Eth, Evm, Provider: DatabaseProviderFactory> TaikoAuthExt<Pool, Eth, Evm, Provider> {
    /// Creates a new instance of `TaikoAuthExt` with the given provider.
    pub fn new(provider: Provider, pool: Pool, tx_resp_builder: Eth, evm_config: Evm) -> Self {
        Self { provider, pool, tx_resp_builder, evm_config }
    }
}

#[async_trait]
impl<Pool, Eth, Evm, Provider> TaikoAuthExtApiServer<RpcTransaction<Eth::Network>>
    for TaikoAuthExt<Pool, Eth, Evm, Provider>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>> + 'static,
    Eth: RpcConvert<Primitives: NodePrimitives<SignedTx = PoolConsensusTx<Pool>>> + 'static,
    Provider: DatabaseProviderFactory
        + BlockReaderIdExt<Header = alloy_consensus::Header>
        + StateProviderFactory
        + 'static,
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
    /// Sets the L1 head origin in the database.
    async fn set_head_l1_origin(&self, id: U256) -> RpcResult<u64> {
        let tx = self
            .provider
            .database_provider_rw()
            .map_err(|_| EthApiError::InternalEthError)?
            .into_tx();

        tx.put::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, id.to::<u64>())
            .map_err(|_| EthApiError::InternalEthError)?;

        tx.commit().map_err(|_| EthApiError::InternalEthError)?;

        Ok(id.to())
    }

    /// Sets the L1 origin signature in the database.
    async fn set_l1_origin_signature(&self, id: U256, signature: Bytes) -> RpcResult<RpcL1Origin> {
        let tx = self
            .provider
            .database_provider_rw()
            .map_err(|_| EthApiError::InternalEthError)?
            .into_tx();

        let mut l1_origin = tx
            .get::<StoredL1OriginTable>(id.to())
            .map_err(|_| EthApiError::InternalEthError)?
            .ok_or(TaikoApiError::GethNotFound)?;

        l1_origin.signature = <[u8; 65]>::try_from(signature.as_ref()).map_err(|_| {
            EthApiError::InvalidParams("Signature must be a 65-byte array".to_string())
        })?;

        tx.put::<StoredL1OriginTable>(l1_origin.block_id.to(), l1_origin.clone())
            .map_err(|_| EthApiError::InternalEthError)?;

        tx.commit().map_err(|_| EthApiError::InternalEthError)?;

        Ok(l1_origin.into())
    }

    /// Updates the L1 origin in the database.
    async fn update_l1_origin(&self, l1_origin: RpcL1Origin) -> RpcResult<Option<RpcL1Origin>> {
        let tx = self
            .provider
            .database_provider_rw()
            .map_err(|_| EthApiError::InternalEthError)?
            .into_tx();

        tx.put::<StoredL1OriginTable>(l1_origin.block_id.to(), l1_origin.clone().into())
            .map_err(|_| EthApiError::InternalEthError)?;

        tx.commit().map_err(|_| EthApiError::InternalEthError)?;

        Ok(Some(l1_origin))
    }

    /// Retrieves the transaction pool content with the given limits.
    async fn tx_pool_content(
        &self,
        beneficiary: Address,
        base_fee: u64,
        block_max_gas_limit: u64,
        max_bytes_per_tx_list: u64,
        locals: Option<Vec<Address>>,
        max_transactions_lists: u64,
    ) -> RpcResult<Vec<PreBuiltTxList<RpcTransaction<Eth::Network>>>> {
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
    ) -> RpcResult<Vec<PreBuiltTxList<RpcTransaction<Eth::Network>>>> {
        if max_transactions_lists == 0 {
            return Err(EthApiError::InvalidParams(
                "`maxBytesPerTxList` must not be `0`".to_string(),
            )
            .into());
        }

        // Fetch the parent block and its state, for building the prebuilt transaction lists later.
        let parent_block = self
            .provider
            .block_by_number_or_tag(BlockNumberOrTag::Latest)
            .map_err(|e| EthApiError::Internal(e.into()))?
            .ok_or(EthApiError::HeaderNotFound(BlockId::Number(BlockNumberOrTag::Latest)))?;
        let sealed_parent = parent_block.seal();
        let parent = sealed_parent.sealed_header();

        let state_provider = self.provider.state_by_block_hash(parent.hash()).map_err(|_| {
            EthApiError::EvmCustom("Failed to initialize EVM state provider".to_string())
        })?;
        let mut db = State::builder()
            .with_database(StateProviderDatabase::new(&state_provider))
            .with_bundle_update()
            .build();
        let mut prebuilt_lists = vec![PreBuiltTxList::default()];

        info!(target: "taiko_rpc_payload_builder", ?parent, "Building prebuilt transaction based on the parent block");

        // Create the block builder based on the parent block and the provided attributes.
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
            self.pool.best_transactions_with_attributes(BestTransactionsAttributes {
                basefee: base_fee,
                blob_fee: None,
            });

        info!(target: "taiko_rpc_payload_builder", ?base_fee, ?block_max_gas_limit, ?max_bytes_per_tx_list, ?locals, ?max_transactions_lists, "Building prebuilt transaction lists from the pool");

        // Start iterating over the best transactions in the pool.
        while let Some(pool_tx) = best_txs.next() {
            // ensure if the local accounts are provided, the transaction is from a local account.
            if let Some(local_accounts) = locals.as_ref() {
                if !local_accounts.is_empty() && !local_accounts.contains(&pool_tx.sender()) {
                    // NOTE: we simply mark the transaction as underpriced if it is not from a local
                    // account.
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
                    // NOTE: we simply mark the transaction as underpriced if it is not fitting into
                    // the DA blob.
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
