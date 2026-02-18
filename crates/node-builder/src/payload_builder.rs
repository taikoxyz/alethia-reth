use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_consensus::transaction::TaikoTxEnvelope;
use alethia_reth_payload::TaikoPayloadBuilder;
use alethia_reth_primitives::{TaikoPrimitives, engine::TaikoEngineTypes};
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{BuilderContext, components::PayloadBuilderBuilder};
use reth_transaction_pool::{PoolTransaction, TransactionPool};

/// The builder to spawn [`TaikoPayloadBuilder`] payload building tasks.
#[derive(Debug, Default, Clone)]
pub struct TaikoPayloadBuilderBuilder;

impl<Node, Pool> PayloadBuilderBuilder<Node, Pool, TaikoEvmConfig> for TaikoPayloadBuilderBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            Primitives = TaikoPrimitives,
            ChainSpec = TaikoChainSpec,
            Payload = TaikoEngineTypes,
        >,
    >,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TaikoTxEnvelope>>
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
