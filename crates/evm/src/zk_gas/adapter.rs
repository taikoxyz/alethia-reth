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
        interpreter_types::{InputsTr, Jumps, LegacyBytecode, LoopControl},
    },
};
use std::collections::BTreeMap;

use crate::alloy::TaikoEvmContext;

use super::{
    meter::{ZkGasMeter, ZkGasOutcome, is_spawn_opcode},
    runtime::{PendingV2Step, bn254_work_units, halt_for_zk_gas_limit, set_custom_error},
    schedule::ZkGasSchedule,
    v2::{StateAccess, ZkGasV2Accumulator, ZkGasV2AccumulatorError, ZkGasV2Error},
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
        let metering = schedule.map(|schedule| ZkGasMeteringState::new(Some(schedule)));
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
        self.metering.as_ref()?.meter.as_ref()
    }

    /// Returns a mutable reference to the active zk gas meter, if metering is enabled.
    ///
    /// Returns `None` when the active spec has no zk gas schedule (pre-Unzen specs).
    pub(crate) fn meter_mut(&mut self) -> Option<&mut ZkGasMeter<'static>> {
        self.metering.as_mut()?.meter.as_mut()
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

    /// Enables schema-9 selector collection on the inspected execution path.
    pub(crate) fn enable_v2(&mut self, enabled: bool) {
        if enabled {
            self.metering.get_or_insert_with(|| ZkGasMeteringState::new(None)).v2 =
                Some(ZkGasV2Accumulator::default());
        } else if let Some(metering) = &mut self.metering {
            metering.v2 = None;
            if metering.meter.is_none() {
                self.metering = None;
            }
        }
    }

    /// Returns the inspected-path V2 collector, when enabled.
    pub(crate) fn v2(&self) -> Option<&ZkGasV2Accumulator> {
        self.metering.as_ref()?.v2.as_ref()
    }

    /// Returns the mutable inspected-path V2 collector, when enabled.
    pub(crate) fn v2_mut(&mut self) -> Option<&mut ZkGasV2Accumulator> {
        self.metering.as_mut()?.v2.as_mut()
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
        if let Some(accumulator) = self.metering.as_mut().and_then(|state| state.v2.as_mut()) {
            for address in [interp.input.caller_address(), interp.input.target_address()] {
                let _ = accumulator.observe_state_access(StateAccess::Account(address));
            }
            if let Some(address) = interp.input.bytecode_address() {
                let _ = accumulator.observe_state_access(StateAccess::Account(*address));
            }
            let code_hash = interp.bytecode.get_or_calculate_hash();
            let byte_length = interp.bytecode.bytecode_slice().len() as u64;
            let _ = accumulator
                .observe_state_access(StateAccess::Code { hash: code_hash, byte_length });
        }
    }

    /// Captures the opcode and its pre-step EVM gas snapshot.
    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut TaikoEvmContext<DB>,
    ) {
        self.inner.step(interp, context);
        if let Some(metering) = &mut self.metering {
            // Capture after the wrapped inspector's pre-step callback so selectors describe the
            // state that the interpreter will actually execute.
            metering.begin_step(context.journal().depth(), interp, context);
        }
    }

    /// Charges every completed opcode from the same post-step action used by the production loop.
    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut TaikoEvmContext<DB>,
    ) {
        let Some(metering) = &mut self.metering else {
            self.inner.step_end(interp, context);
            return;
        };
        let depth = context.journal().depth();
        // Pair the pre-step snapshot captured in `step` with the post-step gas remaining.
        let mut step = metering.finish_step(depth, interp.gas.remaining());
        step.spawned = is_spawn_opcode(step.opcode) &&
            matches!(interp.bytecode.action(), Some(InterpreterAction::NewFrame(_)));

        // NewFrame has been selected but not dispatched yet, so a failed wrapper charge can still
        // replace the action before any child contract or precompile executes.
        if let Err(ZkGasOutcome::LimitExceeded) = metering.charge_finished_step(step) {
            halt_for_zk_gas_limit(context, interp);
            self.inner.step_end(interp, context);
            return;
        }
        if let Some(v2_step) = step.v2 &&
            let Some(accumulator) = metering.v2.as_mut() &&
            let Err(error) = v2_step.finish(interp, context, accumulator)
        {
            accumulator.retain_diagnostic(Some(error));
        }
        // Post-step callbacks run after all metering snapshots so a user inspector cannot rewrite
        // the executed action or gas delta before they are attributed. Selector parity is promised
        // for observational inspectors; an inspector that mutates execution in `step` is metered
        // according to the state it leaves for the interpreter.
        self.inner.step_end(interp, context);
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
        let outcome = self.inner.call(context, inputs);
        if let Some(metering) = &mut self.metering {
            let work_units = (inputs.bytecode_address == Address::with_last_byte(0x07))
                .then(|| bn254_work_units(&inputs.input.bytes(context)));
            metering.pending_precompile_work.push(work_units);
            if let Some(accumulator) = metering.v2.as_mut() {
                if let Err(error) = accumulator.confirm_spawn() {
                    accumulator.retain_diagnostic(Some(error));
                }
                for address in [inputs.caller, inputs.target_address, inputs.bytecode_address] {
                    let _ = accumulator.observe_state_access(StateAccess::Account(address));
                }
            }
        }
        outcome
    }

    /// Charges completed precompile work separately from its already-metered CALL wrapper.
    fn call_end(
        &mut self,
        context: &mut TaikoEvmContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        // Snapshot completed precompile work before the wrapped inspector can rewrite the outcome
        // (for example, a tracer callback failure can replace it with a zero-gas revert).
        let precompile_gas_used = outcome
            .was_precompile_called
            .then(|| inputs.gas_limit.saturating_sub(outcome.result.gas.remaining()));
        self.inner.call_end(context, inputs, outcome);

        if let Some(metering) = &mut self.metering &&
            let Some(gas_used) = precompile_gas_used
        {
            // Precompile usage is charged separately from the CALL opcode itself, keyed by the
            // full precompile address.
            if let Some(meter) = &mut metering.meter &&
                let Err(ZkGasOutcome::LimitExceeded) =
                    meter.charge_precompile(&inputs.bytecode_address, gas_used)
            {
                set_custom_error(context);
            }
            let work_units = metering.pending_precompile_work.pop().flatten();
            if let Some(accumulator) = metering.v2.as_mut() {
                let address = U256::from_be_slice(inputs.bytecode_address.as_slice());
                match u16::try_from(address) {
                    Ok(address) => {
                        let _ = accumulator.observe_precompile(address, gas_used, work_units);
                    }
                    Err(_) => {
                        accumulator.retain_diagnostic(Some(ZkGasV2AccumulatorError::Pricing(
                            ZkGasV2Error::InvalidInput("precompile_address"),
                        )))
                    }
                }
            }
        } else if let Some(metering) = &mut self.metering {
            let _ = metering.pending_precompile_work.pop();
        }
    }

    /// Forwards CREATE-family frame initialization to the wrapped inspector.
    fn create(
        &mut self,
        context: &mut TaikoEvmContext<DB>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        let outcome = self.inner.create(context, inputs);
        if let Some(accumulator) = self.metering.as_mut().and_then(|state| state.v2.as_mut()) {
            if let Err(error) = accumulator.confirm_spawn() {
                accumulator.retain_diagnostic(Some(error));
            }
            let _ = accumulator.observe_state_access(StateAccess::Account(inputs.caller()));
        }
        outcome
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
    meter: Option<ZkGasMeter<'static>>,
    /// Per-frame in-flight opcode step state keyed by journal depth.
    pending_steps: [PendingStep; MAX_CALL_DEPTH],
    /// Typed snapshots allocated only for frame depths actually executed with V2 enabled.
    pending_v2_steps: BTreeMap<usize, PendingV2Step>,
    /// Highest journal depth ever observed in this state's lifetime.
    /// Bounds transaction reset work to the actual call depth reached.
    max_active_depth: usize,
    /// Optional schema-9 selector collector.
    v2: Option<ZkGasV2Accumulator>,
    /// BN254 scalar work captured by nested CALL callbacks.
    pending_precompile_work: Vec<Option<u64>>,
}

impl ZkGasMeteringState {
    /// Creates new per-inspector state for the provided schedule.
    fn new(schedule: Option<&'static ZkGasSchedule>) -> Self {
        Self {
            meter: schedule.map(ZkGasMeter::new),
            pending_steps: [PendingStep::EMPTY; MAX_CALL_DEPTH],
            pending_v2_steps: BTreeMap::new(),
            max_active_depth: 0,
            v2: None,
            pending_precompile_work: Vec::new(),
        }
    }

    /// Discards the in-flight transaction zk gas together with all per-frame step bookkeeping.
    ///
    /// Resetting the per-depth snapshots here keeps aborted execution from leaking into the next
    /// transaction regardless of how REVM unwound its frames.
    fn reset_transaction(&mut self) {
        if let Some(meter) = &mut self.meter {
            meter.reset_transaction();
        }
        for index in 0..=self.max_active_depth {
            self.pending_steps[index] = PendingStep::EMPTY;
        }
        self.max_active_depth = 0;
        self.pending_v2_steps.clear();
        if let Some(v2) = &mut self.v2 {
            v2.reset_transaction();
        }
        self.pending_precompile_work.clear();
    }

    /// Records the opcode and gas snapshot for the current frame depth.
    #[inline(always)]
    fn begin_step<DB: reth_revm::Database>(
        &mut self,
        depth: usize,
        interpreter: &Interpreter<EthInterpreter>,
        context: &TaikoEvmContext<DB>,
    ) {
        // Any previous pending step at this depth must already have been consumed by `step_end`.
        self.pending_steps[depth] = PendingStep {
            opcode: interpreter.bytecode.opcode(),
            gas_remaining: interpreter.gas.remaining(),
        };
        if self.v2.is_some() {
            self.pending_v2_steps.insert(depth, PendingV2Step::capture(interpreter, context));
        } else {
            self.pending_v2_steps.remove(&depth);
        }
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
            v2: self.pending_v2_steps.remove(&depth),
        }
    }

    /// Charges a completed opcode step against the active meter.
    #[inline(always)]
    fn charge_finished_step(&mut self, step: FinishedStep) -> Result<(), ZkGasOutcome> {
        // Spawn opcodes use the fixed consensus estimate only when they selected a child frame.
        // Otherwise we charge the measured interpreter gas delta from this opcode step.
        let Some(meter) = &mut self.meter else { return Ok(()) };
        if step.spawned {
            meter.charge_spawn_opcode(step.opcode)
        } else {
            meter.charge_opcode(step.opcode, step.step_gas)
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
    /// Typed pre-step V2 snapshot, when collection is enabled.
    v2: Option<PendingV2Step>,
}

#[cfg(test)]
mod tests {
    use crate::zk_gas::unzen::UNZEN_ZK_GAS_SCHEDULE;

    use super::{FinishedStep, ZkGasMeteringState};

    #[test]
    fn reset_transaction_clears_meter_and_step_bookkeeping() {
        let mut metering = ZkGasMeteringState::new(Some(&UNZEN_ZK_GAS_SCHEDULE));
        metering.pending_steps[1] = super::PendingStep { opcode: 0x01, gas_remaining: 10 };
        metering.max_active_depth = 1;
        metering.meter.as_mut().unwrap().charge_opcode(0x01, 3).expect("charge fits");

        metering.reset_transaction();

        assert_eq!(metering.meter.as_ref().unwrap().tx_zk_gas_used(), 0);
        assert_eq!(metering.pending_steps[1].opcode, 0);
        assert_eq!(metering.pending_steps[1].gas_remaining, 0);
        assert_eq!(metering.max_active_depth, 0);
    }

    #[test]
    fn charge_finished_step_uses_active_meter_schedule_for_spawn_estimate() {
        let mut metering = ZkGasMeteringState::new(Some(&UNZEN_ZK_GAS_SCHEDULE));

        metering
            .charge_finished_step(FinishedStep {
                opcode: 0xf0,
                step_gas: 1,
                spawned: true,
                v2: None,
            })
            .expect("spawn estimate should fit");

        assert_eq!(
            metering.meter.as_ref().unwrap().tx_zk_gas_used(),
            UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.create *
                u64::from(UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers[0xf0])
        );
    }
}
