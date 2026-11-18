//! Target-independent charging for memory lengths before native address conversion.

use reth_revm::interpreter::{Host, instructions::InstructionTable, interpreter::EthInterpreter};

/// Installs Unzen's wide-length charging adapters on narrow-pointer targets.
///
/// The instruction table is shared by plain and inspected execution. Other forks and 64-bit
/// targets retain revm's original instructions.
pub(crate) fn install<H: Host>(table: &mut InstructionTable<EthInterpreter, H>, enabled: bool) {
    #[cfg(target_pointer_width = "32")]
    if enabled {
        install_wide_lengths(table);
    }
    #[cfg(not(target_pointer_width = "32"))]
    let _ = (table, enabled);
}

/// Replaces only instructions that charge words or data after converting their length.
#[cfg(any(target_pointer_width = "32", test))]
fn install_wide_lengths<H: Host>(table: &mut InstructionTable<EthInterpreter, H>) {
    use reth_revm::interpreter::instructions::Instruction;
    table[0x20] = Instruction::new(wide_length::<0x20, H>);
    table[0x37] = Instruction::new(wide_length::<0x37, H>);
    table[0x39] = Instruction::new(wide_length::<0x39, H>);
    table[0x3c] = Instruction::new(wide_length::<0x3c, H>);
    table[0x5e] = Instruction::new(wide_length::<0x5e, H>);
    table[0xa0] = Instruction::new(wide_length::<0xa0, H>);
    table[0xa1] = Instruction::new(wide_length::<0xa1, H>);
    table[0xa2] = Instruction::new(wide_length::<0xa2, H>);
    table[0xa3] = Instruction::new(wide_length::<0xa3, H>);
    table[0xa4] = Instruction::new(wide_length::<0xa4, H>);
}

/// Computes the charge skipped when a u64 length cannot be represented by a 32-bit address.
///
/// Lengths outside u64 retain revm's operand failure without a dynamic charge. Word rounding
/// and multiplication use u64 arithmetic, matching the 64-bit interpreter's gas parameters.
#[cfg(any(target_pointer_width = "32", test))]
fn skipped_charge<const OP: u8, H: Host>(
    context: &reth_revm::interpreter::InstructionContext<'_, H, EthInterpreter>,
) -> Option<u64> {
    use reth_revm::context_interface::cfg::GasId;
    let index = match OP {
        0x37 | 0x39 | 0x5e => 2,
        0x3c => 3,
        _ => 1,
    };
    let len = u64::try_from(context.interpreter.stack.peek(index).ok()?).ok()?;
    if len <= u64::from(u32::MAX) {
        return None;
    }
    let params = context.host.gas_params();
    let rate = match OP {
        0x20 => GasId::keccak256_per_word(),
        0x37 | 0x39 => GasId::copy_per_word(),
        0x3c => GasId::extcodecopy_per_word(),
        0x5e => GasId::mcopy_per_word(),
        _ => return Some(params.log_cost(OP - 0xa0, len)),
    };
    Some(params.get(rate).saturating_mul(len.div_ceil(32)))
}

/// Runs the original instruction and restores any charge skipped by native length conversion.
///
/// revm validates static context and stack operands before converting the length. Retaining that
/// execution order avoids charging a step that failed an earlier check. A rejected word/data
/// charge becomes `OutOfGas`, so both interpreter loops materialize the same spend-all halt.
/// If it fits, it is charged before the address-size halt, as on the 64-bit memory-failure path.
#[cfg(any(target_pointer_width = "32", test))]
fn wide_length<const OP: u8, H: Host>(
    context: reth_revm::interpreter::InstructionContext<'_, H, EthInterpreter>,
) -> reth_revm::interpreter::InstructionExecResult {
    use reth_revm::interpreter::instructions::instruction_table;
    let charge = skipped_charge::<OP, H>(&context);
    let before = context.interpreter.gas.remaining();
    let result = instruction_table::<EthInterpreter, H>()[OP as usize].execute(
        reth_revm::interpreter::InstructionContext {
            interpreter: context.interpreter,
            host: context.host,
        },
    );
    restore_length_charge(result, before, charge, context.interpreter)
}

/// Restores a dynamic charge only when native length conversion skipped it entirely.
#[cfg(any(target_pointer_width = "32", test))]
fn restore_length_charge(
    result: reth_revm::interpreter::InstructionExecResult,
    before: u64,
    charge: Option<u64>,
    interpreter: &mut reth_revm::interpreter::Interpreter<EthInterpreter>,
) -> reth_revm::interpreter::InstructionExecResult {
    use reth_revm::interpreter::InstructionResult;
    if result == Err(InstructionResult::InvalidOperandOOG) &&
        interpreter.gas.remaining() == before &&
        let Some(charge) = charge &&
        !interpreter.gas.record_regular_cost(charge)
    {
        return Err(InstructionResult::OutOfGas);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_revm::{
        context_interface::host::DummyHost,
        interpreter::{Gas, InstructionContext, InstructionResult, Interpreter},
        primitives::{U256, hardfork::SpecId},
    };

    #[test]
    fn wide_adapter_preserves_earlier_static_context_failure() {
        let mut interpreter =
            Interpreter::<EthInterpreter> { gas: Gas::new(100_000), ..Default::default() };
        interpreter.runtime_flag.is_static = true;
        assert!(interpreter.stack.push(U256::from(1u64 << 32)));
        assert!(interpreter.stack.push(U256::ZERO));
        let mut host = DummyHost::new(SpecId::OSAKA);
        let mut table = reth_revm::interpreter::instructions::instruction_table();
        install_wide_lengths(&mut table);
        let result = table[0xa0]
            .execute(InstructionContext { interpreter: &mut interpreter, host: &mut host });
        assert_eq!(result, Err(InstructionResult::StateChangeDuringStaticCall));
        assert_eq!(interpreter.gas.remaining(), 100_000);
    }
    #[test]
    fn narrow_length_shortfall_becomes_full_oog() {
        let mut interpreter =
            Interpreter::<EthInterpreter> { gas: Gas::new(100_000), ..Default::default() };
        let result = restore_length_charge(
            Err(InstructionResult::InvalidOperandOOG),
            100_000,
            Some(402_653_184),
            &mut interpreter,
        );
        assert_eq!(result, Err(InstructionResult::OutOfGas));
        interpreter.halt(result.unwrap_err());
        assert_eq!(interpreter.gas.remaining(), 0);
    }

    #[test]
    fn narrow_length_charge_is_kept_when_it_fits() {
        let mut interpreter =
            Interpreter::<EthInterpreter> { gas: Gas::new(1_000_000_000), ..Default::default() };
        let result = restore_length_charge(
            Err(InstructionResult::InvalidOperandOOG),
            1_000_000_000,
            Some(402_653_184),
            &mut interpreter,
        );
        assert_eq!(result, Err(InstructionResult::InvalidOperandOOG));
        assert_eq!(interpreter.gas.remaining(), 597_346_816);
    }

    #[test]
    fn recorded_length_charge_is_not_repeated() {
        let mut interpreter =
            Interpreter::<EthInterpreter> { gas: Gas::new(1_000_000_000), ..Default::default() };
        assert!(interpreter.gas.record_regular_cost(402_653_184));
        let result = restore_length_charge(
            Err(InstructionResult::InvalidOperandOOG),
            1_000_000_000,
            Some(402_653_184),
            &mut interpreter,
        );
        assert_eq!(result, Err(InstructionResult::InvalidOperandOOG));
        assert_eq!(interpreter.gas.remaining(), 597_346_816);
    }
}
