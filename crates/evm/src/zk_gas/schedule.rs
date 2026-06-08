//! Shared zk gas schedule types and fork selection helpers.

use alloy_primitives::Address;

use crate::spec::TaikoSpecId;

use super::unzen::UNZEN_ZK_GAS_SCHEDULE;

/// Fail-safe multiplier applied to any precompile absent from a schedule's table.
pub const FAILSAFE_MULTIPLIER: u16 = u16::MAX;

/// Fixed raw-gas estimates for spawn opcodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ZkGasSchedule {
    /// Maximum zk gas permitted across a single block.
    pub block_limit: u64,
    /// Fixed zk gas charged once per block transaction before opcode or precompile
    /// metering begins.
    pub tx_intrinsic_zk_gas: u64,
    /// Per-opcode proving-cost multipliers indexed by opcode byte.
    pub opcode_multipliers: [u16; 256],
    /// Per-precompile proving-cost multipliers keyed by full 20-byte precompile address.
    pub precompile_multipliers: &'static [(Address, u16)],
    /// Fixed raw-gas estimates for spawn opcodes.
    pub spawn_estimates: SpawnEstimates,
}

impl ZkGasSchedule {
    /// Returns the proving-cost multiplier for `address`, or [`FAILSAFE_MULTIPLIER`] when the
    /// precompile is not listed in this schedule.
    #[inline]
    pub fn precompile_multiplier(&self, address: &Address) -> u16 {
        self.precompile_multipliers
            .iter()
            .find(|(addr, _)| addr == address)
            .map_or(FAILSAFE_MULTIPLIER, |&(_, multiplier)| multiplier)
    }
}

/// Returns the consensus zk gas schedule for the active Taiko fork, when defined. Only Unzen
/// defines a schedule; every chain shares the single [`UNZEN_ZK_GAS_SCHEDULE`].
pub const fn schedule_for(spec: TaikoSpecId) -> Option<&'static ZkGasSchedule> {
    match spec {
        TaikoSpecId::UNZEN => Some(&UNZEN_ZK_GAS_SCHEDULE),
        _ => None,
    }
}
