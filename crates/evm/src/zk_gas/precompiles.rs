//! Wrapped precompile provider that meters Uzen precompile execution.

use std::sync::Arc;

use reth_revm::{
    context::ContextTr,
    handler::PrecompileProvider,
    interpreter::{CallInputs, InterpreterResult},
    primitives::Address,
};

use super::{adapter::{SharedUzenZkGasMeter, UZEN_ZK_GAS_LIMIT_ERR}, meter::ZkGasOutcome};

/// Composite precompile provider that meters Uzen precompile gas after execution.
#[derive(Clone)]
pub struct UzenZkGasPrecompiles<P> {
    /// Wrapped inner precompile provider.
    inner: P,
    /// Optional shared Uzen meter handle. `None` keeps non-Uzen execution on the pass-through path.
    meter: Option<SharedUzenZkGasMeter>,
}

impl<P> UzenZkGasPrecompiles<P> {
    /// Creates a new wrapped precompile provider around `inner` and the optional shared meter.
    pub fn new(inner: P, meter: Option<SharedUzenZkGasMeter>) -> Self {
        Self { inner, meter }
    }

    /// Returns a shared reference to the wrapped precompile provider.
    pub const fn inner(&self) -> &P {
        &self.inner
    }

    /// Returns a mutable reference to the wrapped precompile provider.
    pub fn inner_mut(&mut self) -> &mut P {
        &mut self.inner
    }

    /// Returns the shared meter handle when Uzen metering is active for this provider.
    pub fn shared_meter(&self) -> Option<SharedUzenZkGasMeter> {
        self.meter.as_ref().map(Arc::clone)
    }
}

impl<CTX, P> PrecompileProvider<CTX> for UzenZkGasPrecompiles<P>
where
    CTX: ContextTr,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    /// Interpreter output returned by the wrapped precompile provider.
    type Output = InterpreterResult;

    /// Delegates spec updates to the wrapped provider.
    fn set_spec(&mut self, spec: <CTX::Cfg as reth_revm::context::Cfg>::Spec) -> bool {
        self.inner.set_spec(spec)
    }

    /// Runs the wrapped precompile provider and charges Uzen precompile gas after execution.
    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        let result = self.inner.run(context, inputs)?;
        let Some(result) = result else {
            return Ok(None);
        };

        if let Some(meter) = &self.meter {
            let gas_used = inputs.gas_limit.saturating_sub(result.gas.remaining());
            let address_low_byte = inputs.bytecode_address.as_slice()[19];
            if let Err(ZkGasOutcome::LimitExceeded) =
                lock_meter(meter).charge_precompile(address_low_byte, gas_used)
            {
                return Err(UZEN_ZK_GAS_LIMIT_ERR.to_string());
            }
        }

        let _ = context;
        Ok(Some(result))
    }

    /// Delegates warm-address iteration to the wrapped provider.
    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        self.inner.warm_addresses()
    }

    /// Delegates precompile membership checks to the wrapped provider.
    fn contains(&self, address: &Address) -> bool {
        self.inner.contains(address)
    }
}

/// Locks the shared meter while recovering cleanly from poison.
fn lock_meter(
    meter: &SharedUzenZkGasMeter,
) -> std::sync::MutexGuard<'_, super::meter::UzenZkGasMeter<'static>> {
    meter.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
