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
    /// Remaining zk gas budget for the current transaction.
    ///
    /// Equals `schedule.block_limit - block_zk_gas_used` and is refreshed by
    /// `reset_transaction` and `commit_transaction`. Lets `charge_amount`
    /// short-circuit the three-checked-add liveness check used previously.
    tx_budget: u64,
}

impl<'a> ZkGasMeter<'a> {
    /// Creates a new meter for the provided consensus schedule.
    pub const fn new(schedule: &'a ZkGasSchedule) -> Self {
        Self { schedule, block_zk_gas_used: 0, tx_zk_gas_used: 0, tx_budget: schedule.block_limit }
    }

    /// Resets the in-flight zk gas for the current transaction and refreshes
    /// the cached `tx_budget` from the current finalized block usage.
    pub fn reset_transaction(&mut self) {
        self.tx_zk_gas_used = 0;
        self.tx_budget = self.schedule.block_limit.saturating_sub(self.block_zk_gas_used);
    }

    /// Promotes the current transaction's zk gas into the finalized block total
    /// and refreshes the cached `tx_budget`.
    pub fn commit_transaction(&mut self) -> Result<(), ZkGasOutcome> {
        self.block_zk_gas_used = self
            .block_zk_gas_used
            .checked_add(self.tx_zk_gas_used)
            .ok_or(ZkGasOutcome::LimitExceeded)?;
        debug_assert!(
            self.block_zk_gas_used <= self.schedule.block_limit,
            "block_zk_gas_used={} exceeded schedule.block_limit={} after promoting tx={}; \
             charge_amount enforces the per-tx upper bound, so this can only fire if a \
             caller mutated block state outside the meter API",
            self.block_zk_gas_used,
            self.schedule.block_limit,
            self.tx_zk_gas_used,
        );
        self.tx_zk_gas_used = 0;
        self.tx_budget = self.schedule.block_limit.saturating_sub(self.block_zk_gas_used);
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

    /// Returns the zk gas budget remaining for the current transaction.
    pub const fn tx_budget(&self) -> u64 {
        self.tx_budget
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

    /// Charges the fixed per-transaction intrinsic zk gas defined by the active schedule.
    ///
    /// Mirrors `TX_INTRINSIC_ZK_GAS` from the Unzen zk gas spec: the charge accumulates into the
    /// in-flight transaction total and is only promoted into the block total when the transaction
    /// commits. A schedule value of `0` (Masaya) makes this a no-op.
    pub fn charge_tx_intrinsic(&mut self) -> Result<(), ZkGasOutcome> {
        self.charge_amount(self.schedule.tx_intrinsic_zk_gas)
    }

    /// Applies a checked zk gas charge against the current transaction budget.
    ///
    /// `tx_budget` is refreshed by `reset_transaction` and `commit_transaction`
    /// to equal `block_limit - block_zk_gas_used`. Saturating-add handles the
    /// `u64` overflow case: if the sum would overflow, it caps at `u64::MAX`,
    /// which is always greater than `tx_budget` (block limits are far below
    /// `u64::MAX`), so the comparison still rejects.
    fn charge_amount(&mut self, charge: u64) -> Result<(), ZkGasOutcome> {
        let next_tx = self.tx_zk_gas_used.saturating_add(charge);
        if next_tx > self.tx_budget {
            return Err(ZkGasOutcome::LimitExceeded);
        }
        self.tx_zk_gas_used = next_tx;
        // Tripwire: catches `tx_budget` drifting out of sync with the live
        // `block_zk_gas_used` value. Runs only in debug builds.
        debug_assert!(
            self.block_zk_gas_used.saturating_add(self.tx_zk_gas_used) <= self.schedule.block_limit,
            "tx_budget drifted: block={} tx={} limit={}",
            self.block_zk_gas_used,
            self.tx_zk_gas_used,
            self.schedule.block_limit,
        );
        Ok(())
    }

    /// Test-only: forces the meter into an arbitrary `(block, tx)` state without
    /// running it through `commit_transaction`. Used by the `charge_amount`
    /// equivalence test to probe invariant-breaking combinations.
    #[cfg(test)]
    pub(crate) fn force_state_for_test(&mut self, block_zk_gas_used: u64, tx_zk_gas_used: u64) {
        self.block_zk_gas_used = block_zk_gas_used;
        self.tx_zk_gas_used = tx_zk_gas_used;
        self.tx_budget = self.schedule.block_limit.saturating_sub(block_zk_gas_used);
    }

    /// Test-only: exposes `charge_amount` directly so equivalence tests do not
    /// need to round-trip through `charge_opcode`'s multiplier lookup.
    #[cfg(test)]
    pub(crate) fn charge_amount_for_test(&mut self, charge: u64) -> Result<(), ZkGasOutcome> {
        self.charge_amount(charge)
    }
}
