use std::future;

use alloy_consensus::EthereumTxEnvelope;
use reth::{
    api::{FullNodeTypes, NodeTypes},
    builder::{BuilderContext, components::ExecutorBuilder},
    chainspec::ChainSpec,
    providers::BlockReaderIdExt,
};
use reth_ethereum::EthPrimitives;

use crate::{evm::evm::TaikoEvmExtraContext, factory::config::TaikoEvmConfig};

#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct TaikoExecutorBuilder;

impl<Types, Node> ExecutorBuilder<Node> for TaikoExecutorBuilder
where
    Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>,
    Node: FullNodeTypes<Types = Types>,
{
    type EVM = TaikoEvmConfig;

    fn build_evm(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl Future<Output = eyre::Result<Self::EVM>> + Send {
        let txs = ctx
            .provider()
            .block_by_id(ctx.head().number.into())
            .unwrap()
            .unwrap()
            .into_body()
            .transactions;

        let anchor = txs[0].clone();

        let mut nonce = 0;
        // let mut caller;
        if let EthereumTxEnvelope::Eip1559(tx) = anchor {
            nonce = tx.tx().nonce;
        }

        future::ready(Ok(TaikoEvmConfig::new(
            ctx.chain_spec(),
            TaikoEvmExtraContext::new(
                50,           // basefee share percentage
                Option::None, // TODO: implement a way to get the proposer address
                Some(nonce),
            ),
        )))
    }
}
