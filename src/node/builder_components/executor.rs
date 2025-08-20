use std::future;

use reth_ethereum::EthPrimitives;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{BuilderContext, components::ExecutorBuilder};

use crate::{
    chainspec::spec::TaikoChainSpec, evm::config::TaikoEvmConfig, payload::engine::TaikoEngineTypes,
};
/// A builder for the Taiko block executor.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct TaikoExecutorBuilder;

impl<Types, Node> ExecutorBuilder<Node> for TaikoExecutorBuilder
where
    Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = TaikoChainSpec,
            Payload = TaikoEngineTypes,
        >,
    Node: FullNodeTypes<Types = Types>,
{
    /// The EVM config to use.
    type EVM = TaikoEvmConfig;

    /// Creates the EVM config.
    fn build_evm(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl Future<Output = eyre::Result<Self::EVM>> + Send {
        future::ready(Ok(TaikoEvmConfig::new(ctx.chain_spec())))
    }
}
