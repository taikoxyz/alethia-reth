//! Interpreter-side zk gas metering used by the production no-inspector path.

use reth_revm::{
    context::{ContextError, ContextTr},
    interpreter::{
        Interpreter, InterpreterAction,
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
#[inline]
pub(crate) fn run_metered_plain<CTX: ContextTr>(
    context: &mut CTX,
    interpreter: &mut Interpreter<EthInterpreter>,
    instruction_table: &InstructionTable<EthInterpreter, CTX>,
    meter: &mut ZkGasMeter<'static>,
) -> InterpreterAction {
    while interpreter.bytecode.is_not_end() {
        let opcode = interpreter.bytecode.opcode();
        let gas_before = interpreter.gas.remaining();

        interpreter.step(instruction_table, context);

        let charge = if is_spawn_opcode(opcode) &&
            matches!(&interpreter.bytecode.action, Some(InterpreterAction::NewFrame(_)))
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
