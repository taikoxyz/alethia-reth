//! Inspector-side zk gas metering for opcode execution.

use std::sync::{Arc, Mutex, MutexGuard};

use reth_revm::{
    Inspector,
    context::{ContextError, ContextTr, JournalTr},
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter,
        interpreter::EthInterpreter, interpreter_types::Jumps,
    },
};

use crate::{alloy::TaikoEvmContext, spec::TaikoSpecId};

use super::{
    meter::{ZkGasMeter, ZkGasOutcome},
    schedule::{ZkGasSchedule, schedule_for},
};

/// Dedicated custom error string emitted when zk gas accounting exceeds the block limit.
pub const ZK_GAS_LIMIT_ERR: &str = "zk gas limit exceeded";

/// Shared zk gas meter handle used by both the inspector and wrapped precompiles.
pub type SharedZkGasMeter = Arc<Mutex<ZkGasMeter<'static>>>;

/// Returns a freshly initialized shared meter handle for the requested spec when metering is
/// active.
pub fn shared_meter_for_spec(spec: TaikoSpecId) -> Option<SharedZkGasMeter> {
    schedule_for(spec).map(|schedule| Arc::new(Mutex::new(ZkGasMeter::new(schedule))))
}

/// Composite inspector that meters zk gas before delegating to an inner inspector.
pub struct ZkGasInspector<I> {
    /// User-provided or factory-provided inner inspector.
    inner: I,
    /// Optional metering state. `None` keeps all non-metered execution on the pass-through path.
    metering: Option<UzenMeteringState>,
}

impl<I> ZkGasInspector<I> {
    /// Creates a new composite inspector around `inner` and the optional shared meter handle.
    pub fn new(inner: I, meter: Option<SharedZkGasMeter>) -> Self {
        let metering = meter.map(UzenMeteringState::new);
        Self { inner, metering }
    }

    /// Returns a shared reference to the wrapped inner inspector.
    pub const fn inner(&self) -> &I {
        &self.inner
    }

    /// Returns a mutable reference to the wrapped inner inspector.
    pub fn inner_mut(&mut self) -> &mut I {
        &mut self.inner
    }

    /// Returns the shared meter handle when zk gas metering is active for this inspector.
    pub fn shared_meter(&self) -> Option<SharedZkGasMeter> {
        self.metering.as_ref().map(|state| Arc::clone(&state.meter))
    }
}

impl<DB, I> Inspector<TaikoEvmContext<DB>, EthInterpreter> for ZkGasInspector<I>
where
    DB: reth_revm::Database,
    I: Inspector<TaikoEvmContext<DB>, EthInterpreter>,
{
    /// Initializes the wrapped inner inspector before execution enters a frame.
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut TaikoEvmContext<DB>,
    ) {
        self.inner.initialize_interp(interp, context);
    }

    /// Flushes any deferred spawn-opcode charge before starting the next opcode.
    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut TaikoEvmContext<DB>,
    ) {
        if let Some(metering) = &mut self.metering {
            if let Err(ZkGasOutcome::LimitExceeded) = metering.flush_deferred_steps() {
                set_custom_error(context);
                interp.halt_fatal();
                return;
            }
            metering.begin_step(
                context.journal().depth(),
                interp.bytecode.opcode(),
                interp.gas.remaining(),
            );
        }
        self.inner.step(interp, context);
    }

    /// Charges ordinary opcodes immediately and defers CALL/CREATE-family charging until dispatch
    /// resolves.
    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut TaikoEvmContext<DB>,
    ) {
        self.inner.step_end(interp, context);

        let Some(metering) = &mut self.metering else {
            return;
        };
        let depth = context.journal().depth();
        let Some(step) = metering.finish_step(depth, interp.gas.remaining()) else {
            return;
        };

        if is_spawn_opcode(step.opcode) {
            metering.defer_step(depth, step);
            return;
        }

        if let Err(ZkGasOutcome::LimitExceeded) = charge_finished_step(&metering.meter, step) {
            set_custom_error(context);
            interp.halt_fatal();
        }
    }

    /// Marks CALL-family steps that actually opened a child frame.
    fn call(
        &mut self,
        context: &mut TaikoEvmContext<DB>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        let outcome = self.inner.call(context, inputs);
        if outcome.is_none() &&
            let Some(metering) = &mut self.metering
        {
            metering.mark_call_spawn(context.journal().depth());
        }
        outcome
    }

    /// Marks precompile dispatches and flushes deferred CALL-family charges once the call outcome
    /// is known.
    fn call_end(
        &mut self,
        context: &mut TaikoEvmContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        let was_precompile_called = outcome.was_precompile_called;
        self.inner.call_end(context, inputs, outcome);

        if let Some(metering) = &mut self.metering {
            if was_precompile_called {
                metering.mark_call_spawn(context.journal().depth());
            }
            if let Err(ZkGasOutcome::LimitExceeded) = metering.flush_deferred_steps() {
                set_custom_error(context);
                return;
            }
            if was_precompile_called {
                let gas_used = inputs.gas_limit.saturating_sub(outcome.result.gas.remaining());
                let address_low_byte = inputs.bytecode_address.as_slice()[19];
                if let Err(ZkGasOutcome::LimitExceeded) =
                    lock_meter(&metering.meter).charge_precompile(address_low_byte, gas_used)
                {
                    set_custom_error(context);
                }
            }
        }
    }

    /// Marks CREATE-family steps that actually opened a child frame.
    fn create(
        &mut self,
        context: &mut TaikoEvmContext<DB>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        let outcome = self.inner.create(context, inputs);
        if outcome.is_none() &&
            let Some(metering) = &mut self.metering
        {
            metering.mark_create_spawn(context.journal().depth());
        }
        outcome
    }

    /// Flushes deferred CREATE-family charges once the create outcome is known.
    fn create_end(
        &mut self,
        context: &mut TaikoEvmContext<DB>,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.inner.create_end(context, inputs, outcome);

        if let Some(metering) = &mut self.metering &&
            let Err(ZkGasOutcome::LimitExceeded) = metering.flush_deferred_steps()
        {
            set_custom_error(context);
        }
    }
}

/// Per-inspector metering state shared across opcode callbacks.
struct UzenMeteringState {
    /// Shared checked meter that owns the schedule and accumulated usage.
    meter: SharedZkGasMeter,
    /// Per-frame in-flight opcode step state keyed by journal depth.
    pending_steps: Vec<Option<PendingStep>>,
    /// Completed CALL/CREATE-family steps waiting for spawn information before charging.
    deferred_steps: Vec<Option<FinishedStep>>,
}

impl UzenMeteringState {
    /// Creates new per-inspector state around the provided shared meter.
    fn new(meter: SharedZkGasMeter) -> Self {
        Self { meter, pending_steps: Vec::new(), deferred_steps: Vec::new() }
    }

    /// Records the opcode and gas snapshot for the current frame depth.
    fn begin_step(&mut self, depth: usize, opcode: u8, gas_remaining: u64) {
        self.ensure_depth(depth);
        self.pending_steps[depth] = Some(PendingStep { opcode, gas_remaining, spawned: false });
    }

    /// Marks that the current CALL-family opcode actually dispatched child work.
    fn mark_call_spawn(&mut self, depth: usize) {
        self.mark_spawn(depth, is_call_opcode);
        self.mark_spawn(parent_step_depth(depth), is_call_opcode);
    }

    /// Marks that the current CREATE-family opcode actually dispatched child work.
    fn mark_create_spawn(&mut self, depth: usize) {
        self.mark_spawn(depth, is_create_opcode);
        self.mark_spawn(parent_step_depth(depth), is_create_opcode);
    }

    /// Finalizes the current step state and returns the completed metering record.
    fn finish_step(&mut self, depth: usize, gas_remaining: u64) -> Option<FinishedStep> {
        let pending = self.pending_steps.get_mut(depth)?.take()?;
        Some(FinishedStep {
            schedule: meter_schedule(&self.meter),
            opcode: pending.opcode,
            step_gas: pending.gas_remaining.saturating_sub(gas_remaining),
            spawned: pending.spawned,
        })
    }

    /// Stores a finished spawn opcode until frame-resolution hooks can determine its raw gas
    /// source.
    fn defer_step(&mut self, depth: usize, step: FinishedStep) {
        self.ensure_depth(depth);
        self.deferred_steps[depth] = Some(step);
    }

    /// Charges and clears every deferred spawn opcode.
    fn flush_deferred_steps(&mut self) -> Result<(), ZkGasOutcome> {
        for deferred in &mut self.deferred_steps {
            if let Some(step) = deferred.take() {
                charge_finished_step(&self.meter, step)?;
            }
        }
        Ok(())
    }

    /// Ensures the per-depth vectors can store state for `depth`.
    fn ensure_depth(&mut self, depth: usize) {
        if self.pending_steps.len() <= depth {
            self.pending_steps.resize_with(depth + 1, || None);
        }
        if self.deferred_steps.len() <= depth {
            self.deferred_steps.resize_with(depth + 1, || None);
        }
    }

    /// Marks pending or deferred spawn steps when the opcode matches the expected family.
    fn mark_spawn(&mut self, depth: usize, predicate: fn(u8) -> bool) {
        if let Some(Some(pending)) = self.pending_steps.get_mut(depth) &&
            predicate(pending.opcode)
        {
            pending.spawned = true;
        }
        if let Some(Some(deferred)) = self.deferred_steps.get_mut(depth) &&
            predicate(deferred.opcode)
        {
            deferred.spawned = true;
        }
    }
}

/// Per-frame state captured between `step` and `step_end`.
#[derive(Clone, Copy)]
struct PendingStep {
    /// Opcode byte currently executing in the frame.
    opcode: u8,
    /// Remaining EVM gas observed before the opcode executed.
    gas_remaining: u64,
    /// Whether this opcode already proved that it dispatched child work.
    spawned: bool,
}

/// Completed metering record for a single opcode step.
#[derive(Clone, Copy)]
struct FinishedStep {
    /// Consensus-owned schedule backing the shared meter.
    schedule: &'static ZkGasSchedule,
    /// Opcode byte that was just executed.
    opcode: u8,
    /// Raw EVM gas spent by the opcode step on the interpreter path.
    step_gas: u64,
    /// Whether the opcode dispatched child work and should use the fixed spawn estimate.
    spawned: bool,
}

/// Returns `true` when `opcode` is a CALL-family spawn opcode.
fn is_call_opcode(opcode: u8) -> bool {
    matches!(opcode, 0xf1 | 0xf2 | 0xf4 | 0xfa)
}

/// Returns `true` when `opcode` is a CREATE-family spawn opcode.
fn is_create_opcode(opcode: u8) -> bool {
    matches!(opcode, 0xf0 | 0xf5)
}

/// Returns `true` when `opcode` belongs to the spawn-opcode set.
fn is_spawn_opcode(opcode: u8) -> bool {
    is_call_opcode(opcode) || is_create_opcode(opcode)
}

/// Returns the caller-frame depth that owns the current spawn-opcode step.
fn parent_step_depth(depth: usize) -> usize {
    depth.saturating_sub(1)
}

/// Returns the fixed spawn estimate for the provided Uzen opcode.
fn spawn_estimate(schedule: &'static ZkGasSchedule, opcode: u8) -> u64 {
    match opcode {
        0xf1 => schedule.spawn_estimates.call,
        0xf2 => schedule.spawn_estimates.callcode,
        0xf4 => schedule.spawn_estimates.delegatecall,
        0xfa => schedule.spawn_estimates.staticcall,
        0xf0 => schedule.spawn_estimates.create,
        0xf5 => schedule.spawn_estimates.create2,
        _ => unreachable!("spawn estimate requested for non-spawn opcode: {opcode:#x}"),
    }
}

/// Returns the schedule backing a shared meter handle.
fn meter_schedule(_meter: &SharedZkGasMeter) -> &'static ZkGasSchedule {
    schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule is available")
}

/// Charges a completed opcode step against the shared meter.
fn charge_finished_step(
    meter: &SharedZkGasMeter,
    step: FinishedStep,
) -> Result<(), ZkGasOutcome> {
    let raw_gas =
        if step.spawned { spawn_estimate(step.schedule, step.opcode) } else { step.step_gas };
    lock_meter(meter).charge_opcode(step.opcode, raw_gas)
}

/// Sets the dedicated custom zk gas limit error on the EVM context when none is present yet.
fn set_custom_error<CTX: ContextTr>(context: &mut CTX) {
    let err_slot = context.error();
    if err_slot.is_ok() {
        *err_slot = Err(ContextError::Custom(ZK_GAS_LIMIT_ERR.to_string()));
    }
}

/// Locks the shared meter while recovering cleanly from poison.
pub(crate) fn lock_meter(meter: &SharedZkGasMeter) -> MutexGuard<'_, ZkGasMeter<'static>> {
    meter.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
