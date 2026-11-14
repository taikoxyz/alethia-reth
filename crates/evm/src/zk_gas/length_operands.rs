//! Canonical Unzen length-operand halts on narrow-pointer targets.
//!
//! A valid Unzen transaction has at most 2^24 gas. For these instructions, any length in
//! [2^32, 2^64) costs at least 3 * ceil(2^32 / 32) = 402,653,184 dynamic gas. A 64-bit
//! interpreter therefore runs out of gas before accessing memory. On 32-bit targets revm's
//! earlier `usize` conversion instead fails with `InvalidOperandOOG`, which does not forfeit
//! the frame's remaining gas before zk metering. Normalize only that length-conversion halt;
//! leave the original instruction responsible for stack, static-call, and other checks.

use alloy_primitives::U256;
use reth_revm::{
    handler::instructions::EthInstructions,
    interpreter::{
        Host, Instruction, InstructionContext, InstructionExecResult, InstructionResult,
        instruction_table, interpreter::EthInterpreter,
    },
    state::bytecode::opcode,
};

/// Installs the Unzen length wrappers without changing the static gas table.
pub(crate) fn install<H: Host>(instructions: &mut EthInstructions<EthInterpreter, H>) {
    let table = instructions.instruction_table_mut();
    table[opcode::KECCAK256 as usize] =
        Instruction::new(canonical_length::<H, { opcode::KECCAK256 }, 1>);
    table[opcode::CALLDATACOPY as usize] =
        Instruction::new(canonical_length::<H, { opcode::CALLDATACOPY }, 2>);
    table[opcode::CODECOPY as usize] =
        Instruction::new(canonical_length::<H, { opcode::CODECOPY }, 2>);
    table[opcode::EXTCODECOPY as usize] =
        Instruction::new(canonical_length::<H, { opcode::EXTCODECOPY }, 3>);
    table[opcode::MCOPY as usize] = Instruction::new(canonical_length::<H, { opcode::MCOPY }, 2>);
    table[opcode::LOG0 as usize] = Instruction::new(canonical_length::<H, { opcode::LOG0 }, 1>);
    table[opcode::LOG1 as usize] = Instruction::new(canonical_length::<H, { opcode::LOG1 }, 1>);
    table[opcode::LOG2 as usize] = Instruction::new(canonical_length::<H, { opcode::LOG2 }, 1>);
    table[opcode::LOG3 as usize] = Instruction::new(canonical_length::<H, { opcode::LOG3 }, 1>);
    table[opcode::LOG4 as usize] = Instruction::new(canonical_length::<H, { opcode::LOG4 }, 1>);
}

/// Executes the stock opcode, translating only the narrow-pointer length-conversion failure.
fn canonical_length<H: Host, const OP: u8, const LENGTH_INDEX: usize>(
    context: InstructionContext<'_, H, EthInterpreter>,
) -> InstructionExecResult {
    let wide_length = context
        .interpreter
        .stack
        .peek(LENGTH_INDEX)
        .is_ok_and(|length| length > U256::from(u32::MAX) && length <= U256::from(u64::MAX));
    let result = instruction_table::<EthInterpreter, H>()[OP as usize].execute(context);
    if wide_length && result == Err(InstructionResult::InvalidOperandOOG) {
        Err(InstructionResult::OutOfGas)
    } else {
        result
    }
}
