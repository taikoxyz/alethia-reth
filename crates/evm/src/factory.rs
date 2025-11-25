use alloy_evm::{Database, EvmEnv, EvmFactory};
use reth::revm::{
    Context, Inspector, MainBuilder, MainContext,
    context::{
        BlockEnv, TxEnv,
        result::{EVMError, HaltReason},
    },
    inspector::NoOpInspector,
    interpreter::interpreter::EthInterpreter,
};
use reth_evm::precompiles::PrecompilesMap;
use reth_revm::precompile::{PrecompileSpecId, Precompiles};

use crate::{
    alloy::{TaikoEvmContext, TaikoEvmWrapper},
    evm::TaikoEvm,
    spec::TaikoSpecId,
};

/// A factory type for creating instances of the Taiko EVM given a certain input.
#[derive(Default, Debug, Clone, Copy)]
pub struct TaikoEvmFactory;

impl EvmFactory for TaikoEvmFactory {
    /// The EVM type that this factory creates.
    type Evm<DB: Database, I: Inspector<TaikoEvmContext<DB>, EthInterpreter>> =
        TaikoEvmWrapper<DB, I, Self::Precompiles>;
    /// Transaction environment.
    type Tx = TxEnv;
    /// EVM error.
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
    /// Halt reason.
    type HaltReason = HaltReason;
    /// The EVM context for inspectors.
    type Context<DB: Database> = TaikoEvmContext<DB>;
    /// The EVM specification identifier
    type Spec = TaikoSpecId;
    /// Block environment used by the EVM.
    type BlockEnv = BlockEnv;
    /// Precompiles used by the EVM.
    type Precompiles = PrecompilesMap;

    /// Creates a new instance of an EVM.
    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let spec_id = input.cfg_env.spec;
        let evm = Context::mainnet()
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .with_db(db)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(PrecompilesMap::from_static(Precompiles::new(
                PrecompileSpecId::from_spec_id(spec_id.into()),
            )));

        TaikoEvmWrapper::new(TaikoEvm::new(evm), false)
    }

    /// Creates a new instance of an EVM with an inspector.
    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = input.cfg_env.spec;
        let evm = Context::mainnet()
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .with_db(db)
            .build_mainnet_with_inspector(inspector)
            .with_precompiles(PrecompilesMap::from_static(Precompiles::new(
                PrecompileSpecId::from_spec_id(spec_id.into()),
            )));

        TaikoEvmWrapper::new(TaikoEvm::new(evm), true)
    }
}
