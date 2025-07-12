use std::{convert::Infallible, sync::Arc};

use alloy_consensus::BlockHeader;
use alloy_eips::BlockId;
use alloy_hardforks::EthereumHardforks;
use alloy_network::Ethereum;
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::{TransactionReceipt, TransactionRequest};
use alloy_signer::Either;
use jsonrpsee::tokio;
use reth::{
    chainspec::EthChainSpec,
    network::NetworkInfo,
    revm::{
        context::{Block, TxEnv},
        primitives::{B256, TxKind, U256},
    },
    rpc::api::eth::{
        FullEthApiTypes, RpcNodeCoreExt, RpcReceipt,
        helpers::{
            AddDevSigners, Call, EthCall, EthFees, LoadBlock, LoadFee, LoadPendingBlock,
            LoadReceipt, LoadState, SpawnBlocking, Trace, estimate::EstimateCall,
        },
    },
    tasks::{
        TaskSpawner,
        pool::{BlockingTaskGuard, BlockingTaskPool},
    },
    transaction_pool::{PoolTransaction, TransactionPool},
};
use reth_ethereum::TransactionSigned;
use reth_ethereum_primitives::Receipt;
use reth_evm::{ConfigureEvm, EvmEnv, EvmFactory, SpecFor, block::BlockExecutorFactory};
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_api::NodePrimitives;
use reth_primitives_traits::{BlockBody, SealedHeader, TransactionMeta};
use reth_provider::{
    BlockNumReader, BlockReader, BlockReaderIdExt, ChainSpecProvider, NodePrimitivesProvider,
    ProviderBlock, ProviderHeader, ProviderReceipt, ProviderTx, ReceiptProvider,
    StageCheckpointReader, StateProviderFactory, TransactionsProvider,
};
use reth_rpc::{
    EthApi,
    eth::{DevSigner, EthApiTypes, EthTxBuilder, RpcNodeCore},
};
use reth_rpc_eth_api::{
    FromEthApiError, IntoEthApiError,
    helpers::{EthApiSpec, EthBlocks, EthSigner, EthState, EthTransactions, LoadTransaction},
    types::RpcTypes,
};
use reth_rpc_eth_types::{
    EthApiError, EthReceiptBuilder, EthStateCache, FeeHistoryCache, GasPriceOracle, PendingBlock,
    RpcInvalidTransactionError, error::FromEvmError, revm_utils::CallFees,
    utils::recover_raw_transaction,
};
use revm_database_interface::Database;

use crate::{
    block::{assembler::TaikoBlockAssembler, factory::TaikoBlockExecutorFactory},
    chainspec::spec::TaikoChainSpec,
    evm::{config::TaikoNextBlockEnvAttributes, factory::TaikoEvmFactory},
};

/// `Eth` API implementation for Taiko network.
pub struct TaikoEthApi<Provider: BlockReader, Pool, Network, EvmConfig>(
    pub EthApi<Provider, Pool, Network, EvmConfig>,
);

impl<Provider, Pool, Network, EvmConfig> Clone for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Provider: BlockReader,
{
    fn clone(&self) -> Self {
        TaikoEthApi(self.0.clone())
    }
}

impl<Provider, Pool, Network, EvmConfig> EthApiTypes
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: Send + Sync,
    Provider: BlockReader,
{
    /// Extension of [`FromEthApiError`], with network specific errors.
    type Error = EthApiError;
    /// Blockchain primitive types, specific to network, e.g. block and transaction.
    type NetworkTypes = Ethereum;
    /// Conversion methods for transaction RPC type.
    type TransactionCompat = EthTxBuilder;

    /// Returns reference to transaction response builder.
    fn tx_resp_builder(&self) -> &Self::TransactionCompat {
        &self.0.tx_resp_builder
    }
}

impl<Provider, Pool, Network, EvmConfig> RpcNodeCore
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Provider: BlockReader + NodePrimitivesProvider + Clone + Unpin,
    Pool: Send + Sync + Clone + Unpin,
    Network: Send + Sync + Clone,
    EvmConfig: Send + Sync + Clone + Unpin,
{
    /// Blockchain data primitives.
    type Primitives = Provider::Primitives;
    /// The provider type used to interact with the node.
    type Provider = Provider;
    /// The transaction pool of the node.
    type Pool = Pool;
    /// The node's EVM configuration.
    type Evm = EvmConfig;
    /// Network API.
    type Network = Network;
    /// Builder for new blocks.
    type PayloadBuilder = ();

    /// Returns the transaction pool of the node.
    fn pool(&self) -> &Self::Pool {
        self.0.pool()
    }

    /// Returns the node's evm config.
    fn evm_config(&self) -> &Self::Evm {
        self.0.evm_config()
    }

    /// Returns the handle to the network
    fn network(&self) -> &Self::Network {
        self.0.network()
    }

    /// Returns the handle to the payload builder service.
    fn payload_builder(&self) -> &Self::PayloadBuilder {
        &()
    }

    /// Returns the provider of the node.
    fn provider(&self) -> &Self::Provider {
        self.0.provider()
    }
}

impl<Provider, Pool, Network, EvmConfig> RpcNodeCoreExt
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Provider: BlockReader + NodePrimitivesProvider + Clone + Unpin,
    Pool: Send + Sync + Clone + Unpin,
    Network: Send + Sync + Clone,
    EvmConfig: Send + Sync + Clone + Unpin,
{
    /// Returns handle to RPC cache service.
    #[inline]
    fn cache(&self) -> &EthStateCache<ProviderBlock<Provider>, ProviderReceipt<Provider>> {
        self.0.cache()
    }
}

impl<Provider, Pool, Network, EvmConfig> std::fmt::Debug
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Provider: BlockReader,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaikoEthApi").finish_non_exhaustive()
    }
}

impl<Provider, Pool, Network, EvmConfig> SpawnBlocking
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: Clone + Send + Sync + 'static,
    Provider: BlockReader,
{
    /// Returns a handle for spawning IO heavy blocking tasks.
    ///
    /// Runtime access in default trait method implementations.
    #[inline]
    fn io_task_spawner(&self) -> impl TaskSpawner {
        self.0.task_spawner()
    }

    /// Returns a handle for spawning CPU heavy blocking tasks.
    ///
    /// Thread pool access in default trait method implementations.
    #[inline]
    fn tracing_task_pool(&self) -> &BlockingTaskPool {
        self.0.blocking_task_pool()
    }

    /// Returns handle to semaphore for pool of CPU heavy blocking tasks.
    #[inline]
    fn tracing_task_guard(&self) -> &BlockingTaskGuard {
        self.0.blocking_task_guard()
    }
}

impl<Provider, Pool, Network, EvmConfig> AddDevSigners
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Provider: BlockReader,
{
    /// Generates 20 random developer accounts.
    /// Used in DEV mode.
    fn with_dev_accounts(&self) {
        *self.0.signers().write() = DevSigner::random_signers(20)
    }
}

impl<Provider, Pool, Network, EvmConfig> LoadReceipt
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: RpcNodeCoreExt<
        Provider: TransactionsProvider<Transaction = TransactionSigned>
                      + ReceiptProvider<Receipt = reth_ethereum_primitives::Receipt>,
    >,
    Provider: BlockReader + ChainSpecProvider,
{
    /// Helper method for `eth_getBlockReceipts` and `eth_getTransactionReceipt`.
    async fn build_transaction_receipt(
        &self,
        tx: TransactionSigned,
        meta: TransactionMeta,
        receipt: Receipt,
    ) -> Result<RpcReceipt<Self::NetworkTypes>, Self::Error> {
        let hash = meta.block_hash;
        // get all receipts for the block
        let all_receipts = self
            .cache()
            .get_receipts(hash)
            .await
            .map_err(Self::Error::from_eth_err)?
            .ok_or(EthApiError::HeaderNotFound(hash.into()))?;

        Ok(EthReceiptBuilder::new(&tx, meta, &receipt, &all_receipts, None)?.build())
    }
}

impl<Provider, Pool, Network, EvmConfig> Trace for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: LoadState<
            Provider: BlockReader,
            Evm: ConfigureEvm<
                Primitives: NodePrimitives<
                    BlockHeader = ProviderHeader<Self::Provider>,
                    SignedTx = ProviderTx<Self::Provider>,
                >,
                Error = Infallible,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
                BlockExecutorFactory = TaikoBlockExecutorFactory<
                    RethReceiptBuilder,
                    Arc<TaikoChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
            Error: FromEvmError<Self::Evm>,
        >,
    Provider: BlockReader,
{
}

impl<Provider, Pool, Network, EvmConfig> EthFees for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: LoadFee,
    Provider: BlockReader,
{
}

impl<Provider, Pool, Network, EvmConfig> LoadFee for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: LoadBlock<Provider = Provider>,
    Provider: BlockReaderIdExt
        + ChainSpecProvider<ChainSpec: EthChainSpec + EthereumHardforks>
        + StateProviderFactory,
{
    /// Returns a handle for reading gas price.
    #[inline]
    fn gas_oracle(&self) -> &GasPriceOracle<Self::Provider> {
        self.0.gas_oracle()
    }

    /// Returns a handle for reading fee history data from memory.
    #[inline]
    fn fee_history_cache(&self) -> &FeeHistoryCache {
        self.0.fee_history_cache()
    }
}

impl<Provider, Pool, Network, EvmConfig> LoadBlock
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: LoadPendingBlock
        + SpawnBlocking
        + RpcNodeCoreExt<
            Pool: TransactionPool<
                Transaction: PoolTransaction<Consensus = ProviderTx<Self::Provider>>,
            >,
            Primitives: NodePrimitives<SignedTx = ProviderTx<Self::Provider>>,
            Evm = EvmConfig,
        >,
    Provider: BlockReader,
    EvmConfig: ConfigureEvm<Primitives = <Self as RpcNodeCore>::Primitives>,
{
}

impl<Provider, Pool, Network, EvmConfig> EthCall for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: EstimateCall
        + LoadPendingBlock
        + FullEthApiTypes
        + RpcNodeCoreExt<
            Pool: TransactionPool<
                Transaction: PoolTransaction<Consensus = ProviderTx<Self::Provider>>,
            >,
            Primitives: NodePrimitives<SignedTx = ProviderTx<Self::Provider>>,
            Evm = EvmConfig,
        >,
    EvmConfig: ConfigureEvm<Primitives = <Self as RpcNodeCore>::Primitives>,
    Provider: BlockReader,
{
}

impl<Provider, Pool, Network, EvmConfig> EstimateCall
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: Call,
    Provider: BlockReader,
{
}

impl<Provider, Pool, Network, EvmConfig> Call for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: LoadState<
            Evm: ConfigureEvm<
                BlockExecutorFactory: BlockExecutorFactory<EvmFactory: EvmFactory<Tx = TxEnv>>,
                Primitives: NodePrimitives<
                    BlockHeader = ProviderHeader<Self::Provider>,
                    SignedTx = ProviderTx<Self::Provider>,
                >,
            >,
            Error: FromEvmError<Self::Evm>,
        > + SpawnBlocking,
    Provider: BlockReader,
{
    /// Returns default gas limit to use for `eth_call` and tracing RPC methods.
    ///
    /// Data access in default trait method implementations.
    #[inline]
    fn call_gas_limit(&self) -> u64 {
        self.0.gas_cap()
    }

    /// Returns the maximum number of blocks accepted for `eth_simulateV1`.
    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.0.max_simulate_blocks()
    }

    /// Configures a new `TxEnv`  for the [`TransactionRequest`]
    ///
    /// All `TxEnv` fields are derived from the given [`TransactionRequest`], if fields are
    /// `None`, they fall back to the [`EvmEnv`]'s settings.
    fn create_txn_env(
        &self,
        evm_env: &EvmEnv<SpecFor<Self::Evm>>,
        request: TransactionRequest,
        mut db: impl Database<Error: Into<EthApiError>>,
    ) -> Result<TxEnv, Self::Error> {
        // Ensure that if versioned hashes are set, they're not empty
        if request
            .blob_versioned_hashes
            .as_ref()
            .is_some_and(|hashes| hashes.is_empty())
        {
            return Err(RpcInvalidTransactionError::BlobTransactionMissingBlobHashes.into_eth_err());
        }

        let tx_type = request.minimal_tx_type() as u8;

        let TransactionRequest {
            from,
            to,
            gas_price,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            gas,
            value,
            input,
            nonce,
            access_list,
            chain_id,
            blob_versioned_hashes,
            max_fee_per_blob_gas,
            authorization_list,
            transaction_type: _,
            sidecar: _,
        } = request;

        let CallFees {
            max_priority_fee_per_gas,
            gas_price,
            max_fee_per_blob_gas,
        } = CallFees::ensure_fees(
            gas_price.map(U256::from),
            max_fee_per_gas.map(U256::from),
            max_priority_fee_per_gas.map(U256::from),
            U256::from(evm_env.block_env.basefee),
            blob_versioned_hashes.as_deref(),
            max_fee_per_blob_gas.map(U256::from),
            evm_env.block_env.blob_gasprice().map(U256::from),
        )?;

        let gas_limit = gas.unwrap_or(
            // Use maximum allowed gas limit. The reason for this
            // is that both Erigon and Geth use pre-configured gas cap even if
            // it's possible to derive the gas limit from the block:
            // <https://github.com/ledgerwatch/erigon/blob/eae2d9a79cb70dbe30b3a6b79c436872e4605458/cmd/rpcdaemon/commands/trace_adhoc.go#L956
            // https://github.com/ledgerwatch/erigon/blob/eae2d9a79cb70dbe30b3a6b79c436872e4605458/eth/ethconfig/config.go#L94>
            evm_env.block_env.gas_limit,
        );

        let chain_id = chain_id.unwrap_or(evm_env.cfg_env.chain_id);

        let caller = from.unwrap_or_default();

        let nonce = if let Some(nonce) = nonce {
            nonce
        } else {
            db.basic(caller)
                .map_err(Into::into)?
                .map(|acc| acc.nonce)
                .unwrap_or_default()
        };

        let env = TxEnv {
            tx_type,
            gas_limit,
            nonce,
            caller,
            gas_price: gas_price.saturating_to(),
            gas_priority_fee: max_priority_fee_per_gas.map(|v| v.saturating_to()),
            kind: to.unwrap_or(TxKind::Create),
            value: value.unwrap_or_default(),
            data: input
                .try_into_unique_input()
                .map_err(Self::Error::from_eth_err)?
                .unwrap_or_default(),
            chain_id: Some(chain_id),
            access_list: access_list.unwrap_or_default(),
            // EIP-4844 fields
            blob_hashes: blob_versioned_hashes.unwrap_or_default(),
            max_fee_per_blob_gas: max_fee_per_blob_gas
                .map(|v| v.saturating_to())
                .unwrap_or_default(),
            // EIP-7702 fields
            authorization_list: authorization_list
                .unwrap_or_default()
                .into_iter()
                .map(Either::Left)
                .collect(),
        };

        Ok(env)
    }
}

impl<Provider, Pool, Network, EvmConfig> EthState
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: LoadState + SpawnBlocking,
    Provider: BlockReader,
{
    /// Returns the maximum number of blocks into the past for generating state proofs.
    fn max_proof_window(&self) -> u64 {
        self.0.eth_proof_window()
    }
}

impl<Provider, Pool, Network, EvmConfig> LoadState
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: RpcNodeCoreExt<
            Provider: BlockReader
                          + StateProviderFactory
                          + ChainSpecProvider<ChainSpec: EthereumHardforks>,
            Pool: TransactionPool,
        >,
    Provider: BlockReader,
{
}

impl<Provider, Pool, Network, EvmConfig> EthBlocks
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: LoadBlock<
            Error = EthApiError,
            NetworkTypes: RpcTypes<Receipt = TransactionReceipt>,
            Provider: BlockReader<
                Transaction = reth_ethereum_primitives::TransactionSigned,
                Receipt = reth_ethereum_primitives::Receipt,
            >,
        >,
    Provider: BlockReader + ChainSpecProvider,
{
    /// Helper function for `eth_getBlockReceipts`.
    ///
    /// Returns all transaction receipts in block, or `None` if block wasn't found.
    async fn block_receipts(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Vec<RpcReceipt<Self::NetworkTypes>>>, Self::Error>
    where
        Self: LoadReceipt,
    {
        if let Some((block, receipts)) = self.load_block_and_receipts(block_id).await? {
            let block_number = block.number();
            let base_fee = block.base_fee_per_gas();
            let block_hash = block.hash();
            let excess_blob_gas = block.excess_blob_gas();
            let timestamp = block.timestamp();
            let blob_params = self
                .provider()
                .chain_spec()
                .blob_params_at_timestamp(timestamp);

            return block
                .body()
                .transactions()
                .iter()
                .zip(receipts.iter())
                .enumerate()
                .map(|(idx, (tx, receipt))| {
                    let meta = TransactionMeta {
                        tx_hash: *tx.tx_hash(),
                        index: idx as u64,
                        block_hash,
                        block_number,
                        base_fee,
                        excess_blob_gas,
                        timestamp,
                    };
                    EthReceiptBuilder::new(tx, meta, receipt, &receipts, blob_params)
                        .map(|builder| builder.build())
                })
                .collect::<Result<Vec<_>, Self::Error>>()
                .map(Some);
        }

        Ok(None)
    }
}

impl<Provider, Pool, Network, EvmConfig> LoadPendingBlock
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: SpawnBlocking<
            NetworkTypes: RpcTypes<Header = alloy_rpc_types_eth::Header>,
            Error: FromEvmError<Self::Evm>,
        > + RpcNodeCore<
            Provider: BlockReaderIdExt<
                Transaction = reth_ethereum_primitives::TransactionSigned,
                Block = reth_ethereum_primitives::Block,
                Receipt = reth_ethereum_primitives::Receipt,
                Header = alloy_consensus::Header,
            > + ChainSpecProvider<ChainSpec: EthChainSpec + EthereumHardforks>
                          + StateProviderFactory,
            Pool: TransactionPool<
                Transaction: PoolTransaction<Consensus = ProviderTx<Self::Provider>>,
            >,
            Evm: ConfigureEvm<
                Primitives = <Self as RpcNodeCore>::Primitives,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
            >,
            Primitives: NodePrimitives<
                BlockHeader = ProviderHeader<Self::Provider>,
                SignedTx = ProviderTx<Self::Provider>,
                Receipt = ProviderReceipt<Self::Provider>,
                Block = ProviderBlock<Self::Provider>,
            >,
        >,
    Provider: BlockReader<
            Block = reth_ethereum_primitives::Block,
            Receipt = reth_ethereum_primitives::Receipt,
        >,
{
    /// Returns a handle to the pending block.
    #[inline]
    fn pending_block(
        &self,
    ) -> &tokio::sync::Mutex<
        Option<PendingBlock<ProviderBlock<Self::Provider>, ProviderReceipt<Self::Provider>>>,
    > {
        self.0.pending_block()
    }

    /// Returns [`ConfigureEvm::NextBlockEnvCtx`] for building a local pending block.
    fn next_env_attributes(
        &self,
        parent: &SealedHeader<ProviderHeader<Self::Provider>>,
    ) -> Result<<Self::Evm as reth_evm::ConfigureEvm>::NextBlockEnvCtx, Self::Error> {
        Ok(TaikoNextBlockEnvAttributes {
            timestamp: parent.timestamp().saturating_add(12),
            suggested_fee_recipient: parent.beneficiary(),
            prev_randao: B256::random(),
            gas_limit: parent.gas_limit(),
            base_fee_per_gas: parent.base_fee_per_gas().ok_or(EthApiError::InvalidParams(
                "invalid parent base_fee_per_gas".to_string(),
            ))?,
            extra_data: parent.extra_data().clone().into(),
        })
    }
}

impl<Provider, Pool, Network, EvmConfig> EthTransactions
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: LoadTransaction<Provider: BlockReaderIdExt>,
    Provider: BlockReader<Transaction = ProviderTx<Self::Provider>>,
{
    /// Returns a handle for signing data.
    #[inline]
    fn signers(&self) -> &parking_lot::RwLock<Vec<Box<dyn EthSigner<ProviderTx<Self::Provider>>>>> {
        self.0.signers()
    }

    /// Decodes and recovers the transaction and submits it to the pool.
    ///
    /// Returns the hash of the transaction.
    async fn send_raw_transaction(&self, tx: Bytes) -> Result<B256, Self::Error> {
        let recovered = recover_raw_transaction(&tx)?;

        // broadcast raw transaction to subscribers if there is any.
        self.0.broadcast_raw_transaction(tx);

        let pool_transaction = <Self::Pool as TransactionPool>::Transaction::from_pooled(recovered);

        // submit the transaction to the pool with a `Local` origin
        let hash = self
            .pool()
            .add_transaction(
                reth::transaction_pool::TransactionOrigin::Local,
                pool_transaction,
            )
            .await
            .map_err(Self::Error::from_eth_err)?;

        Ok(hash)
    }
}

impl<Provider, Pool, Network, EvmConfig> LoadTransaction
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: SpawnBlocking
        + FullEthApiTypes
        + RpcNodeCoreExt<Provider: TransactionsProvider, Pool: TransactionPool>,
    Provider: BlockReader,
{
}

impl<Provider, Pool, Network, EvmConfig> EthApiSpec
    for TaikoEthApi<Provider, Pool, Network, EvmConfig>
where
    Self: RpcNodeCore<
            Provider: ChainSpecProvider<ChainSpec: EthereumHardforks>
                          + BlockNumReader
                          + StageCheckpointReader,
            Network: NetworkInfo,
        >,
    Provider: BlockReader,
{
    /// The transaction type signers are using.
    type Transaction = ProviderTx<Provider>;

    /// Returns the block number is started on.
    fn starting_block(&self) -> U256 {
        self.0.starting_block()
    }

    /// Returns a handle to the signers owned by provider.
    fn signers(
        &self,
    ) -> &parking_lot::RwLock<Vec<Box<dyn reth_rpc_eth_api::helpers::EthSigner<Self::Transaction>>>>
    {
        self.0.signers()
    }
}
