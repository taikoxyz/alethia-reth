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

/// Builds the EVM configuration honoring the runtime `--jit` CLI settings.
///
/// With the `jit` Cargo feature, the returned config's factory owns a shared revmc backend
/// created from `jit` (`dump_dir` receives debug artifacts when `--jit.debug` is set). Without
/// the feature this errors when `--jit` was requested, and otherwise returns the plain
/// interpreter config.
///
/// Shared by the node's [`ExecutorBuilder`] and by CLI subcommands that execute blocks outside
/// the node builder (`re-execute`), whose reth-provided components closure cannot see the
/// command's own `JitArgs`.
pub fn evm_config_from_jit_args(
    chain_spec: Arc<TaikoChainSpec>,
    jit: &reth_node_core::args::JitArgs,
    dump_dir: Option<std::path::PathBuf>,
) -> eyre::Result<TaikoEvmConfig> {
    #[cfg(feature = "jit")]
    {
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
            // revmc's runtime coerces `enabled = true` and `jit_hot_threshold = 0` whenever
            // `blocking` is set, so forwarding `--jit.blocking` on its own would compile every
            // contract on first touch without `--jit` and without the warning below. Gating it
            // here keeps `--jit`/`reth_jit` the only way to turn compilation on.
            blocking: jit.enabled && jit.blocking,
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

        // `blocking` is fixed at backend construction and never re-read by revmc, so a later
        // `reth_jit` runtime enable cannot recover it — surface the silent drop instead.
        if jit.blocking && !jit.enabled {
            tracing::warn!(
                target: "reth::taiko::cli",
                "--jit.blocking has no effect without --jit and cannot be applied by a later reth_jit enable"
            );
        }

        let factory = alethia_reth_evm::factory::TaikoEvmFactory::new(backend);
        Ok(TaikoEvmConfig::new_with_evm_factory(chain_spec, factory))
    }

    #[cfg(not(feature = "jit"))]
    {
        let _ = dump_dir;
        if jit.enabled {
            Err(eyre::eyre!(
                "JIT compilation was requested with --jit, but this binary was built without the `jit` feature"
            ))
        } else {
            Ok(TaikoEvmConfig::new(chain_spec))
        }
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
        let dump_dir = jit.debug.then(|| ctx.config().datadir().data_dir().join("jit"));

        future::ready(evm_config_from_jit_args(ctx.chain_spec(), jit, dump_dir))
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

    #[cfg(feature = "jit")]
    #[test]
    fn evm_config_from_jit_args_wires_the_runtime_enabled_flag() {
        use reth_node_core::args::JitArgs;

        let enabled = evm_config_from_jit_args(
            TAIKO_MAINNET.clone(),
            &JitArgs { enabled: true, ..Default::default() },
            None,
        )
        .expect("jit build constructs a backend");
        assert!(enabled.evm_factory().backend().enabled());

        let disabled = evm_config_from_jit_args(TAIKO_MAINNET.clone(), &JitArgs::default(), None)
            .expect("jit build constructs a backend");
        assert!(!disabled.evm_factory().backend().enabled());
    }

    #[cfg(feature = "jit")]
    #[test]
    fn evm_config_from_jit_args_keeps_blocking_from_enabling_jit() {
        use reth_node_core::args::JitArgs;

        // revmc turns `blocking` into `enabled = true` with a zero hot threshold, so
        // `--jit.blocking` alone would otherwise compile every contract without `--jit`.
        let blocking_only = evm_config_from_jit_args(
            TAIKO_MAINNET.clone(),
            &JitArgs { blocking: true, ..Default::default() },
            None,
        )
        .expect("jit build constructs a backend");
        assert!(!blocking_only.evm_factory().backend().enabled());

        let blocking_with_jit = evm_config_from_jit_args(
            TAIKO_MAINNET.clone(),
            &JitArgs { enabled: true, blocking: true, ..Default::default() },
            None,
        )
        .expect("jit build constructs a backend");
        assert!(blocking_with_jit.evm_factory().backend().enabled());
    }

    #[cfg(not(feature = "jit"))]
    #[test]
    fn evm_config_from_jit_args_rejects_jit_on_non_jit_builds() {
        use reth_node_core::args::JitArgs;

        let err = evm_config_from_jit_args(
            TAIKO_MAINNET.clone(),
            &JitArgs { enabled: true, ..Default::default() },
            None,
        )
        .expect_err("non-jit build must reject --jit");
        assert!(err.to_string().contains("`jit` feature"));

        assert!(evm_config_from_jit_args(TAIKO_MAINNET.clone(), &JitArgs::default(), None).is_ok());
    }
}
