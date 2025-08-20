use alloy_hardforks::EthereumHardforks;
use alloy_rpc_types_engine::ClientVersionV1;
use reth_ethereum_engine_primitives::EthBuiltPayload;
use reth_node_api::{AddOnsContext, EngineTypes, FullNodeComponents, NodeTypes, PayloadTypes};
use reth_node_builder::rpc::{EngineApiBuilder, EngineValidatorBuilder};
use reth_node_core::version::{CARGO_PKG_VERSION, CLIENT_CODE, NAME_CLIENT, VERGEN_GIT_SHA};
use reth_payload_builder::PayloadStore;
use reth_rpc::EngineApi;
use reth_rpc_engine_api::EngineCapabilities;

use crate::{
    payload::{attributes::TaikoPayloadAttributes, primitives::TaikoExecutionData},
    rpc::engine::api::TaikoEngineApi,
};

/// Builder for basic [`EngineApi`] implementation.
///
/// This provides a basic default implementation for Taiko engine API via
/// [`TaikoEngineTypes`] and uses the general purpose [`EngineApi`] implementation as the builder's
/// output.
#[derive(Debug, Default)]
pub struct TaikoEngineApiBuilder<EV> {
    engine_validator_builder: EV,
}

impl<N, EV> EngineApiBuilder<N> for TaikoEngineApiBuilder<EV>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec: EthereumHardforks,
            Payload: PayloadTypes<
                ExecutionData = TaikoExecutionData,
                PayloadAttributes = TaikoPayloadAttributes,
                BuiltPayload = EthBuiltPayload,
            > + EngineTypes,
        >,
    >,
    EV: EngineValidatorBuilder<N>,
{
    /// The engine API RPC module.
    type EngineApi = TaikoEngineApi<
        N::Provider,
        <N::Types as NodeTypes>::Payload,
        N::Pool,
        EV::Validator,
        <N::Types as NodeTypes>::ChainSpec,
    >;

    /// Builds the engine API instance given the provided [`AddOnsContext`].
    ///
    /// [`Self::EngineApi`] will be converted into the method handlers of the authenticated RPC
    /// server (engine API).
    async fn build_engine_api(self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::EngineApi> {
        let Self { engine_validator_builder } = self;

        let engine_validator = engine_validator_builder.build(ctx).await?;
        let client = ClientVersionV1 {
            code: CLIENT_CODE,
            name: NAME_CLIENT.to_string(),
            version: CARGO_PKG_VERSION.to_string(),
            commit: VERGEN_GIT_SHA.to_string(),
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
        );

        Ok(TaikoEngineApi::new(
            inner_engine_api,
            ctx.node.provider().clone(),
            PayloadStore::new(ctx.node.payload_builder_handle().clone()),
        ))
    }
}
