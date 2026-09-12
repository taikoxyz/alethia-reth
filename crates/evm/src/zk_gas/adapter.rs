//! Inspector-side zk gas metering for opcode execution.
//!
//! High-level flow:
//! 1. Capture the opcode and pre-step gas in `step`.
//! 2. Resolve the step into a `FinishedStep` in `step_end`.
//! 3. Read the interpreter action in `step_end`: `NewFrame` uses the spawn estimate, while every
//!    other result uses the measured interpreter delta, matching the production plain loop.

use alloy_primitives::{Address, Log, U256};
#[cfg(feature = "execution-observer")]
use alloy_primitives::{B256, TxKind};
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

#[cfg(feature = "execution-observer")]
use super::observer::{
    ChargeOutcome, ExecutionEvent, ExecutionPhase, SharedExecutionObserver,
    TransactionExecutionClass,
};
use super::{
    meter::{ZkGasMeter, ZkGasOutcome, is_spawn_opcode},
    observer::{ChargeComponent, OperationComponent, RawGasSource},
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

    /// Creates a metering inspector that publishes owned events to `observer` when scheduled.
    #[cfg(feature = "execution-observer")]
    pub fn new_with_execution_observer(
        inner: I,
        schedule: Option<&'static ZkGasSchedule>,
        observer: SharedExecutionObserver,
    ) -> Self {
        let metering =
            schedule.map(|schedule| ZkGasMeteringState::new_with_observer(schedule, observer));
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

    /// Sets the executor-owned phase and optional transaction index for later adapter events.
    #[cfg(feature = "execution-observer")]
    pub(crate) fn set_execution_observer_context(
        &mut self,
        phase: ExecutionPhase,
        tx_index: Option<u64>,
    ) {
        if let Some(metering) = &mut self.metering {
            metering.set_execution_observer_context(phase, tx_index);
        }
    }

    /// Returns the class captured at the normal top-level frame boundary.
    #[cfg(feature = "execution-observer")]
    pub(crate) fn observed_transaction_execution_class(&self) -> Option<TransactionExecutionClass> {
        self.metering.as_ref().and_then(ZkGasMeteringState::observed_transaction_execution_class)
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
        step.operation_id = metering.emit_opcode_execution(step.opcode, step.step_gas);

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
        #[cfg(feature = "execution-observer")]
        if let Some(metering) = &mut self.metering {
            // The first frame belongs to the transaction recipient. At this point REVM has
            // already performed its ordinary account/code load, so capture that fact without an
            // observer-specific database access. Nested CALL/CREATE frames must not overwrite it.
            metering.capture_top_level_recipient_code_hash(context);
        }
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

    /// Records a completed precompile body and its own native-gas charge.
    fn call_end(
        &mut self,
        context: &mut TaikoEvmContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        let was_precompile_called = outcome.was_precompile_called;
        self.inner.call_end(context, inputs, outcome);

        if let Some(metering) = &mut self.metering {
            let precompile = was_precompile_called.then(|| {
                // The CALL wrapper was already charged at step_end, before dispatch. The
                // precompile body has now completed, so publish it before its own native charge.
                let gas_used = inputs.gas_limit.saturating_sub(outcome.result.gas.remaining());
                let operation_id =
                    metering.emit_precompile_execution(inputs.bytecode_address, gas_used);
                (gas_used, operation_id)
            });
            if let Some((gas_used, operation_id)) = precompile {
                // Precompile usage is charged separately from the CALL opcode itself, keyed by the
                // full precompile address.
                if metering
                    .charge_precompile(&inputs.bytecode_address, gas_used, operation_id)
                    .is_err()
                {
                    set_custom_error(context);
                }
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
    /// Optional host-only event sink and its executor-owned context.
    #[cfg(feature = "execution-observer")]
    observer: Option<ObserverState>,
    /// Class observed when normal execution opened the top-level recipient frame.
    #[cfg(feature = "execution-observer")]
    observed_transaction_execution_class: Option<TransactionExecutionClass>,
}

/// Complete observer-facing facts selected for one zk gas charge attempt.
struct ChargeAttemptDetails {
    /// Component being charged.
    component: ChargeComponent,
    /// Raw gas selected by the active schedule.
    charge_raw_gas: Option<u64>,
    /// Origin of the selected raw gas amount.
    raw_gas_source: RawGasSource,
    /// Active schedule multiplier.
    multiplier: Option<u64>,
    /// Checked requested zk gas before applying the meter.
    requested_current_zkgas: Option<u64>,
}

impl ZkGasMeteringState {
    /// Creates new per-inspector state for the provided schedule.
    fn new(schedule: &'static ZkGasSchedule) -> Self {
        Self {
            meter: ZkGasMeter::new(schedule),
            pending_steps: [PendingStep::EMPTY; MAX_CALL_DEPTH],
            max_active_depth: 0,
            #[cfg(feature = "execution-observer")]
            observer: None,
            #[cfg(feature = "execution-observer")]
            observed_transaction_execution_class: None,
        }
    }

    /// Creates metering state that emits events to an observational host sink.
    #[cfg(feature = "execution-observer")]
    fn new_with_observer(
        schedule: &'static ZkGasSchedule,
        observer: SharedExecutionObserver,
    ) -> Self {
        let mut state = Self::new(schedule);
        state.observer = Some(ObserverState {
            observer,
            phase: ExecutionPhase::PreExecutionSystem,
            tx_index: None,
            next_operation_id: 0,
        });
        state
    }

    /// Updates executor-owned context used by subsequent operation and charge events.
    #[cfg(feature = "execution-observer")]
    fn set_execution_observer_context(&mut self, phase: ExecutionPhase, tx_index: Option<u64>) {
        if let Some(observer) = &mut self.observer {
            observer.phase = phase;
            observer.tx_index = tx_index;
        }
    }

    /// Captures only the first transaction frame; inner calls intentionally cannot alter the
    /// executor's top-level transaction classification.
    #[cfg(feature = "execution-observer")]
    fn capture_top_level_recipient_code_hash<DB: reth_revm::Database>(
        &mut self,
        context: &TaikoEvmContext<DB>,
    ) {
        if self.observed_transaction_execution_class.is_some() ||
            !self.observer.as_ref().is_some_and(|observer| {
                observer.phase == ExecutionPhase::Transactions && observer.tx_index.is_some()
            })
        {
            return;
        }
        if let TxKind::Call(recipient) = context.tx().kind {
            let code_hash =
                context.journal().state.get(&recipient).map(|account| account.info.code_hash);
            self.observed_transaction_execution_class = Some(
                if code_hash.is_some_and(|hash| {
                    hash != B256::ZERO && hash != alloy_primitives::KECCAK256_EMPTY
                }) {
                    TransactionExecutionClass::ContractCall
                } else if context.tx().value.is_zero() {
                    TransactionExecutionClass::NoCodeNoValue
                } else {
                    TransactionExecutionClass::NativeValueTransfer
                },
            );
        }
    }

    #[cfg(feature = "execution-observer")]
    /// Returns the first top-level frame classification captured for the active transaction.
    fn observed_transaction_execution_class(&self) -> Option<TransactionExecutionClass> {
        self.observed_transaction_execution_class
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
        #[cfg(feature = "execution-observer")]
        {
            self.observed_transaction_execution_class = None;
        }
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
            operation_id: None,
        }
    }

    /// Charges a completed opcode step against the active meter.
    #[inline(always)]
    fn charge_finished_step(&mut self, step: FinishedStep) -> Result<(), ZkGasOutcome> {
        // Spawn opcodes use the fixed consensus estimate only when they actually dispatched child
        // work. Otherwise we charge the measured interpreter gas delta from this opcode step.
        let (raw_gas, raw_gas_source) = if step.spawned {
            (
                super::meter::spawn_estimate(self.meter.schedule(), step.opcode),
                RawGasSource::SpawnEstimate,
            )
        } else {
            (step.step_gas, RawGasSource::InterpreterDelta)
        };
        let multiplier =
            u64::from(self.meter.schedule().opcode_multipliers[usize::from(step.opcode)]);
        let requested_current_zkgas = raw_gas.checked_mul(multiplier);
        let result = if step.spawned {
            self.meter.charge_spawn_opcode(step.opcode)
        } else {
            self.charge_opcode(step.opcode, step.step_gas)
        };
        self.emit_charge_attempt(
            step.operation_id,
            ChargeAttemptDetails {
                component: ChargeComponent::Opcode { opcode: step.opcode, spawned: step.spawned },
                charge_raw_gas: Some(raw_gas),
                raw_gas_source,
                multiplier: Some(multiplier),
                requested_current_zkgas,
            },
            result,
        );
        result
    }

    /// Charges a measured opcode against the active meter.
    #[inline(always)]
    fn charge_opcode(&mut self, opcode: u8, raw_gas: u64) -> Result<(), ZkGasOutcome> {
        self.meter.charge_opcode(opcode, raw_gas)
    }

    /// Charges one precompile and publishes the charge selection and checked outcome.
    fn charge_precompile(
        &mut self,
        address: &Address,
        native_gas: u64,
        operation_id: Option<u64>,
    ) -> Result<(), ZkGasOutcome> {
        let multiplier = u64::from(self.meter.schedule().precompile_multiplier(address));
        let requested_current_zkgas = native_gas.checked_mul(multiplier);
        let result = self.meter.charge_precompile(address, native_gas);
        self.emit_charge_attempt(
            operation_id,
            ChargeAttemptDetails {
                component: ChargeComponent::Precompile { address: address.into_array() },
                charge_raw_gas: Some(native_gas),
                raw_gas_source: RawGasSource::PrecompileNative,
                multiplier: Some(multiplier),
                requested_current_zkgas,
            },
            result,
        );
        result
    }

    /// Emits an operation record immediately after an opcode body completed.
    fn emit_opcode_execution(&mut self, opcode: u8, interpreter_raw_gas: u64) -> Option<u64> {
        self.emit_operation(OperationComponent::Opcode { opcode, interpreter_raw_gas })
    }

    /// Emits an operation record immediately after a precompile body completed.
    fn emit_precompile_execution(&mut self, address: Address, native_gas: u64) -> Option<u64> {
        self.emit_operation(OperationComponent::Precompile {
            address: address.into_array(),
            native_gas,
        })
    }

    /// Emits an operation only when the feature-gated host observer is installed.
    fn emit_operation(&mut self, component: OperationComponent) -> Option<u64> {
        #[cfg(feature = "execution-observer")]
        {
            let (observer, operation_id, phase, tx_index) = {
                let state = self.observer.as_mut()?;
                let operation_id = state.next_operation_id;
                state.next_operation_id =
                    state.next_operation_id.checked_add(1).expect("operation id overflow");
                (state.observer.clone(), operation_id, state.phase, state.tx_index)
            };
            observer.on_event(ExecutionEvent::OperationExecuted {
                operation_id,
                phase,
                tx_index,
                component,
            });
            Some(operation_id)
        }
        #[cfg(not(feature = "execution-observer"))]
        {
            let _ = component;
            None
        }
    }

    /// Emits the selected current-schedule charge and its checked meter outcome.
    fn emit_charge_attempt(
        &self,
        operation_id: Option<u64>,
        details: ChargeAttemptDetails,
        result: Result<(), ZkGasOutcome>,
    ) {
        #[cfg(feature = "execution-observer")]
        if let Some(state) = &self.observer {
            let outcome = match result {
                Ok(()) => ChargeOutcome::Applied,
                Err(ZkGasOutcome::LimitExceeded) if details.requested_current_zkgas.is_none() => {
                    ChargeOutcome::ArithmeticOverflow
                }
                Err(ZkGasOutcome::LimitExceeded) => ChargeOutcome::LimitExceeded,
            };
            state.observer.on_event(ExecutionEvent::ChargeAttempt {
                operation_id,
                phase: state.phase,
                tx_index: state.tx_index,
                component: details.component,
                charge_raw_gas: details.charge_raw_gas,
                raw_gas_source: details.raw_gas_source,
                multiplier: details.multiplier,
                requested_current_zkgas: details.requested_current_zkgas,
                outcome,
            });
        }
        #[cfg(not(feature = "execution-observer"))]
        #[allow(irrefutable_let_patterns)]
        let ChargeAttemptDetails {
            component,
            charge_raw_gas,
            raw_gas_source,
            multiplier,
            requested_current_zkgas,
        } = details;
        #[cfg(not(feature = "execution-observer"))]
        let _ = (
            operation_id,
            component,
            charge_raw_gas,
            raw_gas_source,
            multiplier,
            requested_current_zkgas,
            result,
        );
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
    /// Whether the opcode dispatched child work and should use the fixed spawn estimate.
    spawned: bool,
    /// Feature-gated operation identifier allocated after the opcode body completed.
    operation_id: Option<u64>,
}

/// Host observer state retained by one inspector across a block execution.
#[cfg(feature = "execution-observer")]
struct ObserverState {
    /// Event sink that must not influence execution.
    observer: SharedExecutionObserver,
    /// Executor-owned phase for the current callback sequence.
    phase: ExecutionPhase,
    /// Executor-owned transaction index for the current callback sequence.
    tx_index: Option<u64>,
    /// Next block-unique operation identifier.
    next_operation_id: u64,
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "execution-observer")]
    use std::sync::{Arc, Mutex};

    use crate::zk_gas::{meter::ZkGasOutcome, unzen::UNZEN_ZK_GAS_SCHEDULE};

    use super::{FinishedStep, ZkGasMeteringState};
    #[cfg(feature = "execution-observer")]
    use crate::zk_gas::observer::{ChargeOutcome, ExecutionEvent, ExecutionObserver};

    #[cfg(feature = "execution-observer")]
    #[derive(Default)]
    struct RecordingObserver(Mutex<Vec<ExecutionEvent>>);

    #[cfg(feature = "execution-observer")]
    impl ExecutionObserver for RecordingObserver {
        fn on_event(&self, event: ExecutionEvent) {
            self.0.lock().expect("observer lock should not be poisoned").push(event);
        }
    }

    #[cfg(feature = "execution-observer")]
    #[test]
    fn observer_distinguishes_arithmetic_overflow_from_budget_limit() {
        let observer = Arc::new(RecordingObserver::default());
        let mut metering =
            ZkGasMeteringState::new_with_observer(&UNZEN_ZK_GAS_SCHEDULE, observer.clone());
        let opcode = UNZEN_ZK_GAS_SCHEDULE
            .opcode_multipliers
            .iter()
            .position(|multiplier| *multiplier > 1)
            .expect("Unzen schedule has a multiplied opcode") as u8;
        let overflow_id = metering.emit_opcode_execution(opcode, u64::MAX);
        let overflow = metering.charge_finished_step(FinishedStep {
            opcode,
            step_gas: u64::MAX,
            spawned: false,
            operation_id: overflow_id,
        });
        let budget_id =
            metering.emit_opcode_execution(opcode, UNZEN_ZK_GAS_SCHEDULE.block_limit + 1);
        let budget = metering.charge_finished_step(FinishedStep {
            opcode,
            step_gas: UNZEN_ZK_GAS_SCHEDULE.block_limit + 1,
            spawned: false,
            operation_id: budget_id,
        });

        // The meter's consensus-facing normalization is LimitExceeded in both cases.
        assert_eq!(overflow, Err(ZkGasOutcome::LimitExceeded));
        assert_eq!(budget, Err(ZkGasOutcome::LimitExceeded));

        let events = observer.0.lock().expect("observer lock should not be poisoned");
        assert!(matches!(
            events[1],
            ExecutionEvent::ChargeAttempt { operation_id: Some(id), outcome: ChargeOutcome::ArithmeticOverflow, .. }
                if Some(id) == overflow_id
        ));
        assert!(matches!(
            events[3],
            ExecutionEvent::ChargeAttempt { operation_id: Some(id), outcome: ChargeOutcome::LimitExceeded, .. }
                if Some(id) == budget_id
        ));
    }

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
            .charge_finished_step(FinishedStep {
                opcode: 0xf0,
                step_gas: 1,
                spawned: true,
                operation_id: None,
            })
            .expect("spawn estimate should fit");

        assert_eq!(
            metering.meter.tx_zk_gas_used(),
            UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.create *
                u64::from(UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers[0xf0])
        );
    }
}
