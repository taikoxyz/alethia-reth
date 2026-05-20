//! Checked zk gas accounting for a single metered block execution.

use super::schedule::ZkGasSchedule;

/// Outcome used when zk gas charging exceeds the active block budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZkGasOutcome {
    /// Charging this operation would exceed the zk gas limit or overflow `u64` arithmetic.
    LimitExceeded,
}

/// Checked zk gas accounting for a single metered block execution.
pub struct ZkGasMeter<'a> {
    /// Consensus-owned schedule that defines multipliers and the block limit.
    schedule: &'a ZkGasSchedule,
    /// Finalized zk gas accumulated from fully committed transactions.
    block_zk_gas_used: u64,
    /// In-flight zk gas accumulated for the currently executing transaction.
    tx_zk_gas_used: u64,
    /// Remaining block zk gas budget after finalized and in-flight transaction charges.
    remaining_zk_gas: u64,
}

impl<'a> ZkGasMeter<'a> {
    /// Creates a new meter for the provided consensus schedule.
    pub const fn new(schedule: &'a ZkGasSchedule) -> Self {
        Self {
            schedule,
            block_zk_gas_used: 0,
            tx_zk_gas_used: 0,
            remaining_zk_gas: schedule.block_limit,
        }
    }

    /// Resets the in-flight zk gas for the current transaction.
    pub fn reset_transaction(&mut self) {
        self.tx_zk_gas_used = 0;
        self.remaining_zk_gas = self.schedule.block_limit.saturating_sub(self.block_zk_gas_used);
    }

    /// Promotes the current transaction's zk gas into the finalized block total.
    pub fn commit_transaction(&mut self) -> Result<(), ZkGasOutcome> {
        let next_block = self
            .block_zk_gas_used
            .checked_add(self.tx_zk_gas_used)
            .ok_or(ZkGasOutcome::LimitExceeded)?;
        if next_block > self.schedule.block_limit {
            return Err(ZkGasOutcome::LimitExceeded);
        }
        self.block_zk_gas_used = next_block;
        self.tx_zk_gas_used = 0;
        Ok(())
    }

    /// Returns the finalized zk gas from fully committed transactions.
    pub const fn block_zk_gas_used(&self) -> u64 {
        self.block_zk_gas_used
    }

    /// Returns the consensus schedule that backs this meter.
    pub const fn schedule(&self) -> &'a ZkGasSchedule {
        self.schedule
    }

    /// Returns the in-flight zk gas accumulated for the current transaction.
    pub const fn tx_zk_gas_used(&self) -> u64 {
        self.tx_zk_gas_used
    }

    /// Charges zk gas for a single opcode execution.
    #[inline(always)]
    pub fn charge_opcode(&mut self, opcode: u8, raw_gas: u64) -> Result<(), ZkGasOutcome> {
        let multiplier = u64::from(self.schedule.opcode_multipliers[usize::from(opcode)]);
        let charge = raw_gas.checked_mul(multiplier).ok_or(ZkGasOutcome::LimitExceeded)?;
        self.charge_amount(charge)
    }

    /// Charges a CALL/CREATE-family opcode that dispatched child work.
    #[inline(always)]
    pub fn charge_spawn_opcode(&mut self, opcode: u8) -> Result<(), ZkGasOutcome> {
        self.charge_opcode(opcode, spawn_estimate(self.schedule, opcode))
    }

    /// Charges zk gas for a single precompile execution.
    #[inline(always)]
    pub fn charge_precompile(
        &mut self,
        address_low_byte: u8,
        gas_used: u64,
    ) -> Result<(), ZkGasOutcome> {
        let multiplier =
            u64::from(self.schedule.precompile_multipliers[usize::from(address_low_byte)]);
        let charge = gas_used.checked_mul(multiplier).ok_or(ZkGasOutcome::LimitExceeded)?;
        self.charge_amount(charge)
    }

    /// Charges the fixed per-transaction intrinsic zk gas defined by the active schedule.
    ///
    /// Mirrors `TX_INTRINSIC_ZK_GAS` from the Unzen zk gas spec: the charge accumulates into the
    /// in-flight transaction total and is only promoted into the block total when the transaction
    /// commits. A schedule value of `0` (Masaya) makes this a no-op.
    pub fn charge_tx_intrinsic(&mut self) -> Result<(), ZkGasOutcome> {
        self.charge_amount(self.schedule.tx_intrinsic_zk_gas)
    }

    /// Applies a checked zk gas charge against the current transaction and block budget.
    #[inline(always)]
    fn charge_amount(&mut self, charge: u64) -> Result<(), ZkGasOutcome> {
        if charge > self.remaining_zk_gas {
            return Err(ZkGasOutcome::LimitExceeded);
        }
        self.tx_zk_gas_used =
            self.tx_zk_gas_used.checked_add(charge).ok_or(ZkGasOutcome::LimitExceeded)?;
        self.remaining_zk_gas -= charge;
        Ok(())
    }
}

/// Returns `true` when `opcode` belongs to the CALL/CREATE spawn-opcode set.
#[inline(always)]
pub(crate) fn is_spawn_opcode(opcode: u8) -> bool {
    matches!(opcode, 0xf0 | 0xf1 | 0xf2 | 0xf4 | 0xf5 | 0xfa)
}

/// Returns the fixed raw-gas estimate used when a spawn opcode opens child work.
#[inline(always)]
fn spawn_estimate(schedule: &ZkGasSchedule, opcode: u8) -> u64 {
    match opcode {
        0xf1 => schedule.spawn_estimates.call,
        0xf2 => schedule.spawn_estimates.callcode,
        0xf4 => schedule.spawn_estimates.delegatecall,
        0xfa => schedule.spawn_estimates.staticcall,
        0xf0 => schedule.spawn_estimates.create,
        0xf5 => schedule.spawn_estimates.create2,
        _ => unreachable!("spawn estimate requested for non-spawn opcode: {opcode:#x}"),
    }
}
