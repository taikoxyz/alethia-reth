use crate::{TaikoNode, txpool::TaikoPooledTransaction};
use reth_chainspec::EthereumHardforks;
use reth_evm::ConfigureEvm;
use reth_node_api::{FullNodeTypes, PrimitivesTy};
use reth_node_builder::{BuilderContext, components::PoolBuilder};
use reth_provider::{ChainSpecProvider, StateProviderFactory};
use reth_transaction_pool::{
    CoinbaseTipOrdering, PoolConfig, TransactionValidationTaskExecutor,
    blobstore::DiskFileBlobStore,
};
use tracing::{debug, info};

/// A Taiko transaction pool builder compatible with TaikoTxEnvelope.
///
/// This pool builder is similar to EthereumPoolBuilder but works with Taiko's custom
/// transaction envelope type.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct TaikoPoolBuilder {
    /// Pool configuration overrides
    pub pool_config_overrides: Option<PoolConfig>,
}

impl<Node, Evm> PoolBuilder<Node, Evm> for TaikoPoolBuilder
where
    Node: FullNodeTypes<Types = TaikoNode>,
    Node::Provider:
        StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone + 'static,
    Evm: ConfigureEvm<Primitives = PrimitivesTy<Node::Types>> + Clone + 'static,
{
    type Pool = reth_transaction_pool::Pool<
        TransactionValidationTaskExecutor<
            reth_transaction_pool::validate::EthTransactionValidator<
                Node::Provider,
                TaikoPooledTransaction,
                Evm,
            >,
        >,
        CoinbaseTipOrdering<TaikoPooledTransaction>,
        DiskFileBlobStore,
    >;

    async fn build_pool(
        self,
        ctx: &BuilderContext<Node>,
        evm_config: Evm,
    ) -> eyre::Result<Self::Pool> {
        let pool_config = ctx.pool_config();
        let data_dir = ctx.config().datadir();
        let blob_store = DiskFileBlobStore::open(data_dir.blobstore(), Default::default())?;

        let validator =
            TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone(), evm_config)
                .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
                .kzg_settings(ctx.kzg_settings()?)
                .with_local_transactions_config(pool_config.local_transactions_config.clone())
                .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
                .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
                .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
                .build_with_tasks(ctx.task_executor().clone(), blob_store.clone());

        // Spawn KZG settings initialization in background
        if validator.validator().eip4844() {
            let kzg_settings = validator.validator().kzg_settings().clone();
            ctx.task_executor().spawn_blocking_task(async move {
                let _ = kzg_settings.get();
                debug!(target: "reth::cli", "Initialized KZG settings");
            });
        }

        let transaction_pool = reth_transaction_pool::Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            blob_store,
            pool_config.clone(),
        );

        info!(target: "reth::cli", "Taiko transaction pool initialized");
        debug!(target: "reth::cli", "Spawned txpool maintenance task");

        Ok(transaction_pool)
    }
}
