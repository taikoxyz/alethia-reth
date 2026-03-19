//! Checked zk gas accounting for a single Uzen block execution.

use super::schedule::ZkGasSchedule;

/// Outcome used when zk gas charging exceeds the Uzen block budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZkGasOutcome {
    /// Charging this operation would exceed the zk gas limit or overflow `u64` arithmetic.
    LimitExceeded,
}

/// Checked zk gas accounting for a single Uzen block execution.
pub struct UzenZkGasMeter<'a> {
    /// Consensus-owned schedule that defines multipliers and the block limit.
    schedule: &'a ZkGasSchedule,
    /// Finalized zk gas accumulated from fully committed transactions.
    block_zk_gas_used: u64,
    /// In-flight zk gas accumulated for the currently executing transaction.
    tx_zk_gas_used: u64,
}

impl<'a> UzenZkGasMeter<'a> {
    /// Creates a new meter for the provided consensus schedule.
    pub const fn new(schedule: &'a ZkGasSchedule) -> Self {
        Self { schedule, block_zk_gas_used: 0, tx_zk_gas_used: 0 }
    }

    /// Resets the in-flight zk gas for the current transaction.
    pub fn reset_transaction(&mut self) {
        self.tx_zk_gas_used = 0;
    }

    /// Promotes the current transaction's zk gas into the finalized block total.
    pub fn commit_transaction(&mut self) -> Result<(), ZkGasOutcome> {
        self.block_zk_gas_used =
            self.block_zk_gas_used.checked_add(self.tx_zk_gas_used).ok_or(ZkGasOutcome::LimitExceeded)?;
        self.tx_zk_gas_used = 0;
        Ok(())
    }

    /// Returns the finalized zk gas from fully committed transactions.
    pub const fn block_zk_gas_used(&self) -> u64 {
        self.block_zk_gas_used
    }

    /// Returns the in-flight zk gas accumulated for the current transaction.
    pub const fn tx_zk_gas_used(&self) -> u64 {
        self.tx_zk_gas_used
    }

    /// Charges zk gas for a single opcode execution.
    pub fn charge_opcode(&mut self, opcode: u8, raw_gas: u64) -> Result<(), ZkGasOutcome> {
        let multiplier = u64::from(self.schedule.opcode_multipliers[usize::from(opcode)]);
        let charge = raw_gas.checked_mul(multiplier).ok_or(ZkGasOutcome::LimitExceeded)?;
        self.charge_amount(charge)
    }

    /// Charges zk gas for a single precompile execution.
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

    /// Applies a checked zk gas charge against the current transaction and block budget.
    fn charge_amount(&mut self, charge: u64) -> Result<(), ZkGasOutcome> {
        let next_tx = self.tx_zk_gas_used.checked_add(charge).ok_or(ZkGasOutcome::LimitExceeded)?;
        let next_block =
            self.block_zk_gas_used.checked_add(next_tx).ok_or(ZkGasOutcome::LimitExceeded)?;
        if next_block > self.schedule.block_limit {
            return Err(ZkGasOutcome::LimitExceeded);
        }
        self.tx_zk_gas_used = next_tx;
        Ok(())
    }
}
