use std::sync::Arc;

use alloy_evm::{Database, EvmEnv, EvmFactory};
use reth_evm::precompiles::PrecompilesMap;
use reth_revm::{
    Context, Inspector, MainBuilder, MainContext,
    context::{
        BlockEnv, TxEnv,
        result::{EVMError, HaltReason},
    },
    handler::instructions::EthInstructions,
    inspector::NoOpInspector,
    interpreter::interpreter::EthInterpreter,
    precompile::{PrecompileSpecId, Precompiles},
};

use crate::{
    alloy::{TaikoEvmContext, TaikoEvmWrapper},
    error::TaikoInvalidTxError,
    evm::TaikoEvm,
    spec::TaikoSpecId,
    zk_gas::{TaikoChainContext, build_zk_instruction_table},
};
use alethia_reth_chainspec::zk_gas::ZkGasConfig;

/// A factory type for creating instances of the Taiko EVM given a certain input.
#[derive(Debug, Clone)]
pub struct TaikoEvmFactory {
    /// Shared zk gas configuration used to initialize chain context.
    zk_gas_config: ZkGasConfig,
}

impl Default for TaikoEvmFactory {
    /// Creates a factory with the default zk gas configuration.
    fn default() -> Self {
        Self { zk_gas_config: ZkGasConfig::default() }
    }
}

impl TaikoEvmFactory {
    /// Creates a new factory with the provided zk gas configuration.
    pub fn new(zk_gas_config: ZkGasConfig) -> Self {
        Self { zk_gas_config }
    }

    /// Returns the zk gas configuration used by this factory.
    pub fn zk_gas_config(&self) -> ZkGasConfig {
        self.zk_gas_config
    }
}

impl EvmFactory for TaikoEvmFactory {
    /// The EVM type that this factory creates.
    type Evm<DB: Database, I: Inspector<TaikoEvmContext<DB>, EthInterpreter>> =
        TaikoEvmWrapper<DB, I, Self::Precompiles>;
    /// Transaction environment.
    type Tx = TxEnv;
    /// EVM error.
    type Error<DBError: core::error::Error + Send + Sync + 'static> =
        EVMError<DBError, TaikoInvalidTxError>;
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
        let base_table = Arc::new(reth_revm::interpreter::instructions::instruction_table::<
            EthInterpreter,
            TaikoEvmContext<DB>,
        >());
        let instruction_table = build_zk_instruction_table::<TaikoEvmContext<DB>>(&base_table);
        let chain_context = TaikoChainContext::new(self.zk_gas_config, base_table);
        let evm = Context::mainnet()
            .with_chain(chain_context)
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .with_db(db)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(PrecompilesMap::from_static(Precompiles::new(
                PrecompileSpecId::from_spec_id(spec_id.into()),
            )));

        let mut evm = TaikoEvm::new(evm);
        evm.inner.instruction = EthInstructions::new(instruction_table);

        TaikoEvmWrapper::new(evm, false)
    }

    /// Creates a new instance of an EVM with an inspector.
    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = input.cfg_env.spec;
        let base_table = Arc::new(reth_revm::interpreter::instructions::instruction_table::<
            EthInterpreter,
            TaikoEvmContext<DB>,
        >());
        let instruction_table = build_zk_instruction_table::<TaikoEvmContext<DB>>(&base_table);
        let chain_context = TaikoChainContext::new(self.zk_gas_config, base_table);
        let evm = Context::mainnet()
            .with_chain(chain_context)
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .with_db(db)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(PrecompilesMap::from_static(Precompiles::new(
                PrecompileSpecId::from_spec_id(spec_id.into()),
            )))
            .with_inspector(inspector);

        let mut evm = TaikoEvm::new(evm);
        evm.inner.instruction = EthInstructions::new(instruction_table);

        TaikoEvmWrapper::new(evm, true)
    }
}
