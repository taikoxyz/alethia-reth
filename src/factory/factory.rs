use alloy_evm::{Database, EvmEnv, EvmFactory, eth::EthEvmContext};
use reth::revm::{
    Context, Inspector, MainBuilder, MainContext,
    context::{
        TxEnv,
        result::{EVMError, HaltReason},
    },
    handler::EthPrecompiles,
    inspector::NoOpInspector,
    interpreter::interpreter::EthInterpreter,
    primitives::hardfork::SpecId,
};
use reth_evm::precompiles::PrecompilesMap;

use crate::{evm::evm::TaikoEvm, factory::alloy::TaikoEvmWrapper};

/// A factory type for creating instances of the Taiko EVM given a certain input.
#[derive(Default, Debug, Clone, Copy)]
pub struct TaikoEvmFactory;

impl EvmFactory for TaikoEvmFactory {
    /// The EVM type that this factory creates.
    type Evm<DB: Database, I: Inspector<EthEvmContext<DB>, EthInterpreter>> =
        TaikoEvmWrapper<DB, I>;
    /// Transaction environment.
    type Tx = TxEnv;
    /// EVM error.
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
    /// Halt reason.
    type HaltReason = HaltReason;
    /// The EVM context for inspectors.
    type Context<DB: Database> = EthEvmContext<DB>;
    /// The EVM specification identifier
    type Spec = SpecId;
    /// Precompiles used by the EVM.
    type Precompiles = PrecompilesMap;

    /// Creates a new instance of an EVM.
    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let evm = Context::mainnet()
            .with_db(db)
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(PrecompilesMap::from_static(
                EthPrecompiles::default().precompiles,
            ));

        TaikoEvmWrapper::new(TaikoEvm::new(evm), false)
    }

    /// Creates a new instance of an EVM with an inspector.
    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let evm = Context::mainnet()
            .with_db(db)
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(PrecompilesMap::from_static(
                EthPrecompiles::default().precompiles,
            ))
            .with_inspector(inspector);

        TaikoEvmWrapper::new(TaikoEvm::new(evm), false)
    }
}
