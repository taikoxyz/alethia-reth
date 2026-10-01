//! Inspector-side zk gas metering for opcode execution.
//!
//! High-level flow:
//! 1. Capture the opcode and pre-step gas in `step`.
//! 2. Resolve the step into a `FinishedStep` in `step_end`.
//! 3. Read the interpreter action in `step_end`: `NewFrame` uses the spawn estimate, while every
//!    other result uses the measured interpreter delta, matching the production plain loop.

use alloy_primitives::{Address, Log, U256};
use reth_revm::{
    Inspector,
    context::{ContextTr, JournalTr},
    handler::FrameResult,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Interpreter,
        InterpreterAction,
        interpreter::EthInterpreter,
        interpreter_types::{Jumps, LoopControl},
    },
};

use crate::alloy::TaikoEvmContext;

use super::{
    meter::{ZkGasMeter, ZkGasOutcome, is_spawn_opcode},
    runtime::{halt_for_zk_gas_limit, set_custom_error},
    schedule::ZkGasSchedule,
};

/// Dedicated custom error string emitted when zk gas accounting exceeds the block limit.
pub const ZK_GAS_LIMIT_ERR: &str = "zk gas limit exceeded";

/// Upper bound on the EVM call-frame depth recorded by the inspector.
///
/// revm's `CALL_STACK_LIMIT` is 1024 and is enforced as `depth > CALL_STACK_LIMIT`
/// against the child's depth in `make_call_frame`, so the deepest legal frame has
/// `journal().depth() == 1024`. Direct indexing of `0..=1024` needs 1025 slots; we
/// size the per-depth arrays at 1026 to keep one safety slot and preserve the
/// per-opcode bounds-check-free hot path.
const MAX_CALL_DEPTH: usize = 1026;

/// Composite inspector that meters zk gas before delegating to an inner inspector.
pub struct ZkGasInspector<I> {
    /// User-provided or factory-provided inner inspector.
    inner: I,
    /// Optional metering state. `None` keeps all non-metered execution on the pass-through path.
    metering: Option<ZkGasMeteringState>,
}

impl<I> ZkGasInspector<I> {
    /// Creates a new composite inspector around `inner` and the optional zk gas schedule.
    pub fn new(inner: I, schedule: Option<&'static ZkGasSchedule>) -> Self {
        let metering = schedule.map(ZkGasMeteringState::new);
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

    /// Returns a reference to the active zk gas meter, if metering is enabled.
    ///
    /// Returns `None` when the active spec has no zk gas schedule (pre-Unzen specs).
    pub(crate) fn meter(&self) -> Option<&ZkGasMeter<'static>> {
        self.metering.as_ref().map(|state| &state.meter)
    }

    /// Returns a mutable reference to the active zk gas meter, if metering is enabled.
    ///
    /// Returns `None` when the active spec has no zk gas schedule (pre-Unzen specs).
    pub(crate) fn meter_mut(&mut self) -> Option<&mut ZkGasMeter<'static>> {
        self.metering.as_mut().map(|state| &mut state.meter)
    }

    /// Discards in-flight transaction metering state, if metering is enabled.
    ///
    /// Clears both the meter's per-transaction usage and the inspector's step bookkeeping so
    /// nothing recorded by an aborted transaction can charge into the next one.
    pub(crate) fn reset_transaction(&mut self) {
        if let Some(metering) = &mut self.metering {
            metering.reset_transaction();
        }
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

    /// Captures the opcode and its pre-step EVM gas snapshot.
    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut TaikoEvmContext<DB>,
    ) {
        if let Some(metering) = &mut self.metering {
            // Snapshot the opcode and remaining gas before the interpreter mutates frame state.
            metering.begin_step(
                context.journal().depth(),
                interp.bytecode.opcode(),
                interp.gas.remaining(),
            );
        }
        self.inner.step(interp, context);
    }

    /// Charges every completed opcode from the same post-step action used by the production loop.
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
        // Pair the pre-step snapshot captured in `step` with the post-step gas remaining.
        let mut step = metering.finish_step(depth, interp.gas.remaining());
        step.spawned = is_spawn_opcode(step.opcode) &&
            matches!(interp.bytecode.action(), Some(InterpreterAction::NewFrame(_)));

        // NewFrame has been selected but not dispatched yet, so a failed wrapper charge can still
        // replace the action before any child contract or precompile executes.
        if metering.charge_finished_step(step).is_err() {
            halt_for_zk_gas_limit(context, interp);
        }
    }

    /// Forwards emitted logs to the wrapped inner inspector.
    fn log(&mut self, context: &mut TaikoEvmContext<DB>, log: Log) {
        self.inner.log(context, log);
    }

    /// Forwards emitted logs (with interpreter access) to the wrapped inner inspector.
    fn log_full(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut TaikoEvmContext<DB>,
        log: Log,
    ) {
        self.inner.log_full(interp, context, log);
    }

    /// Forwards the generic frame-start hook to the wrapped inner inspector.
    fn frame_start(
        &mut self,
        context: &mut TaikoEvmContext<DB>,
        frame_input: &mut FrameInput,
    ) -> Option<FrameResult> {
        self.inner.frame_start(context, frame_input)
    }

    /// Forwards CALL-family frame initialization to the wrapped inspector.
    fn call(
        &mut self,
        context: &mut TaikoEvmContext<DB>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        self.inner.call(context, inputs)
    }

    /// Charges completed precompile work separately from its already-metered CALL wrapper.
    fn call_end(
        &mut self,
        context: &mut TaikoEvmContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        let was_precompile_called = outcome.was_precompile_called;
        self.inner.call_end(context, inputs, outcome);

        if let Some(metering) = &mut self.metering &&
            was_precompile_called
        {
            // Precompile usage is charged separately from the CALL opcode itself, keyed by the
            // full precompile address.
            let gas_used = inputs.gas_limit.saturating_sub(outcome.result.gas.remaining());
            if let Err(ZkGasOutcome::LimitExceeded) =
                metering.meter.charge_precompile(&inputs.bytecode_address, gas_used)
            {
                set_custom_error(context);
            }
        }
    }

    /// Forwards CREATE-family frame initialization to the wrapped inspector.
    fn create(
        &mut self,
        context: &mut TaikoEvmContext<DB>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        self.inner.create(context, inputs)
    }

    /// Forwards CREATE-family completion to the wrapped inspector.
    fn create_end(
        &mut self,
        context: &mut TaikoEvmContext<DB>,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.inner.create_end(context, inputs, outcome);
    }

    /// Forwards the generic frame-end hook to the wrapped inner inspector.
    fn frame_end(
        &mut self,
        context: &mut TaikoEvmContext<DB>,
        frame_input: &FrameInput,
        frame_result: &mut FrameResult,
    ) {
        self.inner.frame_end(context, frame_input, frame_result);
    }

    /// Forwards selfdestruct notifications to the wrapped inner inspector.
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.inner.selfdestruct(contract, target, value);
    }
}

/// Per-inspector metering state carried across opcode callbacks.
struct ZkGasMeteringState {
    /// Owned checked meter that holds the schedule and accumulated usage.
    meter: ZkGasMeter<'static>,
    /// Per-frame in-flight opcode step state keyed by journal depth.
    pending_steps: [PendingStep; MAX_CALL_DEPTH],
    /// Highest journal depth ever observed in this state's lifetime.
    /// Bounds transaction reset work to the actual call depth reached.
    max_active_depth: usize,
}

impl ZkGasMeteringState {
    /// Creates new per-inspector state for the provided schedule.
    fn new(schedule: &'static ZkGasSchedule) -> Self {
        Self {
            meter: ZkGasMeter::new(schedule),
            pending_steps: [PendingStep::EMPTY; MAX_CALL_DEPTH],
            max_active_depth: 0,
        }
    }

    /// Discards the in-flight transaction zk gas together with all per-frame step bookkeeping.
    ///
    /// Resetting the per-depth snapshots here keeps aborted execution from leaking into the next
    /// transaction regardless of how REVM unwound its frames.
    fn reset_transaction(&mut self) {
        self.meter.reset_transaction();
        for index in 0..=self.max_active_depth {
            self.pending_steps[index] = PendingStep::EMPTY;
        }
        self.max_active_depth = 0;
    }

    /// Records the opcode and gas snapshot for the current frame depth.
    #[inline(always)]
    fn begin_step(&mut self, depth: usize, opcode: u8, gas_remaining: u64) {
        // Any previous pending step at this depth must already have been consumed by `step_end`.
        self.pending_steps[depth] = PendingStep { opcode, gas_remaining };
        if depth > self.max_active_depth {
            self.max_active_depth = depth;
        }
    }

    /// Finalizes the current step state and returns the completed metering record.
    #[inline(always)]
    fn finish_step(&mut self, depth: usize, gas_remaining: u64) -> FinishedStep {
        let pending = self.pending_steps[depth];
        FinishedStep {
            opcode: pending.opcode,
            step_gas: pending.gas_remaining.saturating_sub(gas_remaining),
            spawned: false,
        }
    }

    /// Charges a completed opcode step against the active meter.
    #[inline(always)]
    fn charge_finished_step(&mut self, step: FinishedStep) -> Result<(), ZkGasOutcome> {
        // Spawn opcodes use the fixed consensus estimate only when they selected a child frame.
        // Otherwise we charge the measured interpreter gas delta from this opcode step.
        if step.spawned {
            self.meter.charge_spawn_opcode(step.opcode)
        } else {
            self.charge_opcode(step.opcode, step.step_gas)
        }
    }

    /// Charges a measured opcode against the active meter.
    #[inline(always)]
    fn charge_opcode(&mut self, opcode: u8, raw_gas: u64) -> Result<(), ZkGasOutcome> {
        self.meter.charge_opcode(opcode, raw_gas)
    }
}

/// Per-frame state captured between `step` and `step_end`.
#[derive(Clone, Copy)]
struct PendingStep {
    /// Opcode byte currently executing in the frame.
    opcode: u8,
    /// Remaining EVM gas observed before the opcode executed.
    gas_remaining: u64,
}

impl PendingStep {
    /// Empty placeholder overwritten by `begin_step` before `finish_step` reads a depth.
    const EMPTY: Self = Self { opcode: 0, gas_remaining: 0 };
}

/// Completed metering record for a single opcode step.
#[derive(Clone, Copy)]
struct FinishedStep {
    /// Opcode byte that was just executed.
    opcode: u8,
    /// Raw EVM gas spent by the opcode step on the interpreter path.
    step_gas: u64,
    /// Whether the opcode selected `NewFrame` and should use the fixed spawn estimate.
    spawned: bool,
}

#[cfg(test)]
mod tests {
    use crate::zk_gas::unzen::UNZEN_ZK_GAS_SCHEDULE;

    use super::{FinishedStep, ZkGasMeteringState};

    #[test]
    fn reset_transaction_clears_meter_and_step_bookkeeping() {
        let mut metering = ZkGasMeteringState::new(&UNZEN_ZK_GAS_SCHEDULE);
        metering.begin_step(1, 0x01, 10);
        metering.meter.charge_opcode(0x01, 3).expect("charge fits");

        metering.reset_transaction();

        assert_eq!(metering.meter.tx_zk_gas_used(), 0);
        assert_eq!(metering.pending_steps[1].opcode, 0);
        assert_eq!(metering.pending_steps[1].gas_remaining, 0);
        assert_eq!(metering.max_active_depth, 0);
    }

    #[test]
    fn charge_finished_step_uses_active_meter_schedule_for_spawn_estimate() {
        let mut metering = ZkGasMeteringState::new(&UNZEN_ZK_GAS_SCHEDULE);

        metering
            .charge_finished_step(FinishedStep { opcode: 0xf0, step_gas: 1, spawned: true })
            .expect("spawn estimate should fit");

        assert_eq!(
            metering.meter.tx_zk_gas_used(),
            UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.create *
                u64::from(UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers[0xf0])
        );
    }
}
