#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Rust Taiko node (alethia-reth) binary executable.
use alethia_reth_cli::{TaikoCli, TaikoCliExtArgs, parser::TaikoChainSpecParser};
use alethia_reth_node::{
    TaikoNode,
    rpc::eth::{
        auth::{TaikoAuthExt, TaikoAuthExtApiServer},
        eth::{TaikoExt, TaikoExtApiServer},
    },
};
use reth::api::FullNodeComponents;
use reth_rpc::eth::EthApiTypes;
use tracing::info;

#[global_allocator]
/// Global allocator used by the node binary.
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    if let Err(err) = TaikoCli::<TaikoChainSpecParser, TaikoCliExtArgs>::parse_args().run(
        async move |builder, _ext_args| {
            info!(target: "reth::taiko::cli", "Launching Taiko node");
            let handle = builder
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
                        ctx.registry.eth_api().converter().clone(),
                        ctx.node().evm_config().clone(),
                    );
                    ctx.auth_module.merge_auth_methods(taiko_auth_rpc_ext.into_rpc())?;

                    Ok(())
                })
                .launch_with_debug_capabilities()
                .await?;

            handle.wait_for_node_exit().await
        },
    ) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
