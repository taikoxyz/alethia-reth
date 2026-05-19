//! Inspector-side zk gas metering for opcode execution.
//!
//! High-level flow:
//! 1. Capture the opcode and pre-step gas in `step`.
//! 2. Resolve the step into a `FinishedStep` in `step_end`.
//! 3. Either charge immediately, or defer charging until `call_end` / `create_end` confirms whether
//!    a spawn opcode actually opened child work.

use reth_revm::{
    Inspector,
    context::{ContextTr, JournalTr},
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter,
        interpreter::EthInterpreter, interpreter_types::Jumps,
    },
};

use crate::alloy::TaikoEvmContext;

use super::{
    meter::{ZkGasMeter, ZkGasOutcome, is_spawn_opcode},
    runtime::set_custom_error,
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
    /// Returns `None` when the active spec/chain combination has no zk gas schedule
    /// (pre-Unzen specs).
    pub(crate) fn meter(&self) -> Option<&ZkGasMeter<'static>> {
        self.metering.as_ref().map(|state| &state.meter)
    }

    /// Returns a mutable reference to the active zk gas meter, if metering is enabled.
    ///
    /// Returns `None` when the active spec/chain combination has no zk gas schedule
    /// (pre-Unzen specs).
    pub(crate) fn meter_mut(&mut self) -> Option<&mut ZkGasMeter<'static>> {
        self.metering.as_mut().map(|state| &mut state.meter)
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
            // Spawn opcodes are charged one callback later, once the runtime tells us whether
            // they actually opened a child frame or hit a precompile.
            if metering.has_deferred_steps &&
                let Err(ZkGasOutcome::LimitExceeded) = metering.flush_deferred_steps()
            {
                set_custom_error(context);
                interp.halt_fatal();
                return;
            }
            // Snapshot the opcode and remaining gas before the interpreter mutates frame state.
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
        // Pair the pre-step snapshot captured in `step` with the post-step gas remaining.
        let step = metering.finish_step(depth, interp.gas.remaining());

        if is_spawn_opcode(step.opcode) {
            // CALL/CREATE-family opcodes need one more callback to learn whether they really
            // spawned child work. Until then we cannot choose between measured gas and the fixed
            // spawn estimate from the consensus schedule.
            metering.defer_step(depth, step);
            return;
        }

        // Ordinary opcodes can be charged immediately from their measured interpreter gas cost.
        if let Err(ZkGasOutcome::LimitExceeded) = metering.charge_opcode(step.opcode, step.step_gas)
        {
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
            // `None` means REVM continues into a child frame, so this CALL-family opcode should
            // use the fixed spawn estimate instead of its measured interpreter-only gas delta.
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
                // Precompile dispatch is also treated as spawned child work for CALL-family steps.
                metering.mark_call_spawn(context.journal().depth());
            }
            // At this point the call outcome is known, so any deferred CALL-family opcode can be
            // charged using the correct raw-gas source.
            if let Err(ZkGasOutcome::LimitExceeded) = metering.flush_deferred_steps() {
                set_custom_error(context);
                return;
            }
            if was_precompile_called {
                // Precompile usage is charged separately from the CALL opcode itself using the
                // low-byte address lookup in the precompile multiplier table.
                let gas_used = inputs.gas_limit.saturating_sub(outcome.result.gas.remaining());
                let address_low_byte = inputs.bytecode_address.as_slice()[19];
                if let Err(ZkGasOutcome::LimitExceeded) =
                    metering.meter.charge_precompile(address_low_byte, gas_used)
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
            // CREATE-family opcodes use the same deferred pattern as CALL-family opcodes.
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
            // CREATE-family opcodes can finally be charged once the create outcome is resolved.
            let Err(ZkGasOutcome::LimitExceeded) = metering.flush_deferred_steps()
        {
            set_custom_error(context);
        }
    }
}

/// Per-inspector metering state carried across opcode callbacks.
struct ZkGasMeteringState {
    /// Owned checked meter that holds the schedule and accumulated usage.
    meter: ZkGasMeter<'static>,
    /// Per-frame in-flight opcode step state keyed by journal depth.
    pending_steps: [PendingStep; MAX_CALL_DEPTH],
    /// Completed CALL/CREATE-family steps waiting for spawn information before charging.
    deferred_steps: [Option<FinishedStep>; MAX_CALL_DEPTH],
    /// Whether any deferred CALL/CREATE-family step is currently waiting to be charged.
    has_deferred_steps: bool,
    /// Highest journal depth ever observed in this state's lifetime.
    /// Bounds the work done by `flush_deferred_steps` so it stays proportional to
    /// the actual call depth a transaction reaches, not the array capacity.
    max_active_depth: usize,
}

impl ZkGasMeteringState {
    /// Creates new per-inspector state for the provided schedule.
    fn new(schedule: &'static ZkGasSchedule) -> Self {
        Self {
            meter: ZkGasMeter::new(schedule),
            pending_steps: [PendingStep::EMPTY; MAX_CALL_DEPTH],
            deferred_steps: [const { None }; MAX_CALL_DEPTH],
            has_deferred_steps: false,
            max_active_depth: 0,
        }
    }

    /// Records the opcode and gas snapshot for the current frame depth.
    #[inline(always)]
    fn begin_step(&mut self, depth: usize, opcode: u8, gas_remaining: u64) {
        // Any previous pending step at this depth must already have been consumed by `step_end`.
        self.pending_steps[depth] = PendingStep { opcode, gas_remaining, spawned: false };
        if depth > self.max_active_depth {
            self.max_active_depth = depth;
        }
    }

    /// Marks that the current CALL-family opcode actually dispatched child work.
    fn mark_call_spawn(&mut self, depth: usize) {
        // REVM can report the spawn signal from either the current frame or the parent frame
        // depending on callback timing, so mark both locations defensively.
        self.mark_spawn(depth, is_call_opcode);
        self.mark_spawn(parent_step_depth(depth), is_call_opcode);
    }

    /// Marks that the current CREATE-family opcode actually dispatched child work.
    fn mark_create_spawn(&mut self, depth: usize) {
        // Same defensive marking strategy as CALL-family opcodes.
        self.mark_spawn(depth, is_create_opcode);
        self.mark_spawn(parent_step_depth(depth), is_create_opcode);
    }

    /// Finalizes the current step state and returns the completed metering record.
    #[inline(always)]
    fn finish_step(&mut self, depth: usize, gas_remaining: u64) -> FinishedStep {
        let pending = self.pending_steps[depth];
        FinishedStep {
            opcode: pending.opcode,
            step_gas: pending.gas_remaining.saturating_sub(gas_remaining),
            spawned: pending.spawned,
        }
    }

    /// Stores a finished spawn opcode until frame-resolution hooks can determine its raw gas
    /// source.
    fn defer_step(&mut self, depth: usize, step: FinishedStep) {
        // There should be at most one unresolved spawn step per frame depth at a time.
        self.deferred_steps[depth] = Some(step);
        self.has_deferred_steps = true;
        if depth > self.max_active_depth {
            self.max_active_depth = depth;
        }
    }

    /// Charges and clears every deferred spawn opcode.
    #[inline(always)]
    fn flush_deferred_steps(&mut self) -> Result<(), ZkGasOutcome> {
        if !self.has_deferred_steps {
            return Ok(());
        }

        for index in 0..=self.max_active_depth {
            if let Some(step) = self.deferred_steps[index].take() {
                // `take()` clears the slot first so partial progress is preserved if charging
                // returns `LimitExceeded`.
                if let Err(err) = self.charge_finished_step(step) {
                    self.has_deferred_steps =
                        self.deferred_steps[..=self.max_active_depth].iter().any(Option::is_some);
                    return Err(err);
                }
            }
        }
        self.has_deferred_steps = false;
        Ok(())
    }

    /// Charges a completed opcode step against the active meter.
    #[inline(always)]
    fn charge_finished_step(&mut self, step: FinishedStep) -> Result<(), ZkGasOutcome> {
        // Spawn opcodes use the fixed consensus estimate only when they actually dispatched child
        // work. Otherwise we charge the measured interpreter gas delta from this opcode step.
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

    /// Marks pending or deferred spawn steps when the opcode matches the expected family.
    fn mark_spawn(&mut self, depth: usize, predicate: fn(u8) -> bool) {
        if let Some(pending) = self.pending_steps.get_mut(depth) &&
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

impl PendingStep {
    /// Empty placeholder overwritten by `begin_step` before `finish_step` reads a depth.
    const EMPTY: Self = Self { opcode: 0, gas_remaining: 0, spawned: false };
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
}

/// Returns `true` when `opcode` is a CALL-family spawn opcode.
fn is_call_opcode(opcode: u8) -> bool {
    // 0xf1 = CALL, 0xf2 = CALLCODE, 0xf4 = DELEGATECALL, 0xfa = STATICCALL.
    matches!(opcode, 0xf1 | 0xf2 | 0xf4 | 0xfa)
}

/// Returns `true` when `opcode` is a CREATE-family spawn opcode.
fn is_create_opcode(opcode: u8) -> bool {
    // 0xf0 = CREATE, 0xf5 = CREATE2.
    matches!(opcode, 0xf0 | 0xf5)
}

/// Returns the caller-frame depth that owns the current spawn-opcode step.
fn parent_step_depth(depth: usize) -> usize {
    depth.saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use crate::{
        spec::TaikoSpecId,
        zk_gas::{
            schedule::schedule_for,
            unzen::{MASAYA_UNZEN_ZK_GAS_SCHEDULE, UNZEN_ZK_GAS_SCHEDULE},
        },
    };

    use super::{FinishedStep, ZkGasMeteringState};

    #[test]
    fn flush_deferred_steps_returns_immediately_when_empty() {
        let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");
        let mut metering = ZkGasMeteringState::new(schedule);

        metering.flush_deferred_steps().expect("empty flush should succeed");

        assert!(!metering.has_deferred_steps);
        assert_eq!(metering.meter.tx_zk_gas_used(), 0);
    }

    #[test]
    fn flush_deferred_steps_clears_flag_after_charging_deferred_step() {
        let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");
        let mut metering = ZkGasMeteringState::new(schedule);

        metering.defer_step(0, FinishedStep { opcode: 0x01, step_gas: 3, spawned: false });
        metering.flush_deferred_steps().expect("deferred flush should succeed");

        assert!(!metering.has_deferred_steps);
        assert_eq!(
            metering.meter.tx_zk_gas_used(),
            3 * u64::from(schedule.opcode_multipliers[0x01])
        );
    }

    #[test]
    fn flush_deferred_steps_preserves_flag_when_later_deferred_step_remains_after_error() {
        let mut metering = ZkGasMeteringState::new(&MASAYA_UNZEN_ZK_GAS_SCHEDULE);

        metering.defer_step(
            0,
            FinishedStep {
                opcode: 0xf0,
                step_gas: MASAYA_UNZEN_ZK_GAS_SCHEDULE.block_limit + 1,
                spawned: false,
            },
        );
        metering.defer_step(1, FinishedStep { opcode: 0x01, step_gas: 1, spawned: false });

        assert!(metering.flush_deferred_steps().is_err());

        assert!(metering.has_deferred_steps);
        assert!(metering.deferred_steps[0].is_none());
        assert!(metering.deferred_steps[1].is_some());
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
