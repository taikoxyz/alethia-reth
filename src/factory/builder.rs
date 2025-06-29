use std::future;

use alloy_consensus::EthereumTxEnvelope;
use reth::{
    api::{FullNodeTypes, NodeTypes},
    builder::{BuilderContext, components::ExecutorBuilder},
    chainspec::ChainSpec,
    providers::BlockReaderIdExt,
    revm::primitives::{Address, Bytes, ruint::Uint},
};
use reth_ethereum::EthPrimitives;
use tracing::warn;

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
        let block = ctx
            .provider()
            .block_by_id(ctx.head().number.into())
            .unwrap()
            .unwrap();

        let txs = block.clone().into_body().transactions;

        let mut anchor_caller: Option<Address> = None;
        let mut anchor_nonce: Option<u64> = None;

        // If the block is not the genesis block, we can extract the anchor transaction.
        if ctx.head().number != 0 {
            if txs.len() == 0 {
                warn!(
                    "Block {} has no transactions, cannot extract anchor transaction.",
                    block.header.number
                );
                return future::ready(Err(eyre::eyre!(
                    "Block {} has no transactions, cannot extract anchor transaction.",
                    block.header.number
                )));
            }
            let anchor = txs[0].clone();

            if let EthereumTxEnvelope::Eip1559(tx) = anchor {
                anchor_nonce = Some(tx.tx().nonce);
                anchor_caller = Some(tx.recover_signer().unwrap());
            }
        }

        future::ready(Ok(TaikoEvmConfig::new(
            ctx.chain_spec(),
            TaikoEvmExtraContext::new(
                decode_ontake_extra_data(block.header.extra_data),
                anchor_caller,
                anchor_nonce,
            ),
        )))
    }
}

fn decode_ontake_extra_data(extradata: Bytes) -> u64 {
    let value = Uint::<256, 4>::from_be_slice(&extradata);
    value.as_limbs()[0] as u64
}
