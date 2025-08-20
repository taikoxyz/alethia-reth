use std::{convert::Infallible, sync::Arc};

use reth_ethereum::EthPrimitives;
use reth_evm::ConfigureEvm;
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_api::{AddOnsContext, FullNodeComponents, NodeTypes};
use reth_node_builder::rpc::EngineValidatorBuilder;

use crate::{
    block::{assembler::TaikoBlockAssembler, factory::TaikoBlockExecutorFactory},
    chainspec::spec::TaikoChainSpec,
    evm::{config::TaikoNextBlockEnvAttributes, factory::TaikoEvmFactory},
    payload::engine::TaikoEngineTypes,
    rpc::engine::validator::TaikoEngineValidator,
};
/// Builder for [`TaikoEngineValidator`].
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct TaikoEngineValidatorBuilder;

impl<N> EngineValidatorBuilder<N> for TaikoEngineValidatorBuilder
where
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = TaikoChainSpec,
                Payload = TaikoEngineTypes,
            >,
            Evm: ConfigureEvm<
                Primitives = EthPrimitives,
                Error = Infallible,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
                BlockExecutorFactory = TaikoBlockExecutorFactory<
                    RethReceiptBuilder,
                    Arc<TaikoChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
{
    /// The consensus implementation to build.
    type Validator = TaikoEngineValidator;

    /// Creates the engine validator.
    async fn build(self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::Validator> {
        Ok(TaikoEngineValidator::new(ctx.config.chain.clone()))
    }
}
