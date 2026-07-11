use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_primitives::engine::TaikoEngineTypes;

use reth_ethereum::EthPrimitives;
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_node_builder::rpc::{EthApiBuilder, EthApiCtx};
use reth_rpc::{EthApi, eth::core::EthRpcConverterFor};
use reth_rpc_eth_api::node::RpcNodeCoreAdapter;

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
    type EthApi = EthApi<
        RpcNodeCoreAdapter<N::Provider, N::Pool, N::Network, TaikoEvmConfig>,
        EthRpcConverterFor<N>,
    >;

    /// Builds the [`EthApi`] from the given context.
    async fn build_eth_api(self, ctx: EthApiCtx<'_, N>) -> eyre::Result<Self::EthApi> {
        let rpc_evm_config = ctx.components.evm_config().for_rpc();
        Ok(reth_rpc::EthApiBuilder::new(
            ctx.components.provider().clone(),
            ctx.components.pool().clone(),
            ctx.components.network().clone(),
            rpc_evm_config,
        )
        .eth_cache(ctx.cache)
        .task_spawner(ctx.components.task_executor().clone())
        .gas_cap(ctx.config.rpc_gas_cap.into())
        .max_simulate_blocks(ctx.config.rpc_max_simulate_blocks)
        .eth_proof_window(ctx.config.eth_proof_window)
        .fee_history_cache_config(ctx.config.fee_history_cache)
        .proof_permits(ctx.config.proof_permits)
        .gas_oracle_config(ctx.config.gas_oracle)
        .max_batch_size(ctx.config.max_batch_size)
        .max_blocking_io_requests(ctx.config.max_blocking_io_requests)
        .pending_block_kind(ctx.config.pending_block_kind)
        .raw_tx_forwarder(ctx.config.raw_tx_forwarder)
        .evm_memory_limit(ctx.config.rpc_evm_memory_limit)
        .force_blob_sidecar_upcasting(ctx.config.force_blob_sidecar_upcasting)
        .build())
    }
}
