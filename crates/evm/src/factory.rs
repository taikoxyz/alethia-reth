use alloy_evm::{Database, EvmEnv, EvmFactory};
use reth_evm::precompiles::PrecompilesMap;
use reth_revm::{
    Context, Inspector, MainBuilder, MainContext,
    context::{
        BlockEnv, DBErrorMarker, TxEnv,
        result::{EVMError, HaltReason},
    },
    inspector::NoOpInspector,
    interpreter::interpreter::EthInterpreter,
    precompile::{PrecompileSpecId, Precompiles},
};

use crate::{
    alloy::{TaikoEvmContext, TaikoEvmWrapper},
    evm::TaikoEvm,
    spec::TaikoSpecId,
    zk_gas::{adapter::ZkGasInspector, schedule::schedule_for},
};

#[cfg(feature = "jit")]
use crate::jit::JitBackend;

/// A factory type for creating instances of the Taiko EVM given a certain input.
///
/// With the `jit` Cargo feature, the factory owns the shared revmc backend. An EVM can execute
/// JIT-compiled code only when all gates pass: the binary was built with the `jit` feature,
/// runtime compilation was enabled with `--jit` (or the `reth_jit` RPC method), the local config
/// selected JIT support via [`reth_evm::ConfigureEvm::with_jit_support`], and the active fork is
/// on the [`spec_supports_jit`] allowlist.
#[derive(Debug, Clone)]
pub struct TaikoEvmFactory {
    /// Shared runtime backend used for JIT-eligible execution.
    #[cfg(feature = "jit")]
    backend: JitBackend,
    /// Disabled backend used for zk-gas forks, inspected execution, and unsupported paths.
    #[cfg(feature = "jit")]
    disabled_backend: JitBackend,
    /// Whether locally created EVMs may dispatch to the shared JIT backend.
    #[cfg(feature = "jit")]
    jit_support: bool,
}

#[cfg(feature = "jit")]
impl TaikoEvmFactory {
    /// Creates a factory backed by the supplied revmc runtime.
    pub fn new(backend: JitBackend) -> Self {
        Self { backend, disabled_backend: JitBackend::disabled(), jit_support: false }
    }

    /// Enables or disables JIT dispatch for EVMs subsequently created by this factory.
    pub const fn with_jit_support_enabled(mut self, enabled: bool) -> Self {
        self.jit_support = enabled;
        self
    }

    /// Enables JIT dispatch for EVMs subsequently created by this factory.
    pub const fn with_jit_support(self) -> Self {
        self.with_jit_support_enabled(true)
    }

    /// Returns whether this factory selects the shared backend for eligible execution.
    pub const fn jit_support_enabled(&self) -> bool {
        self.jit_support
    }

    /// Returns the shared revmc backend handle.
    pub const fn backend(&self) -> &JitBackend {
        &self.backend
    }

    /// Selects the backend for an uninspected execution at the given Taiko fork.
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

/// Runtime controls for the shared revmc backend, exposed through
/// [`reth_evm::ConfigureEvm::jit_backend`] so the upstream `reth_jit` RPC action works
/// against the Taiko EVM configuration.
#[cfg(feature = "jit")]
impl reth_evm::JitBackend for TaikoEvmFactory {
    /// Enables or disables JIT lookups and background compilation.
    fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        self.backend.set_enabled(enabled).map_err(|err| err.to_string())
    }

    /// Pauses out-of-process helper execution without discarding compiled code.
    fn pause(&self) {
        self.backend.pause();
    }

    /// Resumes background JIT promotion.
    fn resume(&self) {
        self.backend.resume();
    }

    /// Clears resident and persisted compiled artifacts.
    fn clear(&self) {
        self.backend.clear_all();
    }
}

impl EvmFactory for TaikoEvmFactory {
    /// The EVM type that this factory creates.
    type Evm<DB: Database, I: Inspector<TaikoEvmContext<DB>, EthInterpreter>> =
        TaikoEvmWrapper<DB, I, Self::Precompiles>;
    /// Transaction environment.
    type Tx = TxEnv;
    /// EVM error.
    type Error<DBError: DBErrorMarker> = EVMError<DBError>;
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
    ///
    /// Inspected execution always selects the disabled backend as a correctness requirement:
    /// compiled code cannot deliver the per-step inspector callbacks (revmc forwards only log,
    /// selfdestruct, and frame-end events) that tracers and the zk-gas inspector depend on.
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
    fn jit_dispatch_is_allowlisted_to_non_zk_gas_specs() {
        assert!(spec_supports_jit(TaikoSpecId::GENESIS));
        assert!(spec_supports_jit(TaikoSpecId::ONTAKE));
        assert!(spec_supports_jit(TaikoSpecId::PACAYA));
        assert!(spec_supports_jit(TaikoSpecId::SHASTA));
        assert!(!spec_supports_jit(TaikoSpecId::UNZEN));
    }
}
