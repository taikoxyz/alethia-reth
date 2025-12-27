use std::sync::Arc;

use alethia_reth_chainspec::zk_gas::ZkGasConfig;
use reth_revm::{
    context_interface::ContextTr,
    interpreter::{
        Host, Interpreter,
        instruction_context::InstructionContext,
        instructions::{Instruction, InstructionTable},
        interpreter::EthInterpreter,
        interpreter_types::Jumps,
    },
};

/// Structured zk gas violation information for the current transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZkGasViolation {
    /// The transaction exceeded its configured zk gas limit.
    TxLimitExceeded {
        /// The configured zk gas limit.
        limit: u64,
        /// The zk gas used when the limit was exceeded.
        used: u64,
    },
    /// Zk gas accounting overflowed during the transaction.
    TxOverflow {
        /// The zk gas used before overflow was detected.
        used: u64,
    },
}

/// Tracks zk gas usage for a single transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZkGasMeter {
    /// Zk gas configuration (opcode multipliers and limits).
    config: ZkGasConfig,
    /// Accumulated zk gas for the current transaction.
    tx_zk_used: u64,
    /// Flag indicating whether zk metering is currently enabled.
    metering_enabled: bool,
    /// Structured violation recorded when zk gas exceeds limits or overflows.
    violation: Option<ZkGasViolation>,
}

impl ZkGasMeter {
    /// Creates a new zk gas meter with the provided configuration.
    pub fn new(config: ZkGasConfig) -> Self {
        Self { config, tx_zk_used: 0, metering_enabled: true, violation: None }
    }

    /// Resets per-transaction accounting state.
    pub fn reset_for_tx(&mut self) {
        self.tx_zk_used = 0;
        self.violation = None;
    }

    /// Returns the zk gas used by the current transaction.
    pub fn tx_zk_used(&self) -> u64 {
        self.tx_zk_used
    }

    /// Returns the zk gas violation, if any.
    pub fn violation(&self) -> Option<ZkGasViolation> {
        self.violation
    }

    /// Enables or disables zk metering for the current execution.
    pub fn set_metering_enabled(&mut self, enabled: bool) {
        self.metering_enabled = enabled;
    }

    /// Applies an opcode gas delta to the zk gas accumulator.
    pub fn on_opcode(&mut self, opcode: u8, gas_delta: u64) {
        if !self.metering_enabled || self.violation.is_some() {
            return;
        }

        let multiplier = u64::from(self.config.multipliers[opcode as usize]);
        let multiplied = match gas_delta.checked_mul(multiplier) {
            Some(value) => value,
            None => {
                self.violation = Some(ZkGasViolation::TxOverflow { used: self.tx_zk_used });
                return;
            }
        };

        let next_total = match self.tx_zk_used.checked_add(multiplied) {
            Some(value) => value,
            None => {
                self.violation = Some(ZkGasViolation::TxOverflow { used: self.tx_zk_used });
                return;
            }
        };

        self.tx_zk_used = next_total;

        if self.tx_zk_used > self.config.tx_limit {
            self.violation = Some(ZkGasViolation::TxLimitExceeded {
                limit: self.config.tx_limit,
                used: self.tx_zk_used,
            });
        }
    }
}

/// Chain context stored inside the revm Context to track zk gas state.
#[derive(Debug)]
pub struct TaikoChainContext {
    /// Zk gas meter used for per-transaction accounting.
    zk_meter: ZkGasMeter,
    /// Raw pointer to the base instruction table used for opcode execution, managed via Arc ref
    /// counts.
    base_instruction_ptr: *const (),
    /// Function pointer used to clone the base instruction table reference count.
    base_instruction_clone: unsafe fn(*const ()),
    /// Function pointer used to drop the base instruction table reference count.
    base_instruction_drop: unsafe fn(*const ()),
}

impl Clone for TaikoChainContext {
    /// Clones the chain context, including bumping the base instruction table ref count.
    ///
    /// This preserves the shared instruction table across cloned contexts.
    fn clone(&self) -> Self {
        unsafe { (self.base_instruction_clone)(self.base_instruction_ptr) };
        Self {
            zk_meter: self.zk_meter,
            base_instruction_ptr: self.base_instruction_ptr,
            base_instruction_clone: self.base_instruction_clone,
            base_instruction_drop: self.base_instruction_drop,
        }
    }
}

impl Drop for TaikoChainContext {
    /// Drops the chain context and releases the base instruction table reference.
    ///
    /// This balances the ref-count increment performed during construction and cloning.
    fn drop(&mut self) {
        unsafe { (self.base_instruction_drop)(self.base_instruction_ptr) };
    }
}

impl TaikoChainContext {
    /// Creates a new chain context with the provided zk gas configuration and base instruction
    /// table.
    pub fn new<CTX>(
        config: ZkGasConfig,
        base_instruction_table: Arc<InstructionTable<EthInterpreter, CTX>>,
    ) -> Self
    where
        CTX: Host,
    {
        let base_instruction_ptr = Arc::into_raw(base_instruction_table) as *const ();
        Self {
            zk_meter: ZkGasMeter::new(config),
            base_instruction_ptr,
            base_instruction_clone: clone_instruction_table::<CTX>,
            base_instruction_drop: drop_instruction_table::<CTX>,
        }
    }

    /// Resets zk gas accounting before executing a new transaction.
    pub fn reset_for_tx(&mut self) {
        self.zk_meter.reset_for_tx();
    }

    /// Records an opcode gas delta into the zk gas meter.
    pub fn on_opcode(&mut self, opcode: u8, gas_delta: u64) {
        self.zk_meter.on_opcode(opcode, gas_delta);
    }

    /// Returns the zk gas used by the current transaction.
    pub fn tx_zk_used(&self) -> u64 {
        self.zk_meter.tx_zk_used()
    }

    /// Returns the zk gas violation, if any, for the current transaction.
    pub fn violation(&self) -> Option<ZkGasViolation> {
        self.zk_meter.violation()
    }

    /// Enables or disables zk metering for the current execution.
    pub fn set_metering_enabled(&mut self, enabled: bool) {
        self.zk_meter.set_metering_enabled(enabled);
    }

    /// Returns the base instruction table for this host type.
    pub fn base_instruction_table<CTX>(&self) -> &InstructionTable<EthInterpreter, CTX>
    where
        CTX: Host + ContextTr<Chain = TaikoChainContext>,
    {
        unsafe { &*(self.base_instruction_ptr as *const InstructionTable<EthInterpreter, CTX>) }
    }
}

/// Increments the reference count for the typed base instruction table.
unsafe fn clone_instruction_table<CTX>(ptr: *const ())
where
    CTX: Host,
{
    // SAFETY: Caller guarantees `ptr` originated from `Arc::into_raw` for this CTX.
    let typed_ptr = ptr as *const InstructionTable<EthInterpreter, CTX>;
    unsafe { Arc::increment_strong_count(typed_ptr) };
}

/// Decrements the reference count for the typed base instruction table.
unsafe fn drop_instruction_table<CTX>(ptr: *const ())
where
    CTX: Host,
{
    // SAFETY: Caller guarantees `ptr` originated from `Arc::into_raw` for this CTX.
    let typed_ptr = ptr as *const InstructionTable<EthInterpreter, CTX>;
    unsafe { Arc::decrement_strong_count(typed_ptr) };
}

/// Provides zk gas state access for EVM wrappers.
pub trait ZkGasEvm {
    /// Returns the zk gas used by the most recently executed transaction.
    fn zk_gas_tx_used(&self) -> u64;

    /// Returns the zk gas violation for the current transaction, if any.
    fn zk_gas_violation(&self) -> Option<ZkGasViolation>;

    /// Enables or disables zk gas metering for the current execution.
    fn zk_gas_set_metering_enabled(&mut self, enabled: bool);

    /// Resets zk gas tracking before running a new transaction.
    fn zk_gas_reset_for_tx(&mut self);
}

/// Builds a zk-metered instruction table for a given host context type using the base table.
///
/// The base table provides the canonical opcode implementations and static gas values; this
/// wrapper keeps behavior identical while adding zk gas accounting hooks.
pub fn build_zk_instruction_table<CTX>(
    base_table: &InstructionTable<EthInterpreter, CTX>,
) -> InstructionTable<EthInterpreter, CTX>
where
    CTX: Host + ContextTr<Chain = TaikoChainContext>,
{
    let mut wrapped_table = *base_table;
    let mut opcode = 0u16;

    while opcode < 256 {
        let index = opcode as usize;
        let static_gas = base_table[index].static_gas();
        wrapped_table[index] = Instruction::new(zk_wrapped_instruction::<CTX>, static_gas);
        opcode += 1;
    }

    wrapped_table
}

/// Shared base instruction table for a given host context type.
/// Wrapper around every opcode instruction that records zk gas deltas.
fn zk_wrapped_instruction<CTX>(ctx: InstructionContext<'_, CTX, EthInterpreter>)
where
    CTX: Host + ContextTr<Chain = TaikoChainContext>,
{
    // Execute the base opcode and record the incremental gas for zk metering.
    let (opcode, gas_delta) = execute_with_zk_delta(&mut *ctx.interpreter, &mut *ctx.host);
    ctx.host.chain_mut().on_opcode(opcode, gas_delta);
}

/// Executes the base opcode and returns the opcode and gas delta.
fn execute_with_zk_delta<CTX>(
    interpreter: &mut Interpreter<EthInterpreter>,
    host: &mut CTX,
) -> (u8, u64)
where
    CTX: Host + ContextTr<Chain = TaikoChainContext>,
{
    // Capture the opcode and gas before executing the base instruction.
    let opcode = interpreter.bytecode.opcode();
    let gas_before = interpreter.gas.spent();

    let base = host.chain().base_instruction_table::<CTX>();
    base[opcode as usize].execute(InstructionContext { interpreter, host });

    // Compute the delta after the base instruction executes.
    let gas_after = interpreter.gas.spent();
    let gas_delta = gas_after.saturating_sub(gas_before);
    (opcode, gas_delta)
}

#[cfg(test)]
mod tests {
    use super::{TaikoChainContext, ZkGasMeter};
    use crate::alloy::TaikoEvmContext;
    use alethia_reth_chainspec::zk_gas::ZkGasConfig;
    use reth_revm::{
        db::EmptyDB,
        interpreter::{instructions::instruction_table, interpreter::EthInterpreter},
    };
    use std::sync::Arc;

    /// Ensures the zk gas meter accumulates using the configured multiplier.
    #[test]
    fn zk_gas_accumulates_with_multiplier() {
        let cfg = ZkGasConfig::default();
        let mut meter = ZkGasMeter::new(cfg);
        meter.on_opcode(0x01, 3);
        meter.on_opcode(0x01, 3);
        assert_eq!(meter.tx_zk_used(), 6);
    }

    /// Confirms the meter flags when the tx zk gas limit is exceeded.
    #[test]
    fn zk_gas_exceeds_tx_limit() {
        let mut cfg = ZkGasConfig::default();
        cfg.tx_limit = 5;
        let mut meter = ZkGasMeter::new(cfg);
        meter.on_opcode(0x01, 3);
        meter.on_opcode(0x01, 3);
        assert!(matches!(meter.violation(), Some(super::ZkGasViolation::TxLimitExceeded { .. })));
    }

    /// Ensures chain context surfaces meter state helpers.
    #[test]
    fn chain_context_tracks_meter() {
        let cfg = ZkGasConfig::default();
        let base_table = Arc::new(instruction_table::<EthInterpreter, TaikoEvmContext<EmptyDB>>());
        let mut ctx = TaikoChainContext::new(cfg, base_table);
        ctx.on_opcode(0x01, 3);
        assert_eq!(ctx.tx_zk_used(), 3);
    }
}
