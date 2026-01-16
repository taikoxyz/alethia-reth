use reth::{
    api::{FullNodeTypes, NodeTypes},
    builder::{BuilderContext, components::PayloadBuilderBuilder},
    transaction_pool::{PoolTransaction, TransactionPool},
};
use reth_ethereum::{EthPrimitives, TransactionSigned};

use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_primitives::engine::TaikoEngineTypes;

pub mod builder;
pub use builder::TaikoPayloadBuilder;

/// The builder to spawn [`TaikoPayloadBuilder`] payload building tasks.
#[derive(Debug, Default, Clone)]
pub struct TaikoPayloadBuilderBuilder;

impl<Node, Pool> PayloadBuilderBuilder<Node, Pool, TaikoEvmConfig> for TaikoPayloadBuilderBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = TaikoChainSpec,
            Payload = TaikoEngineTypes,
        >,
    >,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>
        + Unpin
        + 'static,
{
    /// Payload builder implementation.
    type PayloadBuilder = TaikoPayloadBuilder<Node::Provider, Pool, TaikoEvmConfig>;

    /// Spawns the payload service and returns the handle to it.
    ///
    /// The [`BuilderContext`] is provided to allow access to the node's configuration.
    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: TaikoEvmConfig,
    ) -> eyre::Result<Self::PayloadBuilder> {
        Ok(TaikoPayloadBuilder::new(ctx.provider().clone(), pool, evm_config))
    }
}
