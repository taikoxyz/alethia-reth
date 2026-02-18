//! Builder wiring for Taiko engine RPC module construction.
use alloy_rpc_types_engine::ClientVersionV1;
use reth_engine_primitives::EngineApiValidator;
use reth_node_api::{AddOnsContext, FullNodeComponents, NodeTypes};
use reth_node_builder::rpc::{EngineApiBuilder, PayloadValidatorBuilder};
use reth_node_core::version::{CLIENT_CODE, version_metadata};
use reth_payload_builder::PayloadStore;
use reth_rpc::EngineApi;
use reth_rpc_engine_api::EngineCapabilities;

use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_primitives::{TaikoPrimitives, engine::TaikoEngineTypes};

use crate::engine::api::TaikoEngineApi;

/// Builder for basic [`EngineApi`] implementation.
///
/// This provides a basic default implementation for Taiko engine API via
/// [`TaikoEngineTypes`] and uses the general purpose [`EngineApi`] implementation as the builder's
/// output.
#[derive(Debug, Default)]
pub struct TaikoEngineApiBuilder<PVB> {
    /// Builder used to create the payload validator for engine requests.
    payload_validator_builder: PVB,
}

impl<N, PVB> EngineApiBuilder<N> for TaikoEngineApiBuilder<PVB>
where
    N: FullNodeComponents,
    N::Types: NodeTypes<
            Primitives = TaikoPrimitives,
            ChainSpec = TaikoChainSpec,
            Payload = TaikoEngineTypes,
        >,
    PVB: PayloadValidatorBuilder<N>,
    PVB::Validator: EngineApiValidator<<N::Types as NodeTypes>::Payload>,
{
    /// The engine API RPC module.
    type EngineApi = TaikoEngineApi<
        N::Provider,
        <N::Types as NodeTypes>::Payload,
        N::Pool,
        PVB::Validator,
        <N::Types as NodeTypes>::ChainSpec,
    >;

    /// Builds the engine API instance given the provided [`AddOnsContext`].
    ///
    /// [`Self::EngineApi`] will be converted into the method handlers of the authenticated RPC
    /// server (engine API).
    async fn build_engine_api(self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::EngineApi> {
        let Self { payload_validator_builder } = self;

        let engine_validator = payload_validator_builder.build(ctx).await?;
        let client = ClientVersionV1 {
            code: CLIENT_CODE,
            name: version_metadata().name_client.to_string(),
            version: version_metadata().cargo_pkg_version.to_string(),
            commit: version_metadata().vergen_git_sha.to_string(),
        };
        let inner_engine_api = EngineApi::new(
            ctx.node.provider().clone(),
            ctx.config.chain.clone(),
            ctx.beacon_engine_handle.clone(),
            PayloadStore::new(ctx.node.payload_builder_handle().clone()),
            ctx.node.pool().clone(),
            Box::new(ctx.node.task_executor().clone()),
            client,
            EngineCapabilities::default(),
            engine_validator,
            ctx.config.engine.accept_execution_requests_hash,
            ctx.node.network().clone(),
        );

        Ok(TaikoEngineApi::new(
            inner_engine_api,
            ctx.node.provider().clone(),
            PayloadStore::new(ctx.node.payload_builder_handle().clone()),
        ))
    }
}
