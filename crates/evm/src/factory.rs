use alloy_evm::{Database, EvmEnv, EvmFactory};
use alloy_primitives::Address;
use reth_evm::precompiles::PrecompilesMap;
use reth_revm::{
    Inspector,
    context::{
        BlockEnv, TxEnv,
        result::{EVMError, HaltReason},
    },
    inspector::NoOpInspector,
    interpreter::interpreter::EthInterpreter,
};

use crate::{
    alloy::TaikoEvmWrapper,
    context::{TaikoContext, TaikoEvmBuilder, TaikoEvmContext},
    precompiles::taiko_precompiles_map,
    spec::TaikoSpecId,
};

/// A factory type for creating instances of the Taiko EVM given a certain input.
#[derive(Default, Debug, Clone, Copy)]
pub struct TaikoEvmFactory {
    /// Used for customizing the treasury address, chain-specific will be used if None
    treasury_address: Option<Address>,
}

impl TaikoEvmFactory {
    /// Creates a new instance of the Taiko EVM factory.
    pub fn new(treasury_address: Option<Address>) -> Self {
        Self { treasury_address }
    }
}

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
        let precompiles = taiko_precompiles_map(
            reth_revm::precompile::PrecompileSpecId::from_spec_id(spec_id.into()),
        );
        let taiko_evm = TaikoEvmContext::taiko_mainnet()
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .with_db(db)
            .build_taiko_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(precompiles);

        TaikoEvmWrapper::new(taiko_evm, false).with_treasury_address(self.treasury_address)
    }

    /// Creates a new instance of an EVM with an inspector.
    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = input.cfg_env.spec;
        let precompiles = taiko_precompiles_map(
            reth_revm::precompile::PrecompileSpecId::from_spec_id(spec_id.into()),
        );
        let taiko_evm = TaikoEvmContext::taiko_mainnet()
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .with_db(db)
            .build_taiko_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(precompiles)
            .with_inspector(inspector);

        TaikoEvmWrapper::new(taiko_evm, true).with_treasury_address(self.treasury_address)
    }
}
