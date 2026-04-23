//! Install the historical-proofs sidecar into the node builder when
//! `--proofs-history` is enabled.
//!
//! When the sidecar is disabled the helper returns the builder unchanged so
//! the code path is inert with zero cost. When enabled, the helper opens the
//! MDBX-backed proof storage, installs the [`OpProofsExEx`] execution
//! extension, and registers the sidecar-backed `eth_getProof` and
//! `debug_executionWitness` RPC overrides.
//!
//! Ported from op-reth's `crates/node/src/proof_history.rs` install pattern
//! and wired directly against op-reth's `OpProofsExEx` + `MdbxProofsStorage`
//! types imported from the ethereum-optimism monorepo.

use std::sync::Arc;

use alethia_reth_cli::args::ProofsHistoryArgs;
use alethia_reth_rpc::proofs::{
    DebugApiProofsOverrideServer, EthApiProofsOverrideServer, ProofsDebugApi, ProofsEthApi,
    ProofsStateProviderFactory,
};
use alloy_rlp::Encodable;
use eyre::WrapErr;
use futures_util::FutureExt;
use reth_evm::ConfigureEvm;
use reth_node_api::{FullNodeComponents, FullNodeTypes, NodePrimitives};
use reth_node_builder::{
    NodeAdapter, NodeBuilderWithComponents, WithLaunchContext, components::NodeComponentsBuilder,
    rpc::RethRpcAddOns,
};
use reth_optimism_exex::OpProofsExEx;
use reth_optimism_trie::{MdbxProofsStorage, OpProofsStorage};
use reth_provider::HeaderProvider;
use reth_rpc_eth_api::{EthApiTypes, RpcNodeCore, helpers::FullEthApi};
use tracing::info;

/// Install the proofs-history ExEx and RPC overrides into the in-progress
/// node builder.
///
/// If `args.enabled` is `false`, the builder is returned unchanged.
/// Otherwise, the MDBX sidecar store is opened, the [`OpProofsExEx`] is
/// scheduled via [`install_exex`](WithLaunchContext::install_exex), and the
/// `eth_getProof` / `debug_executionWitness` RPC handlers are replaced with
/// the sidecar-backed implementations via
/// [`extend_rpc_modules`](WithLaunchContext::extend_rpc_modules).
pub async fn install_proofs_history<T, CB, AO>(
    builder: WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>,
    args: &ProofsHistoryArgs,
) -> eyre::Result<WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>>
where
    T: FullNodeTypes,
    CB: NodeComponentsBuilder<T>,
    AO: RethRpcAddOns<NodeAdapter<T, CB::Components>>,
    AO::EthApi: FullEthApi + Clone,
    <AO::EthApi as RpcNodeCore>::Primitives: NodePrimitives<BlockHeader = alloy_consensus::Header>,
    NodeAdapter<T, CB::Components>: FullNodeComponents,
    <NodeAdapter<T, CB::Components> as FullNodeComponents>::Evm:
        ConfigureEvm<Primitives = <AO::EthApi as RpcNodeCore>::Primitives>,
    <<NodeAdapter<T, CB::Components> as FullNodeTypes>::Provider as HeaderProvider>::Header:
        Encodable,
    jsonrpsee_types::error::ErrorObject<'static>: From<<AO::EthApi as EthApiTypes>::Error>,
{
    if !args.enabled {
        return Ok(builder);
    }

    args.validate()?;
    let path = args
        .storage_path
        .as_ref()
        .expect("storage_path is present because args.validate() succeeded");

    // `OpProofsStorage<S>` is a transparent type alias for `S` in op-reth, so
    // the handle is just `Arc<MdbxProofsStorage>`. The type annotation below
    // documents this at the binding site.
    let storage: OpProofsStorage<Arc<MdbxProofsStorage>> = Arc::new(
        MdbxProofsStorage::new(path)
            .wrap_err_with(|| format!("failed to open proofs-history sidecar at {path:?}"))?,
    );

    info!(
        target: "alethia::proofs-history",
        window = args.window,
        ?path,
        "proofs-history sidecar enabled; historical eth_getProof window controlled by \
         --proofs-history.window, superseding --rpc.eth-proof-window"
    );

    let exex_storage = storage.clone();
    let exex_window = args.window;
    let exex_prune_interval = args.prune_interval;
    let exex_verification_interval = args.verification_interval;

    let rpc_storage = storage;

    Ok(builder
        .install_exex("proofs-history", move |exex_context| {
            let storage = exex_storage.clone();
            async move {
                Ok(OpProofsExEx::builder(exex_context, storage)
                    .with_proofs_history_window(exex_window)
                    .with_proofs_history_prune_interval(exex_prune_interval)
                    .with_verification_interval(exex_verification_interval)
                    .build()
                    .run()
                    .boxed())
            }
        })
        .extend_rpc_modules(move |ctx| {
            let factory = ProofsStateProviderFactory::new(
                ctx.registry.eth_api().clone(),
                rpc_storage.clone(),
            );
            let debug_ext = ProofsDebugApi::new(
                ctx.node().provider().clone(),
                ctx.registry.eth_api().clone(),
                ProofsStateProviderFactory::new(
                    ctx.registry.eth_api().clone(),
                    rpc_storage.clone(),
                ),
                ctx.node().evm_config().clone(),
            );
            let eth_ext = ProofsEthApi::new(ctx.registry.eth_api().clone(), factory);

            let eth_replaced = ctx.modules.replace_configured(eth_ext.into_rpc())?;
            let debug_replaced = ctx.modules.replace_configured(debug_ext.into_rpc())?;
            info!(
                target: "alethia::proofs-history",
                eth_replaced,
                debug_replaced,
                "proofs-history RPC overrides installed (debug_executionWitness, eth_getProof)"
            );
            Ok(())
        }))
}
