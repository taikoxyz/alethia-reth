#![allow(clippy::too_many_arguments)]
use std::{convert::Infallible, sync::Arc};

use alloy_consensus::BlockHeader;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_json_rpc::RpcObject;
use alloy_primitives::{Bytes, U256};
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth::{
    revm::primitives::Address,
    transaction_pool::{PoolConsensusTx, PoolTransaction, TransactionPool},
};
use reth_db::transaction::DbTx;
use reth_db_api::transaction::DbTxMut;
use reth_ethereum::{EthPrimitives, TransactionSigned};
use reth_evm::ConfigureEngineEvm;
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_api::{Block, NodePrimitives};
use reth_provider::{BlockReaderIdExt, DBProvider, DatabaseProviderFactory, StateProviderFactory};
use reth_revm::{State, database::StateProviderDatabase};
use reth_rpc_eth_api::{RpcConvert, RpcTransaction};
use reth_rpc_eth_types::EthApiError;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::eth::error::TaikoApiError;
use alethia_reth_block::{
    assembler::TaikoBlockAssembler,
    config::TaikoNextBlockEnvAttributes,
    factory::TaikoBlockExecutorFactory,
    tx_selection::{
        DEFAULT_DA_ZLIB_GUARD_BYTES, SelectionOutcome, TxSelectionConfig,
        select_and_execute_pool_transactions,
    },
};
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_db::model::{
    BatchToLastBlock, STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1OriginTable,
};
use alethia_reth_evm::factory::TaikoEvmFactory;
use alethia_reth_primitives::{
    engine::types::TaikoExecutionData, payload::attributes::RpcL1Origin,
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
    async fn set_head_l1_origin(&self, id: U256) -> RpcResult<U256>;
    #[method(name = "updateL1Origin")]
    async fn update_l1_origin(&self, l1_origin: RpcL1Origin) -> RpcResult<Option<RpcL1Origin>>;
    #[method(name = "setL1OriginSignature")]
    async fn set_l1_origin_signature(&self, id: U256, signature: Bytes) -> RpcResult<RpcL1Origin>;
    #[method(name = "setBatchToLastBlock")]
    async fn set_batch_to_last_block(&self, batch_id: U256, block_number: U256) -> RpcResult<u64>;
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
    Evm: ConfigureEngineEvm<
            TaikoExecutionData,
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
    async fn set_head_l1_origin(&self, id: U256) -> RpcResult<U256> {
        let tx = self
            .provider
            .database_provider_rw()
            .map_err(|_| EthApiError::InternalEthError)?
            .into_tx();

        tx.put::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, id.to::<u64>())
            .map_err(|_| EthApiError::InternalEthError)?;

        tx.commit().map_err(|_| EthApiError::InternalEthError)?;

        Ok(id)
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

        Ok(l1_origin.into_rpc())
    }

    /// Sets the mapping from batch ID to its last block number in the database.
    async fn set_batch_to_last_block(&self, batch_id: U256, block_number: U256) -> RpcResult<u64> {
        let tx = self
            .provider
            .database_provider_rw()
            .map_err(|_| EthApiError::InternalEthError)?
            .into_tx();
        tx.put::<BatchToLastBlock>(batch_id.to(), block_number.to())
            .map_err(|_| EthApiError::InternalEthError)?;
        tx.commit().map_err(|_| EthApiError::InternalEthError)?;
        Ok(batch_id.to())
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
                "`maxTransactionsLists` must not be `0`".to_string(),
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

        info!(target: "taiko_rpc_payload_builder", ?base_fee, ?block_max_gas_limit, ?max_bytes_per_tx_list, ?locals, ?max_transactions_lists, "Building prebuilt transaction lists from the pool");

        // Use the shared transaction selection logic
        let config = TxSelectionConfig {
            base_fee,
            gas_limit_per_list: block_max_gas_limit,
            max_da_bytes_per_list: max_bytes_per_tx_list,
            da_size_zlib_guard_bytes: DEFAULT_DA_ZLIB_GUARD_BYTES,
            max_lists: max_transactions_lists as usize,
            min_tip,
            locals: locals.unwrap_or_default(),
        };

        match select_and_execute_pool_transactions(&mut builder, &self.pool, &config, || false) {
            Ok(SelectionOutcome::Completed(lists)) => {
                // Convert ExecutedTxList to PreBuiltTxList with RPC transactions
                lists
                    .into_iter()
                    .map(|list| {
                        let tx_list = list
                            .transactions
                            .into_iter()
                            .map(|etx| {
                                self.tx_resp_builder
                                    .fill_pending(etx.tx)
                                    .map_err(|e| EthApiError::Other(Box::new(e.into())))
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        Ok(PreBuiltTxList {
                            tx_list,
                            estimated_gas_used: list.total_gas_used,
                            bytes_length: list.total_da_bytes,
                        })
                    })
                    .collect::<Result<Vec<_>, EthApiError>>()
                    .map_err(Into::into)
            }
            Ok(SelectionOutcome::Cancelled) => {
                // Should never happen since we pass || false
                unreachable!("tx selection should never be cancelled")
            }
            Err(err) => Err(EthApiError::Internal(err.into()).into()),
        }
    }
}
