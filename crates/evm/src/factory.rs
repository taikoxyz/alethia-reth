use alloy_evm::{Database, EvmEnv, EvmFactory};
use reth_evm::precompiles::PrecompilesMap;
use reth_revm::{
    Context, Inspector, MainBuilder, MainContext,
    context::{
        BlockEnv, TxEnv,
        result::{EVMError, HaltReason},
    },
    inspector::NoOpInspector,
    interpreter::interpreter::EthInterpreter,
    precompile::{PrecompileSpecId, Precompiles},
};

use crate::{
    alloy::{TaikoEvmContext, TaikoEvmWrapper},
    evm::TaikoEvm,
    jit::JitBackendControl,
    spec::TaikoSpecId,
    zk_gas::{adapter::ZkGasInspector, schedule::schedule_for},
};

#[cfg(feature = "jit")]
use crate::jit::JitBackend;

/// A factory type for creating instances of the Taiko EVM given a certain input.
#[derive(Debug, Clone)]
pub struct TaikoEvmFactory {
    /// Shared runtime backend used for pre-Unzen execution.
    #[cfg(feature = "jit")]
    backend: JitBackend,
    /// Disabled backend used for Unzen, direct RPC, and inspected execution.
    #[cfg(feature = "jit")]
    disabled_backend: JitBackend,
    /// Whether locally created EVMs may dispatch to the shared JIT backend.
    #[cfg(feature = "jit")]
    jit_support: bool,
}

impl TaikoEvmFactory {
    /// Creates a factory backed by the supplied revmc runtime.
    #[cfg(feature = "jit")]
    pub fn new(backend: JitBackend) -> Self {
        Self { backend, disabled_backend: JitBackend::disabled(), jit_support: false }
    }

    /// Enables or disables JIT dispatch for EVMs subsequently created by this factory.
    #[cfg(feature = "jit")]
    pub const fn with_jit_support_enabled(mut self, enabled: bool) -> Self {
        self.jit_support = enabled;
        self
    }

    /// Enables JIT dispatch for EVMs subsequently created by this factory.
    #[cfg(feature = "jit")]
    pub const fn with_jit_support(self) -> Self {
        self.with_jit_support_enabled(true)
    }

    /// Returns whether this factory selects the shared backend for eligible execution.
    #[cfg(feature = "jit")]
    pub const fn jit_support_enabled(&self) -> bool {
        self.jit_support
    }

    /// Returns the runtime controls when JIT support was compiled in.
    pub fn jit_backend(&self) -> Option<&dyn JitBackendControl> {
        #[cfg(feature = "jit")]
        {
            Some(&self.backend)
        }
        #[cfg(not(feature = "jit"))]
        None
    }

    /// Selects the backend for an uninspected execution at the given Taiko fork.
    #[cfg(feature = "jit")]
    fn backend_for_spec(&self, spec_id: TaikoSpecId) -> JitBackend {
        // `schedule_for` is re-checked as a belt-and-suspenders guard: a spec with a zk-gas
        // schedule must never reach compiled code even if the allowlist says otherwise.
        if self.jit_support && spec_supports_jit(spec_id) && schedule_for(spec_id).is_none() {
            self.backend.clone()
        } else {
            self.disabled_backend.clone()
        }
    }
}

/// Returns whether execution under `spec_id` may dispatch to revmc-compiled code.
///
/// Compiled programs bake in upstream mainnet gas and opcode semantics, so only forks that
/// execute with exactly those semantics are eligible. Unzen is excluded because its zk-gas
/// metering needs per-opcode interpreter hooks.
///
/// This match is deliberately exhaustive: adding a fork variant fails compilation here so that
/// every new fork is classified explicitly. Any fork that reprices gas or changes opcode
/// behavior in any way must stay interpreter-only — compiled code would silently diverge from
/// consensus otherwise.
#[cfg(feature = "jit")]
const fn spec_supports_jit(spec_id: TaikoSpecId) -> bool {
    match spec_id {
        TaikoSpecId::GENESIS | TaikoSpecId::ONTAKE | TaikoSpecId::PACAYA | TaikoSpecId::SHASTA => {
            true
        }
        TaikoSpecId::UNZEN => false,
    }
}

impl Default for TaikoEvmFactory {
    /// Creates an interpreter-only factory.
    fn default() -> Self {
        #[cfg(feature = "jit")]
        {
            Self::new(JitBackend::disabled())
        }
        #[cfg(not(feature = "jit"))]
        Self {}
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
        let schedule = schedule_for(spec_id);
        let evm = Context::mainnet()
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .with_db(db)
            .build_mainnet_with_inspector(ZkGasInspector::new(NoOpInspector {}, None))
            .with_precompiles(PrecompilesMap::from_static(Precompiles::new(
                PrecompileSpecId::from_spec_id(spec_id.into()),
            )));

        let evm = TaikoEvm::new(evm).with_zk_gas_schedule(schedule);
        #[cfg(feature = "jit")]
        let evm = revmc::revm_evm::JitEvm::new(evm, self.backend_for_spec(spec_id));

        TaikoEvmWrapper::new_with_inner(evm, false)
    }

    /// Creates a new instance of an EVM with an inspector.
    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = input.cfg_env.spec;
        let schedule = schedule_for(spec_id);
        let evm = Context::mainnet()
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .with_db(db)
            .build_mainnet_with_inspector(ZkGasInspector::new(inspector, schedule))
            .with_precompiles(PrecompilesMap::from_static(Precompiles::new(
                PrecompileSpecId::from_spec_id(spec_id.into()),
            )));

        let evm = TaikoEvm::new(evm);
        #[cfg(feature = "jit")]
        let evm = revmc::revm_evm::JitEvm::new(evm, self.disabled_backend.clone());

        TaikoEvmWrapper::new_with_inner(evm, true)
    }
}

#[cfg(all(test, feature = "jit"))]
mod tests {
    use super::*;

    #[test]
    fn jit_dispatch_is_allowlisted_to_pre_unzen_specs() {
        assert!(spec_supports_jit(TaikoSpecId::GENESIS));
        assert!(spec_supports_jit(TaikoSpecId::ONTAKE));
        assert!(spec_supports_jit(TaikoSpecId::PACAYA));
        assert!(spec_supports_jit(TaikoSpecId::SHASTA));
        assert!(!spec_supports_jit(TaikoSpecId::UNZEN));
    }
}
