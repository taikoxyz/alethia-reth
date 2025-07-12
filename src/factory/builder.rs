use std::future;

use reth::{
    api::{FullNodeTypes, NodeTypes},
    builder::{BuilderContext, components::ExecutorBuilder},
    providers::EthStorage,
};
use reth_ethereum::EthPrimitives;
use reth_trie_db::MerklePatriciaTrie;

use crate::{
    chainspec::spec::TaikoChainSpec, factory::config::TaikoEvmConfig,
    payload::engine::TaikoEngineTypes,
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
            StateCommitment = MerklePatriciaTrie,
            Storage = EthStorage,
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
