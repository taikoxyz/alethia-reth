//! Rust Taiko (taiko-reth) binary executable.

use alloy_chains::Chain;
use alloy_genesis::Genesis;
use clap::Parser;
use reth::{
    args::{RessArgs, RpcServerArgs},
    builder::NodeHandle,
    chainspec::ChainSpec,
    cli::Cli,
    providers::providers::NodeTypesForProvider,
    ress::install_ress_subprotocol,
    revm::Database,
    tasks::TaskManager,
};
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{
    LaunchNode, NodeAdapter, NodeBuilder, NodeBuilderWithComponents, NodeComponentsBuilder,
    NodeConfig, WithLaunchContext, rpc::RethRpcAddOns,
};
use reth_node_ethereum::EthereumNode;
use taiko_reth::{TaikoNode, chainspec::parser::TaikoChainSpecParser};
use tracing::info;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    if let Err(err) =
        Cli::<TaikoChainSpecParser, RessArgs>::parse().run(async move |builder, ress_args| {
            info!(target: "reth::cli", "Launching node");
            let NodeHandle {
                node,
                node_exit_future,
            } = builder.node(TaikoNode::default()).launch().await?;

            let node2 = builder.node(TaikoNode::default());
            let launcher = node2.engine_api_launcher();
            node2.launch_with(node2.engine_api_launcher());
            // let node1 = builder.node(EthereumNode::default());

            // let ctx = TestWithLaunchContext { builder: node1 };
            // ctx.launch_with(builder.node(EthereumNode::default()));

            let builder = builder.node(TaikoNode::default());
            // Install ress subprotocol.
            if ress_args.enabled {
                install_ress_subprotocol(
                    ress_args,
                    node.provider,
                    node.evm_config,
                    node.network,
                    node.task_executor,
                    node.add_ons_handle.engine_events.new_listener(),
                )?;
            }

            node_exit_future.await
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

// #[tokio::main]
// async fn main() -> eyre::Result<()> {
//     // let _guard = RethTracer::new().init()?;

//     let tasks = TaskManager::current();

//     // create optimism genesis with canyon at block 2
//     let spec = ChainSpec::builder()
//         .chain(Chain::mainnet())
//         .genesis(Genesis::default())
//         .london_activated()
//         .paris_activated()
//         .shanghai_activated()
//         .build();

//     // create node config
//     let node_config = NodeConfig::test()
//         .with_rpc(RpcServerArgs::default().with_http())
//         .with_chain(spec);

//     let handle = NodeBuilder::new(node_config)
//         .testing_node(tasks.executor())
//         .launch_node(TaikoNode::default())
//         .await
//         .unwrap();

//     println!("Node started");

//     handle.node_exit_future.await
// }

pub struct TestWithLaunchContext<Builder> {
    pub builder: Builder,
}

impl<Types, T, CB, AO>
    TestWithLaunchContext<WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>>
where
    Types: NodeTypesForProvider + NodeTypes,
    // DB: Database + Clone + Unpin + 'static,
    T: FullNodeTypes<
        Types = Types,
        // DB = DB,
        // Provider = BlockchainProvider<NodeTypesWithDBAdapter<Types, DB>>,
    >,
    CB: NodeComponentsBuilder<T>,
    AO: RethRpcAddOns<NodeAdapter<T, CB::Components>>,
{
    /// Launches the node with the given launcher.
    pub fn launch_with<L>(self, launcher: WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>) {
        // launcher.launch_node(self.builder)
        todo!()
    }

    pub fn hello(self) {}
}
