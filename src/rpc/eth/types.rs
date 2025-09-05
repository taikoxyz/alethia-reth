use alloy_network::Ethereum;
use alloy_primitives::Bytes;
use jsonrpsee::tokio;
use reth_evm::TxEnvFor;
use reth_revm::primitives::{B256, U256};
use reth_rpc::{
    EthApi,
    eth::{DevSigner, EthApiTypes, RpcNodeCore},
};
use reth_rpc_api::eth::{
    RpcNodeCoreExt,
    helpers::{
        AddDevSigners, Call, EthCall, EthFees, LoadBlock, LoadFee, LoadPendingBlock, LoadReceipt,
        LoadState, SpawnBlocking, Trace, estimate::EstimateCall,
    },
};
use reth_rpc_eth_api::{
    RpcConvert, SignableTxRequest,
    helpers::{
        EthApiSpec, EthBlocks, EthSigner, EthState, EthTransactions, LoadTransaction,
        pending_block::PendingEnvBuilder, spec::SignersForApi,
    },
    types::RpcTypes,
};
use reth_rpc_eth_types::{
    EthApiError, EthStateCache, FeeHistoryCache, GasPriceOracle, PendingBlock, error::FromEvmError,
    utils::recover_raw_transaction,
};
use reth_storage_api::{ProviderHeader, ProviderTx};
use reth_tasks::{
    TaskSpawner,
    pool::{BlockingTaskGuard, BlockingTaskPool},
};
use reth_transaction_pool::{PoolTransaction, TransactionOrigin, TransactionPool};

/// `Eth` API implementation for Taiko network.
pub struct TaikoEthApi<N: RpcNodeCore, Rpc: RpcConvert>(pub EthApi<N, Rpc>);

impl<N: RpcNodeCore, Rpc: RpcConvert> Clone for TaikoEthApi<N, Rpc> {
    fn clone(&self) -> Self {
        TaikoEthApi(self.0.clone())
    }
}

impl<N, Rpc> EthApiTypes for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    /// Extension of [`FromEthApiError`], with network specific errors.
    type Error = EthApiError;
    /// Blockchain primitive types, specific to network, e.g. block and transaction.
    type NetworkTypes = Ethereum;
    /// Conversion methods for transaction RPC type.
    type RpcConvert = Rpc;

    /// Returns reference to transaction response builder.
    fn tx_resp_builder(&self) -> &Self::RpcConvert {
        self.0.tx_resp_builder()
    }
}

impl<N, Rpc> RpcNodeCore for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    type Primitives = N::Primitives;
    type Provider = N::Provider;
    type Pool = N::Pool;
    type Evm = N::Evm;
    type Network = N::Network;

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

    /// Returns the provider of the node.
    fn provider(&self) -> &Self::Provider {
        self.0.provider()
    }
}

impl<N, Rpc> RpcNodeCoreExt for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    /// Returns handle to RPC cache service.
    #[inline]
    fn cache(&self) -> &EthStateCache<N::Primitives> {
        self.0.cache()
    }
}

impl<N, Rpc> std::fmt::Debug for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaikoEthApi").finish_non_exhaustive()
    }
}

impl<N, Rpc> SpawnBlocking for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
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

impl<N, Rpc> AddDevSigners for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<
        Network: RpcTypes<TransactionRequest: SignableTxRequest<ProviderTx<N::Provider>>>,
    >,
{
    /// Generates 20 random developer accounts.
    /// Used in DEV mode.
    fn with_dev_accounts(&self) {
        *self.0.signers().write() = DevSigner::random_signers(20)
    }
}

impl<N, Rpc> LoadReceipt for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Network = Ethereum>,
{
}

impl<N, Rpc> Trace for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives>,
{
}

impl<N, Rpc> EthFees for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Network = Ethereum>,
{
}

impl<N, Rpc> LoadFee for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Network = Ethereum>,
{
    /// Returns a handle for reading gas price.
    #[inline]
    fn gas_oracle(&self) -> &GasPriceOracle<Self::Provider> {
        self.0.gas_oracle()
    }

    /// Returns a handle for reading fee history data from memory.
    #[inline]
    fn fee_history_cache(&self) -> &FeeHistoryCache<ProviderHeader<N::Provider>> {
        self.0.fee_history_cache()
    }
}

impl<N, Rpc> LoadBlock for TaikoEthApi<N, Rpc>
where
    Self: LoadPendingBlock,
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Network = Ethereum>,
{
}

impl<N, Rpc> EthCall for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<
            Primitives = N::Primitives,
            Error = EthApiError,
            TxEnv = TxEnvFor<N::Evm>,
            Network = Ethereum,
        >,
{
}

impl<N, Rpc> EstimateCall for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, TxEnv = TxEnvFor<N::Evm>>,
{
}

impl<N, Rpc> Call for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, TxEnv = TxEnvFor<N::Evm>>,
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
}

impl<N, Rpc> EthState for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives>,
{
    /// Returns the maximum number of blocks into the past for generating state proofs.
    fn max_proof_window(&self) -> u64 {
        self.0.eth_proof_window()
    }
}

impl<N, Rpc> LoadState for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives>,
{
}

impl<N, Rpc> EthBlocks for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Network = Ethereum>,
{
}

impl<N, Rpc> LoadPendingBlock for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Network = Ethereum>,
{
    /// Returns a handle to the pending block.
    #[inline]
    fn pending_block(&self) -> &tokio::sync::Mutex<Option<PendingBlock<Self::Primitives>>> {
        self.0.pending_block()
    }

    #[inline]
    fn pending_env_builder(&self) -> &dyn PendingEnvBuilder<Self::Evm> {
        self.0.pending_env_builder()
    }
}

impl<N, Rpc> EthTransactions for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Network = Ethereum, Error = EthApiError>,
{
    /// Returns a handle for signing data.
    #[inline]
    fn signers(&self) -> &parking_lot::RwLock<Vec<Box<dyn EthSigner<ProviderTx<Self::Provider>>>>> {
        EthApiSpec::signers(&self.0)
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
        let hash = self.pool().add_transaction(TransactionOrigin::Local, pool_transaction).await?;

        Ok(hash)
    }
}

impl<N, Rpc> LoadTransaction for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Network = Ethereum>,
{
}

impl<N, Rpc> EthApiSpec for TaikoEthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives>,
{
    /// The transaction type signers are using.
    type Transaction = ProviderTx<N::Provider>;
    type Rpc = Rpc::Network;

    /// Returns the block number is started on.
    fn starting_block(&self) -> U256 {
        self.0.starting_block()
    }

    /// Returns a handle to the signers owned by provider.
    fn signers(&self) -> &SignersForApi<Self> {
        self.0.signers()
    }
}
