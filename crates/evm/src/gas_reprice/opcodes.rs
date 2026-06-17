//! Builds an instruction table whose repriced opcodes carry an additive `static_gas`
//! surcharge on top of the Ethereum spec base table.

use reth_revm::{
    interpreter::{
        Host, Instruction, InstructionContext, InstructionTable,
        instructions::{arithmetic, host, instruction_table_gas_changes_spec, memory, system},
        interpreter_types::InterpreterTypes,
    },
    revm::primitives::hardfork::SpecId,
};

use super::schedule::RepriceSchedule;

/// Rebuilds one table entry as `Instruction::new(f, base_static_gas + surcharge)`.
///
/// `f` MUST be the same revm instruction fn the base table uses for `opcode`; only the
/// static gas changes. `static_gas` is private with no setter, so the fn is named
/// explicitly and the entry reconstructed.
///
/// Maintenance: the `(opcode, f)` pairs in [`repriced_instruction_table`] duplicate revm's
/// own opcode-to-function mapping. When bumping the reth/revm pin, re-verify each pair
/// against revm's instruction table — a fn that moved would otherwise be silently replaced.
#[inline]
fn reprice<W, H>(
    table: &mut InstructionTable<W, H>,
    opcode: usize,
    f: fn(InstructionContext<'_, H, W>),
    surcharge: u64,
) where
    W: InterpreterTypes,
    H: Host,
{
    let base = table[opcode].static_gas();
    table[opcode] = Instruction::new(f, base.saturating_add(surcharge));
}

/// Returns the Ethereum spec base table with the schedule's opcode surcharges added.
///
/// The repriced opcode set is the explicit list below. Calibration (out of scope) extends
/// this list following the identical `reprice(...)` pattern.
pub fn repriced_instruction_table<W, H>(
    eth_spec: SpecId,
    schedule: &RepriceSchedule,
) -> InstructionTable<W, H>
where
    W: InterpreterTypes,
    H: Host,
{
    let surcharges = &schedule.opcode_surcharge;
    let mut table = instruction_table_gas_changes_spec::<W, H>(eth_spec);

    reprice(&mut table, 0x01, arithmetic::add::<W, H>, surcharges[0x01]);
    reprice(&mut table, 0x02, arithmetic::mul::<W, H>, surcharges[0x02]);
    reprice(&mut table, 0x20, system::keccak256::<W, H>, surcharges[0x20]);
    reprice(&mut table, 0x51, memory::mload::<W, H>, surcharges[0x51]);
    reprice(&mut table, 0x54, host::sload::<W, H>, surcharges[0x54]);

    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{alloy::TaikoEvmContext, gas_reprice::schedule::example_reprice_schedule};
    use reth_revm::{db::EmptyDB, interpreter::interpreter::EthInterpreter};

    type Ctx = TaikoEvmContext<EmptyDB>;

    #[test]
    fn repriced_entries_add_surcharge_over_base() {
        let spec = SpecId::OSAKA;
        let schedule = example_reprice_schedule();
        let base = instruction_table_gas_changes_spec::<EthInterpreter, Ctx>(spec);
        let table = repriced_instruction_table::<EthInterpreter, Ctx>(spec, &schedule);

        for op in [0x01usize, 0x02, 0x20, 0x51, 0x54] {
            assert_eq!(
                table[op].static_gas(),
                // Mirror the production arithmetic so the test stays correct in the
                // saturating region (e.g. a future sentinel surcharge).
                base[op].static_gas().saturating_add(schedule.opcode_surcharge[op]),
                "opcode {op:#x} should be base + surcharge"
            );
        }
    }

    #[test]
    fn unrepriced_entries_are_unchanged() {
        let spec = SpecId::OSAKA;
        let schedule = example_reprice_schedule();
        let base = instruction_table_gas_changes_spec::<EthInterpreter, Ctx>(spec);
        let table = repriced_instruction_table::<EthInterpreter, Ctx>(spec, &schedule);

        // Spot-check non-contiguous opcodes outside the repriced set to catch an
        // off-by-one that clobbers an adjacent slot: STOP, ISZERO, PUSH1.
        for op in [0x00usize, 0x15, 0x60] {
            assert_eq!(
                table[op].static_gas(),
                base[op].static_gas(),
                "opcode {op:#x} should be unchanged"
            );
        }
    }

    #[test]
    fn surcharge_on_unwired_opcode_is_ignored() {
        // The builder reprices only opcodes whose fn it names; a schedule entry for any
        // other opcode (here SSTORE 0x55) is intentionally dropped, not applied. This pins
        // that contract so extending coverage is a conscious change to the builder.
        let spec = SpecId::OSAKA;
        let mut schedule = example_reprice_schedule();
        schedule.opcode_surcharge[0x55] = 999;
        let base = instruction_table_gas_changes_spec::<EthInterpreter, Ctx>(spec);
        let table = repriced_instruction_table::<EthInterpreter, Ctx>(spec, &schedule);

        assert_eq!(
            table[0x55].static_gas(),
            base[0x55].static_gas(),
            "surcharge on an unwired opcode (SSTORE) must be ignored"
        );
    }
}
