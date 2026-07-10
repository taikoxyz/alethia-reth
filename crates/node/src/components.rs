//! Reth component builders (executor, network, consensus) for Taiko node composition.
use std::{fmt::Debug, future, sync::Arc};

use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_consensus::validation::{TaikoBeaconConsensus, TaikoBlockReader};
use alethia_reth_evm::jit::JitConfig;
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
#[cfg(feature = "jit")]
use tracing::warn;

/// A builder for the Taiko block executor.
#[derive(Debug, Clone, Default)]
pub struct TaikoExecutorBuilder {
    /// Runtime configuration used to create the shared revmc backend.
    jit: JitConfig,
}

impl TaikoExecutorBuilder {
    /// Creates an executor builder with the supplied JIT runtime settings.
    pub const fn new(jit: JitConfig) -> Self {
        Self { jit }
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

    /// Creates the EVM config.
    fn build_evm(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl future::Future<Output = eyre::Result<Self::EVM>> + Send {
        #[cfg(feature = "jit")]
        let result = (|| {
            let dump_dir = self.jit.debug.then(|| ctx.config().datadir().data_dir().join("jit"));
            let backend = self.jit.build_backend(dump_dir)?;

            if self.jit.enabled {
                warn!(
                    target: "reth::taiko::cli",
                    hot_threshold = self.jit.hot_threshold,
                    workers = ?self.jit.worker_count,
                    blocking = self.jit.blocking,
                    "Started experimental revmc JIT backend; this may cause instability"
                );
            }

            let factory = alethia_reth_evm::factory::TaikoEvmFactory::new(backend);
            Ok(TaikoEvmConfig::new_with_evm_factory(ctx.chain_spec(), factory))
        })();

        #[cfg(not(feature = "jit"))]
        let result = if self.jit.enabled {
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
    fn taiko_executor_builder_disables_jit_by_default() {
        assert!(!TaikoExecutorBuilder::default().jit.enabled);
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
