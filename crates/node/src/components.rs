//! Reth component builders (executor, network, consensus) for Taiko node composition.
use std::{fmt::Debug, future, sync::Arc};

use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_consensus::validation::{TaikoBeaconConsensus, TaikoBlockReader};
use alethia_reth_primitives::engine::TaikoEngineTypes;
use alloy_primitives::B256;
use reth::{
    network::{EthNetworkPrimitives, NetworkHandle, PeersInfo},
    transaction_pool::{PoolTransaction, TransactionPool},
};
use reth_ethereum::{EthPrimitives, PooledTransactionVariant};
use reth_node_api::{FullNodeTypes, NodeTypes, TxTy};
use reth_node_builder::{
    BuilderContext,
    components::{ConsensusBuilder, ExecutorBuilder, NetworkBuilder},
};
use reth_primitives_traits::{AlloyBlockHeader, Block};
use reth_provider::BlockReader;
use tracing::info;

/// A builder for the Taiko block executor.
#[derive(Debug, Clone, Default)]
pub struct TaikoExecutorBuilder;

impl TaikoExecutorBuilder {
    /// Builds an EVM configuration whose factory owns the shared revmc backend created from
    /// the node's `--jit` CLI settings.
    #[cfg(feature = "jit")]
    fn build_jit_evm_config(
        chain_spec: Arc<TaikoChainSpec>,
        jit: &reth_node_core::args::JitArgs,
        dump_dir: Option<std::path::PathBuf>,
    ) -> eyre::Result<TaikoEvmConfig> {
        let config = alethia_reth_evm::jit::JitConfig {
            enabled: jit.enabled,
            hot_threshold: jit.hot_threshold,
            worker_count: jit.worker_count,
            channel_capacity: jit.channel_capacity,
            max_pending_jobs: jit.max_pending_jobs,
            max_bytecode_len: jit.max_bytecode_len,
            code_cache_bytes: jit.code_cache_bytes,
            idle_evict_duration: jit.idle_evict_duration,
            debug: jit.debug,
            blocking: jit.blocking,
        };
        let backend = config.build_backend(dump_dir)?;

        if config.enabled {
            tracing::warn!(
                target: "reth::taiko::cli",
                hot_threshold = config.hot_threshold,
                workers = ?config.worker_count,
                blocking = config.blocking,
                "Started experimental revmc JIT backend; this may cause instability"
            );
        }

        let factory = alethia_reth_evm::factory::TaikoEvmFactory::new(backend);
        Ok(TaikoEvmConfig::new_with_evm_factory(chain_spec, factory))
    }
}

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

    /// Creates the EVM config from the node's `--jit` CLI settings.
    fn build_evm(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl future::Future<Output = eyre::Result<Self::EVM>> + Send {
        let jit = &ctx.config().jit;

        #[cfg(feature = "jit")]
        let result = Self::build_jit_evm_config(
            ctx.chain_spec(),
            jit,
            jit.debug.then(|| ctx.config().datadir().data_dir().join("jit")),
        );

        #[cfg(not(feature = "jit"))]
        let result = if jit.enabled {
            Err(eyre::eyre!(
                "JIT compilation was requested with --jit, but this binary was built without the `jit` feature"
            ))
        } else {
            Ok(TaikoEvmConfig::new(ctx.chain_spec()))
        };

        future::ready(result)
    }
}

/// A basic Taiko network builder service.
#[derive(Debug, Default, Clone, Copy)]
pub struct TaikoNetworkBuilder;

impl<Node, Pool> NetworkBuilder<Node, Pool> for TaikoNetworkBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = TaikoChainSpec, Primitives = EthPrimitives>>,
    Pool: TransactionPool<
            Transaction: PoolTransaction<
                Consensus = TxTy<Node::Types>,
                Pooled = PooledTransactionVariant,
            >,
        > + Unpin
        + 'static,
{
    /// The network built.
    type Network = NetworkHandle<EthNetworkPrimitives>;

    /// Launches the network implementation and returns the handle to it.
    async fn build_network(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<Self::Network> {
        let network = ctx.network_builder().await?;
        let handle = ctx.start_network(network, pool);
        info!(target: "reth::taiko::cli", enode=%handle.local_node_record(), "P2P networking initialized");
        Ok(handle)
    }
}

/// Adapter that exposes a `reth_provider::BlockReader` as a Taiko block reader.
#[derive(Debug)]
pub struct ProviderTaikoBlockReader<T>(pub T);

impl<T> TaikoBlockReader for ProviderTaikoBlockReader<T>
where
    T: BlockReader + Debug + Send + Sync,
    T::Block: Block,
{
    fn block_timestamp_by_hash(&self, hash: B256) -> Option<u64> {
        self.0.block_by_hash(hash).ok().flatten().map(|block| block.header().timestamp())
    }
}

/// A basic Taiko consensus builder.
#[derive(Debug, Default, Clone)]
pub struct TaikoConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for TaikoConsensusBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = TaikoChainSpec,
            Payload = TaikoEngineTypes,
        >,
    >,
{
    /// The consensus implementation to build.
    type Consensus = Arc<TaikoBeaconConsensus>;

    /// Creates the TaikoBeaconConsensus implementation.
    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        let block_reader = Arc::new(ProviderTaikoBlockReader(ctx.provider().clone()));
        Ok(Arc::new(TaikoBeaconConsensus::new(ctx.chain_spec(), block_reader)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alethia_reth_chainspec::TAIKO_MAINNET;
    use reth_storage_api::noop::NoopProvider;

    #[test]
    fn taiko_executor_builder_is_zero_sized() {
        assert_eq!(std::mem::size_of::<TaikoExecutorBuilder>(), 0);
    }

    #[test]
    fn taiko_network_builder_is_zero_sized() {
        assert_eq!(std::mem::size_of::<TaikoNetworkBuilder>(), 0);
    }

    #[test]
    fn provider_reader_returns_none_for_missing_block_hash() {
        let provider = NoopProvider::<TaikoChainSpec, EthPrimitives>::new(TAIKO_MAINNET.clone());
        let reader = ProviderTaikoBlockReader(provider);
        assert_eq!(reader.block_timestamp_by_hash(B256::ZERO), None);
    }
}
