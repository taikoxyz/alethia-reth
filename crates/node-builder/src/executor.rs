use std::future;

use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_primitives::{TaikoPrimitives, engine::TaikoEngineTypes};
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{BuilderContext, components::ExecutorBuilder};

/// A builder for the Taiko block executor.
#[derive(Debug, Clone, Default)]
pub struct TaikoExecutorBuilder;

impl<Types, Node> ExecutorBuilder<Node> for TaikoExecutorBuilder
where
    Types: NodeTypes<
            Primitives = TaikoPrimitives,
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
    ) -> impl future::Future<Output = eyre::Result<Self::EVM>> + Send {
        future::ready(Ok(TaikoEvmConfig::new(ctx.chain_spec())))
    }
}
