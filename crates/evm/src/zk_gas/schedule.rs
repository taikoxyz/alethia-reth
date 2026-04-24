//! Shared zk gas schedule types and fork selection helpers.

use crate::spec::TaikoSpecId;

use super::unzen::UNZEN_ZK_GAS_SCHEDULE;

/// Fixed raw-gas estimates for spawn opcodes.
#[derive(Clone, Copy)]
pub struct SpawnEstimates {
    /// Fixed raw-gas estimate for `CALL`.
    pub call: u64,
    /// Fixed raw-gas estimate for `CALLCODE`.
    pub callcode: u64,
    /// Fixed raw-gas estimate for `DELEGATECALL`.
    pub delegatecall: u64,
    /// Fixed raw-gas estimate for `STATICCALL`.
    pub staticcall: u64,
    /// Fixed raw-gas estimate for `CREATE`.
    pub create: u64,
    /// Fixed raw-gas estimate for `CREATE2`.
    pub create2: u64,
}

/// Consensus-owned zk gas schedule for a Taiko fork.
#[derive(Clone, Copy)]
pub struct ZkGasSchedule {
    /// Maximum zk gas permitted across a single block.
    pub block_limit: u64,
    /// Per-opcode proving-cost multipliers indexed by opcode byte.
    pub opcode_multipliers: [u16; 256],
    /// Per-precompile proving-cost multipliers indexed by low-byte address.
    pub precompile_multipliers: [u16; 256],
    /// Fixed raw-gas estimates for spawn opcodes.
    pub spawn_estimates: SpawnEstimates,
}

/// Returns the consensus zk gas schedule for the active Taiko fork, when defined.
pub const fn schedule_for(spec: TaikoSpecId) -> Option<&'static ZkGasSchedule> {
    match spec {
        TaikoSpecId::UNZEN => Some(&UNZEN_ZK_GAS_SCHEDULE),
        _ => None,
    }
}
