//! Surcharge schedule types for the next-fork gas reprice.
//!
//! NON-CONSENSUS: the example values exist only to exercise the builders. Real
//! surcharges are a separate calibration effort and must match taiko-geth + the prover.

/// Additive gas surcharges applied on top of Ethereum gas for the repriced fork.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepriceSchedule {
    /// Per-opcode additive `static_gas` surcharge, indexed by opcode byte. `0` = unchanged.
    ///
    /// Only opcodes explicitly wired in `repriced_instruction_table` take effect; a surcharge
    /// on any other index is silently ignored (the builder can reprice an opcode only if it
    /// also names that opcode's instruction fn). Extending coverage means adding to that
    /// builder, not just setting an entry here.
    pub opcode_surcharge: [u64; 256],
    /// Fixed gas added to every transaction's intrinsic gas (covers ecrecover proving).
    pub tx_intrinsic_surcharge: u64,
}

/// Returns the example schedule used by tests.
///
/// Non-consensus: the values exist only to exercise the builders and must not be used in
/// production. Real surcharges are derived during calibration.
pub const fn example_reprice_schedule() -> RepriceSchedule {
    let mut opcode_surcharge = [0u64; 256];
    opcode_surcharge[0x01] = 17; // ADD
    opcode_surcharge[0x02] = 16; // MUL
    opcode_surcharge[0x20] = 200; // KECCAK256
    opcode_surcharge[0x51] = 15; // MLOAD
    opcode_surcharge[0x54] = 6; // SLOAD
    RepriceSchedule { opcode_surcharge, tx_intrinsic_surcharge: 243_000 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_schedule_sets_expected_surcharges() {
        let s = example_reprice_schedule();
        assert_eq!(s.opcode_surcharge[0x01], 17); // ADD
        assert_eq!(s.opcode_surcharge[0x02], 16); // MUL
        assert_eq!(s.opcode_surcharge[0x20], 200); // KECCAK256
        assert_eq!(s.opcode_surcharge[0x51], 15); // MLOAD
        assert_eq!(s.opcode_surcharge[0x54], 6); // SLOAD
        assert_eq!(s.tx_intrinsic_surcharge, 243_000);
        // Unlisted opcodes default to zero (unchanged).
        assert_eq!(s.opcode_surcharge[0xfe], 0);
    }
}
