//! Rust Taiko (taiko-reth) binary executable.
use clap::Parser;
use reth::{args::RessArgs, builder::NodeHandle, cli::Cli, ress::install_ress_subprotocol};
use reth_rpc::eth::RpcNodeCore;
use taiko_reth::{
    TaikoNode,
    chainspec::parser::TaikoChainSpecParser,
    rpc::eth::{TaikoExt, TaikoExtApiServer},
};
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
            } = builder
                .node(TaikoNode::default())
                .extend_rpc_modules(move |ctx| {
                    let provider = ctx.node().provider().clone();
                    let taiko_rpc_ext = TaikoExt::new(provider);
                    ctx.modules.merge_configured(taiko_rpc_ext.into_rpc())?;

                    Ok(())
                })
                .launch_with_debug_capabilities()
                .await?;

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
