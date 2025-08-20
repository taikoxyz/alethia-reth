//! Rust Taiko node (taiko-reth) binary executable.
use reth::ress::install_ress_subprotocol;
use reth_node_builder::NodeHandle;
use reth_node_core::args::RessArgs;
use reth_rpc::eth::{EthApiTypes, RpcNodeCore};
use taiko_reth::{
    chainspec::parser::TaikoChainSpecParser,
    cli::TaikoCli,
    node::TaikoNode,
    rpc::eth::{
        auth::{TaikoAuthExt, TaikoAuthExtApiServer},
        eth::{TaikoExt, TaikoExtApiServer},
    },
};
use tracing::info;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    if let Err(err) = TaikoCli::<TaikoChainSpecParser, RessArgs>::parse_args().run(
        async move |builder, ress_args| {
            info!(target: "reth::taiko::cli", "Launching Taiko node");
            let NodeHandle { node, node_exit_future } = builder
                .node(TaikoNode)
                .extend_rpc_modules(move |ctx| {
                    let provider = ctx.node().provider().clone();

                    // Extend the RPC modules with `taiko_` namespace RPCs extensions.
                    let taiko_rpc_ext = TaikoExt::new(provider.clone());
                    ctx.modules.merge_configured(taiko_rpc_ext.into_rpc())?;

                    // Extend the RPC modules with `taikoAuth_` namespace RPCs extensions.
                    let taiko_auth_rpc_ext = TaikoAuthExt::new(
                        provider,
                        ctx.node().pool().clone(),
                        ctx.registry.eth_api().tx_resp_builder().clone(),
                        ctx.node().evm_config().clone(),
                    );
                    ctx.auth_module.merge_auth_methods(taiko_auth_rpc_ext.into_rpc())?;

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
        },
    ) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
