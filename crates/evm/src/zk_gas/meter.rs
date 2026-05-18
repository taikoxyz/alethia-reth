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
}

impl<'a> ZkGasMeter<'a> {
    /// Creates a new meter for the provided consensus schedule.
    pub const fn new(schedule: &'a ZkGasSchedule) -> Self {
        Self { schedule, block_zk_gas_used: 0, tx_zk_gas_used: 0 }
    }

    /// Resets the in-flight zk gas for the current transaction.
    pub fn reset_transaction(&mut self) {
        #[cfg(test)]
        record_charge(ChargeCall::ResetTx);
        self.tx_zk_gas_used = 0;
    }

    /// Promotes the current transaction's zk gas into the finalized block total.
    pub fn commit_transaction(&mut self) -> Result<(), ZkGasOutcome> {
        #[cfg(test)]
        record_charge(ChargeCall::CommitTx);
        self.block_zk_gas_used = self
            .block_zk_gas_used
            .checked_add(self.tx_zk_gas_used)
            .ok_or(ZkGasOutcome::LimitExceeded)?;
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
    pub fn charge_opcode(&mut self, opcode: u8, raw_gas: u64) -> Result<(), ZkGasOutcome> {
        #[cfg(test)]
        record_charge(ChargeCall::Opcode { opcode, raw_gas });
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
        #[cfg(test)]
        record_charge(ChargeCall::Precompile { addr_low_byte: address_low_byte, gas_used });
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
        #[cfg(test)]
        record_charge(ChargeCall::TxIntrinsic);
        self.charge_amount(self.schedule.tx_intrinsic_zk_gas)
    }

    /// Applies a checked zk gas charge against the current transaction and block budget.
    fn charge_amount(&mut self, charge: u64) -> Result<(), ZkGasOutcome> {
        let next_tx = self.tx_zk_gas_used.checked_add(charge).ok_or(ZkGasOutcome::LimitExceeded)?;
        let projected_block_zk_gas_used =
            self.block_zk_gas_used.checked_add(next_tx).ok_or(ZkGasOutcome::LimitExceeded)?;
        if projected_block_zk_gas_used > self.schedule.block_limit {
            return Err(ZkGasOutcome::LimitExceeded);
        }
        self.tx_zk_gas_used = next_tx;
        Ok(())
    }
}

#[cfg(test)]
mod test_recorder {
    use std::cell::RefCell;

    /// One recorded interaction with the zk gas meter.
    #[derive(Clone, PartialEq, Eq, Debug)]
    pub enum ChargeCall {
        /// `ZkGasMeter::charge_opcode(opcode, raw_gas)`.
        Opcode { opcode: u8, raw_gas: u64 },
        /// `ZkGasMeter::charge_precompile(addr_low_byte, gas_used)`.
        Precompile { addr_low_byte: u8, gas_used: u64 },
        /// `ZkGasMeter::charge_tx_intrinsic()`.
        TxIntrinsic,
        /// `ZkGasMeter::reset_transaction()`.
        ResetTx,
        /// `ZkGasMeter::commit_transaction()`.
        CommitTx,
    }

    thread_local! {
        static RECORDER: RefCell<Option<Vec<ChargeCall>>> = const { RefCell::new(None) };
    }

    /// Installs an empty recorder for the current thread, discarding any prior recording.
    pub fn install() {
        RECORDER.with(|r| *r.borrow_mut() = Some(Vec::new()));
    }

    /// Removes and returns the current recording, leaving the thread without a recorder.
    pub fn take() -> Option<Vec<ChargeCall>> {
        RECORDER.with(|r| r.borrow_mut().take())
    }

    /// Pushes one call into the active recorder, if any is installed.
    pub fn record(call: ChargeCall) {
        RECORDER.with(|r| {
            if let Some(v) = r.borrow_mut().as_mut() {
                v.push(call);
            }
        });
    }
}

#[cfg(test)]
pub(crate) use test_recorder::{
    ChargeCall, install as install_charge_recorder, record as record_charge,
    take as take_charge_recording,
};
