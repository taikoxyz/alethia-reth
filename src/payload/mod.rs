use reth::{
    api::{FullNodeTypes, NodeTypes},
    builder::{BuilderContext, components::PayloadBuilderBuilder},
    transaction_pool::{PoolTransaction, TransactionPool},
};
use reth_ethereum::{EthPrimitives, TransactionSigned};

use crate::{
    chainspec::spec::TaikoChainSpec,
    evm::config::TaikoEvmConfig,
    payload::{builder::TaikoPayloadBuilder, engine::TaikoEngineTypes},
};

pub mod attributes;
pub mod builder;
pub mod engine;
pub mod payload;

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
    type PayloadBuilder = TaikoPayloadBuilder<Node::Provider, TaikoEvmConfig>;

    /// Spawns the payload service and returns the handle to it.
    ///
    /// The [`BuilderContext`] is provided to allow access to the node's configuration.
    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: TaikoEvmConfig,
    ) -> eyre::Result<Self::PayloadBuilder> {
        let _ = pool;
        Ok(TaikoPayloadBuilder::new(ctx.provider().clone(), evm_config))
    }
}
