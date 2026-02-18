//! Taiko EVM context types and builder traits.
use reth_revm::{
    Context, Database,
    context::{BlockEnv, CfgEnv, Evm, FrameStack, TxEnv},
    handler::EthPrecompiles,
    primitives::hardfork::SpecId,
};
use revm_database_interface::EmptyDB;

use crate::{evm::TaikoEvm, handler::instructions::TaikoInstructions, spec::TaikoSpecId};

/// Canonical Taiko EVM context type used by the Alloy adapter.
pub type TaikoEvmContext<DB> = Context<BlockEnv, TxEnv, CfgEnv<TaikoSpecId>, DB>;

/// Trait used to initialize Context with default taiko_mainnet types.
pub trait TaikoContext {
    /// Creates a new taiko_mainnet context with default configuration.
    fn taiko_mainnet() -> Self;
}

impl TaikoContext for TaikoEvmContext<EmptyDB> {
    fn taiko_mainnet() -> Self {
        Context::new(EmptyDB::new(), SpecId::default())
            .with_cfg(CfgEnv::new_with_spec(TaikoSpecId::SHASTA))
    }
}

/// Builder trait for constructing [`TaikoEvm`] instances from a context.
pub trait TaikoEvmBuilder: Sized {
    /// The context type that will be used in the EVM.
    type Context;

    /// Builds a mainnet EVM instance without an inspector.
    fn build_taiko_mainnet(self) -> TaikoEvm<Self::Context, (), EthPrecompiles>;

    /// Builds a mainnet EVM instance with the provided inspector.
    fn build_taiko_mainnet_with_inspector<INSP>(
        self,
        inspector: INSP,
    ) -> TaikoEvm<Self::Context, INSP, EthPrecompiles>;
}

impl<DB> TaikoEvmBuilder for TaikoEvmContext<DB>
where
    DB: Database,
{
    type Context = Self;

    fn build_taiko_mainnet(self) -> TaikoEvm<Self::Context, (), EthPrecompiles> {
        let spec = self.cfg.spec().to_owned();
        TaikoEvm::new(Evm {
            ctx: self,
            inspector: (),
            instruction: TaikoInstructions::new_taiko_mainnet(spec),
            precompiles: EthPrecompiles::default(),
            frame_stack: FrameStack::new(),
        })
    }

    fn build_taiko_mainnet_with_inspector<INSP>(
        self,
        inspector: INSP,
    ) -> TaikoEvm<Self::Context, INSP, EthPrecompiles> {
        let spec = self.cfg.spec().to_owned();
        TaikoEvm::new(Evm {
            ctx: self,
            inspector,
            instruction: TaikoInstructions::new_taiko_mainnet(spec),
            precompiles: EthPrecompiles::default(),
            frame_stack: FrameStack::new(),
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use reth_revm::{ExecuteEvm, InspectEvm, inspector::NoOpInspector};

    #[test]
    fn default_run_taiko() {
        let ctx = Context::taiko_mainnet();
        // convert to taiko_mainnet context
        let mut evm = ctx.build_taiko_mainnet_with_inspector(NoOpInspector {});
        // execute
        let _ = evm.inner.transact(TxEnv::builder().build_fill());
        // inspect
        let _ = evm.inner.inspect_one_tx(TxEnv::builder().build_fill());
    }
}
