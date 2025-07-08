use reth::{
    api::{FullNodeTypes, NodeTypes},
    builder::{BuilderContext, components::PayloadBuilderBuilder},
    providers::EthStorage,
    transaction_pool::{PoolTransaction, TransactionPool},
};
use reth_ethereum::{EthPrimitives, TransactionSigned};
use reth_trie_db::MerklePatriciaTrie;

use crate::{
    chainspec::spec::TaikoChainSpec,
    factory::config::TaikoEvmConfig,
    payload::{builder::TaikoPayloadBuilder, engine::TaikoEngineTypes},
};

pub mod attributes;
pub mod builder;
pub mod engine;
pub mod payload;

#[derive(Debug, Default, Clone)]
pub struct TaikoPayloadBuilderBuilder;

impl<Node, Pool> PayloadBuilderBuilder<Node, Pool, TaikoEvmConfig> for TaikoPayloadBuilderBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = TaikoChainSpec,
            StateCommitment = MerklePatriciaTrie,
            Storage = EthStorage,
            Payload = TaikoEngineTypes,
        >,
    >,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>
        + Unpin
        + 'static,
{
    type PayloadBuilder = TaikoPayloadBuilder<Node::Provider, TaikoEvmConfig>;

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
