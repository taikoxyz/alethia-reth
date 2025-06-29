use alloy_hardforks::EthereumHardforks;
use reth::{
    api::{FullNodeTypes, NodeTypes, PayloadTypes, PrimitivesTy, TxTy},
    builder::{BuilderContext, components::PayloadBuilderBuilder},
    transaction_pool::{PoolTransaction, TransactionPool},
};
use reth_ethereum::EthPrimitives;
use reth_ethereum_engine_primitives::EthBuiltPayload;
use reth_evm::ConfigureEvm;

use crate::payload::{
    attributes::TaikoPayloadAttributes, builder::TaikoPayloadBuilder,
    payload::TaikoPayloadBuilderAttributes,
};

pub mod attributes;
pub mod builder;
pub mod payload;

#[derive(Debug, Default, Clone)]
pub struct TaikoPayloadBuilderBuilder;

impl<Types, Node, Pool, Evm> PayloadBuilderBuilder<Node, Pool, Evm> for TaikoPayloadBuilderBuilder
where
    Types: NodeTypes<ChainSpec: EthereumHardforks, Primitives = EthPrimitives>,
    Node: FullNodeTypes<Types = Types>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
    Evm: ConfigureEvm<
            Primitives = PrimitivesTy<Types>,
            NextBlockEnvCtx = reth_evm::NextBlockEnvAttributes,
        > + 'static,
    Types::Payload: PayloadTypes<
            BuiltPayload = EthBuiltPayload,
            PayloadAttributes = TaikoPayloadAttributes,
            PayloadBuilderAttributes = TaikoPayloadBuilderAttributes,
        >,
{
    type PayloadBuilder = TaikoPayloadBuilder<Node::Provider, Evm>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: Evm,
    ) -> eyre::Result<Self::PayloadBuilder> {
        let _ = pool;
        Ok(TaikoPayloadBuilder::new(ctx.provider().clone(), evm_config))
    }
}
