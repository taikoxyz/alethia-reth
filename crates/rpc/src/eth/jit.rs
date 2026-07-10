//! Runtime control for the revmc JIT backend through the upstream `reth_jit` method.

use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_evm::jit::JitBackendControl;
use async_trait::async_trait;
use jsonrpsee::{
    core::{RegisterMethodError, RpcResult},
    proc_macros::rpc,
};
use jsonrpsee_types::error::{ErrorCode, ErrorObjectOwned};
use reth_rpc_builder::TransportRpcModules;
use reth_rpc_server_types::RethRpcModule;
use serde::{Deserialize, Serialize};
use tracing::error;

/// RPC interface for controlling the shared revmc JIT backend.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "reth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "reth"))]
pub trait RethJitApi {
    /// Applies a runtime control action to the revmc JIT backend.
    #[method(name = "jit")]
    async fn reth_jit(&self, action: RethJitAction) -> RpcResult<()>;
}

/// Supported `reth_jit` runtime control actions.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RethJitAction {
    /// Enables JIT lookups and background compilation.
    Enable,
    /// Disables JIT lookups and background compilation.
    Disable,
    /// Pauses out-of-process helper execution without discarding compiled code.
    Pause,
    /// Resumes out-of-process helper execution.
    Unpause,
    /// Clears resident and persisted compiled artifacts.
    Clear,
}

/// `reth_jit` RPC implementation backed by the node's EVM configuration.
#[derive(Clone, Debug)]
pub struct RethJitExt {
    /// EVM configuration that owns the shared revmc backend.
    evm_config: TaikoEvmConfig,
}

impl RethJitExt {
    /// Creates a JIT control extension for the supplied EVM configuration.
    pub const fn new(evm_config: TaikoEvmConfig) -> Self {
        Self { evm_config }
    }
}

/// Applies one RPC action to a backend.
fn apply_action(backend: &dyn JitBackendControl, action: RethJitAction) -> Result<(), String> {
    match action {
        RethJitAction::Enable => backend.set_enabled(true),
        RethJitAction::Disable => backend.set_enabled(false),
        RethJitAction::Pause => {
            backend.pause();
            Ok(())
        }
        RethJitAction::Unpause => {
            backend.resume();
            Ok(())
        }
        RethJitAction::Clear => {
            backend.clear();
            Ok(())
        }
    }
}

/// Converts a backend control failure into a non-sensitive JSON-RPC internal error.
fn internal_error(message: String) -> ErrorObjectOwned {
    error!(%message, "failed to control revmc JIT backend");
    ErrorObjectOwned::owned(
        ErrorCode::InternalError.code(),
        "failed to control JIT backend",
        None::<()>,
    )
}

#[async_trait]
impl RethJitApiServer for RethJitExt {
    /// Applies a runtime control action when a JIT backend is available.
    async fn reth_jit(&self, action: RethJitAction) -> RpcResult<()> {
        let Some(backend) = self.evm_config.jit_backend() else {
            return Ok(());
        };

        apply_action(backend, action).map_err(internal_error)
    }
}

/// Installs `reth_jit` only on transports where the `reth` namespace is configured.
pub fn install_reth_jit_rpc(
    modules: &mut TransportRpcModules,
    evm_config: TaikoEvmConfig,
) -> Result<(), RegisterMethodError> {
    modules.add_or_replace_if_module_configured(
        RethRpcModule::Reth,
        RethJitExt::new(evm_config).into_rpc(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug, Default)]
    struct RecordingBackend {
        actions: Mutex<Vec<&'static str>>,
    }

    impl JitBackendControl for RecordingBackend {
        fn set_enabled(&self, enabled: bool) -> Result<(), String> {
            self.actions.lock().expect("actions lock").push(if enabled {
                "enable"
            } else {
                "disable"
            });
            Ok(())
        }

        fn pause(&self) {
            self.actions.lock().expect("actions lock").push("pause");
        }

        fn resume(&self) {
            self.actions.lock().expect("actions lock").push("unpause");
        }

        fn clear(&self) {
            self.actions.lock().expect("actions lock").push("clear");
        }
    }

    #[test]
    fn registers_upstream_method_name() {
        let module =
            RethJitExt::new(TaikoEvmConfig::new(alethia_reth_chainspec::TAIKO_MAINNET.clone()))
                .into_rpc();

        assert!(module.method_names().any(|method| method == "reth_jit"));
    }

    #[test]
    fn dispatches_all_runtime_actions() {
        let backend = RecordingBackend::default();

        for action in [
            RethJitAction::Enable,
            RethJitAction::Disable,
            RethJitAction::Pause,
            RethJitAction::Unpause,
            RethJitAction::Clear,
        ] {
            apply_action(&backend, action).expect("action should succeed");
        }

        assert_eq!(
            *backend.actions.lock().expect("actions lock"),
            ["enable", "disable", "pause", "unpause", "clear"]
        );
    }
}
