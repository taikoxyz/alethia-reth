use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_primitives::engine::TaikoEngineTypes;

use reth_ethereum::EthPrimitives;
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_node_builder::rpc::{EthApiBuilder, EthApiCtx};
use reth_rpc::{EthApi, eth::core::EthRpcConverterFor};

/// Builds the Taiko `eth` API ([`EthApi`]) for the Taiko node.
#[derive(Debug, Default)]
pub struct TaikoEthApiBuilder;

impl<N> EthApiBuilder<N> for TaikoEthApiBuilder
where
    N: FullNodeComponents<Evm = TaikoEvmConfig>,
    N::Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = TaikoChainSpec,
            Payload = TaikoEngineTypes,
        >,
{
    /// The Ethapi implementation this builder will build.
    type EthApi = EthApi<N, EthRpcConverterFor<N>>;

    /// Builds the [`EthApi`] from the given context.
    async fn build_eth_api(self, ctx: EthApiCtx<'_, N>) -> eyre::Result<Self::EthApi> {
        Ok(ctx.eth_api_builder().build())
    }
}
