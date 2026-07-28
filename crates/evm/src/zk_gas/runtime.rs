//! Interpreter-side zk gas metering used by the production no-inspector path.

use reth_revm::{
    context::{ContextError, ContextTr},
    interpreter::{
        GasTable, Interpreter, InterpreterAction,
        instructions::InstructionTable,
        interpreter::EthInterpreter,
        interpreter_types::{Jumps, LoopControl},
    },
};

use super::{
    adapter::ZK_GAS_LIMIT_ERR,
    meter::{ZkGasMeter, ZkGasOutcome, is_spawn_opcode},
};

/// Runs the interpreter loop while charging zk gas directly in the production path.
///
/// Mirrors revm's `Interpreter::run_plain` control flow — every step signals loop exit through
/// its `Err(InstructionResult)` — with one deliberate difference: an exceptional halt is
/// materialized *before* the step's zk gas charge instead of after the loop.
/// [`Interpreter::halt`] spends all remaining gas for `OutOfGas` results, and that forfeited
/// gas is part of the step's consensus charge: pre-2.4.0 revm called `halt_oog()` (spend-all)
/// inside `step` itself, taiko-geth's zk mirror follows that reference behavior, and these
/// totals feed `header.difficulty` on Unzen. revm's inspected loop materializes the halt
/// before `step_end` for the same reason, which keeps [`super::adapter::ZkGasInspector`]'s
/// measurements identical to this loop's.
#[inline]
pub(crate) fn run_metered_plain<CTX: ContextTr>(
    context: &mut CTX,
    interpreter: &mut Interpreter<EthInterpreter>,
    instruction_table: &InstructionTable<EthInterpreter, CTX>,
    gas_table: &GasTable,
    meter: &mut ZkGasMeter<'static>,
) -> InterpreterAction {
    loop {
        let opcode = interpreter.bytecode.opcode();
        let gas_before = interpreter.gas.remaining();

        let step_result = interpreter.step(instruction_table, gas_table, context);

        // Materialize an exceptional halt before charging so the `OutOfGas` spend-all is
        // reflected in the measured step gas (see the function docs).
        if let Err(result) = step_result &&
            interpreter.bytecode.action().is_none()
        {
            interpreter.halt(result);
        }

        let charge = if is_spawn_opcode(opcode) &&
            matches!(interpreter.bytecode.action(), Some(InterpreterAction::NewFrame(_)))
        {
            meter.charge_spawn_opcode(opcode)
        } else {
            meter.charge_opcode(opcode, gas_before.saturating_sub(interpreter.gas.remaining()))
        };

        if let Err(ZkGasOutcome::LimitExceeded) = charge {
            set_custom_error(context);
            interpreter.halt_fatal();
            break;
        }

        if step_result.is_err() {
            break;
        }
    }

    interpreter.take_next_action()
}

/// Sets the dedicated custom zk gas limit error on the EVM context when none is present yet.
pub(crate) fn set_custom_error<CTX: ContextTr>(context: &mut CTX) {
    let err_slot = context.error();
    if err_slot.is_ok() {
        *err_slot = Err(ContextError::Custom(ZK_GAS_LIMIT_ERR.to_string()));
    }
}
