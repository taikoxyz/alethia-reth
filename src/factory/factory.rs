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
    primitives::{Address, hardfork::SpecId},
};
use reth_evm::precompiles::PrecompilesMap;
use tracing::info;

use crate::{
    evm::evm::{TaikoEvm, TaikoEvmExtraContext},
    factory::alloy::TaikoEvmWrapper,
};

#[derive(Default, Debug, Clone, Copy)]
pub struct TaikoEvmFactory {
    pub extra_context: TaikoEvmExtraContext,
}

impl TaikoEvmFactory {
    pub fn new(extra_context: TaikoEvmExtraContext) -> Self {
        Self { extra_context }
    }

    pub fn basefee_share_pctg(&self) -> u64 {
        self.extra_context.basefee_share_pctg()
    }

    pub fn anchor_caller_address(&self) -> Option<Address> {
        self.extra_context.anchor_caller_address()
    }

    pub fn anchor_caller_nonce(&self) -> Option<u64> {
        self.extra_context.anchor_caller_nonce()
    }
}

impl EvmFactory for TaikoEvmFactory {
    type Evm<DB: Database, I: Inspector<EthEvmContext<DB>, EthInterpreter>> =
        TaikoEvmWrapper<DB, I>;
    type Tx = TxEnv;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Context<DB: Database> = EthEvmContext<DB>;
    type Spec = SpecId;
    type Precompiles = PrecompilesMap;

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

        let taiko_evm = TaikoEvm::new(evm, self.extra_context);

        TaikoEvmWrapper::new(taiko_evm, false)
    }

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

        let taiko_evm = TaikoEvm::new(evm, self.extra_context);

        TaikoEvmWrapper::new(taiko_evm, true)
    }
}
