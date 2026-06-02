#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Rust Taiko node (alethia-reth) binary executable.
use std::sync::Arc;

use alethia_reth_chainspec::hardfork::{TaikoHardfork, TaikoHardforks};
use alethia_reth_cli::{
    TaikoChainSpecParser, TaikoCli, TaikoCliExtArgs, command::TaikoNodeExtArgs,
};
use alethia_reth_evm::precompiles::context::set_record_l1_served_calls;
use alethia_reth_l1_client::{L1RpcClient, install_l1sload_fetcher, install_l1staticcall_fetcher};
use alethia_reth_node::{
    TaikoNode,
    proof_history::{install_proof_history, install_proof_history_rpc},
    rpc::eth::{
        auth::{TaikoAuthExt, TaikoAuthExtApiServer},
        eth::{TaikoExt, TaikoExtApiServer},
    },
};
use alloy_hardforks::ForkCondition;
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
        async move |builder, ext_args| {
            info!(target: "reth::taiko::cli", "Launching Taiko node");

            // Wire the L1 RPC fetchers into the L1Sload / L1Staticcall precompiles. The
            // fetchers run on the same tokio runtime as the node (block_in_place +
            // Handle::block_on bridges the synchronous precompile contract into our async HTTP
            // client).
            //
            // Fail-fast policy (mirrors Nethermind's TaikoPlugin): if Unzen is configured on
            // the chainspec (any condition other than `Never`), the L1 RPC URL is mandatory —
            // without it every L1 precompile call would halt at runtime, producing a node that
            // silently rejects every L2 transaction touching the precompiles. Better to refuse
            // to start.
            let unzen_configured = !matches!(
                builder.config().chain.taiko_fork_activation(TaikoHardfork::Unzen),
                ForkCondition::Never,
            );
            match ext_args.l1_rpc_url() {
                Some(l1_url) => {
                    let client = L1RpcClient::new(l1_url).map_err(|e| {
                        eyre::eyre!("failed to initialize L1 RPC client (URL: {l1_url}): {e}")
                    })?;
                    let client = Arc::new(client);
                    let handle = tokio::runtime::Handle::current();
                    install_l1sload_fetcher(client.clone(), handle.clone());
                    install_l1staticcall_fetcher(client, handle);
                    // Live node: nobody collects served-call records here (only the prover
                    // preflight does), so disable recording to keep the lists bounded.
                    set_record_l1_served_calls(false);
                    info!(
                        target: "reth::taiko::cli",
                        l1_url,
                        "L1 precompile RPC fetchers installed",
                    );
                }
                None if unzen_configured => {
                    eyre::bail!(
                        "--l1-rpc-url must be provided when Unzen is configured on the chainspec — \
                         L1Sload/L1Staticcall precompiles require it for live L1 reads",
                    );
                }
                None => {
                    // Unzen not configured: the precompiles never activate, no L1 RPC needed.
                }
            }

            let node_builder = builder.node(TaikoNode);
            let (node_builder, proof_history_storage) =
                install_proof_history(node_builder, ext_args.proof_history_config())?;
            let handle = node_builder
                .extend_rpc_modules(move |mut ctx| {
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

                    if let Some(storage) = proof_history_storage {
                        install_proof_history_rpc(&mut ctx, storage)?;
                    }

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
