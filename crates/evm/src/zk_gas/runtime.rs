//! Interpreter-side zk gas metering used by the production no-inspector path.

use alloy_primitives::{Address, B256, U256};
use reth_revm::{
    Database,
    context::{ContextError, ContextTr},
    interpreter::{
        GasTable, InstructionResult, Interpreter, InterpreterAction,
        instructions::InstructionTable,
        interpreter::EthInterpreter,
        interpreter_types::{InputsTr, Jumps, LoopControl},
    },
};

use crate::alloy::TaikoEvmContext;

#[cfg(test)]
std::thread_local! {
    static BN254_WORK_UNIT_OBSERVATIONS: core::cell::Cell<usize> = const {
        core::cell::Cell::new(0)
    };
}

use super::{
    adapter::ZK_GAS_LIMIT_ERR,
    meter::{ZkGasMeter, ZkGasOutcome, is_spawn_opcode},
    pricing::{
        AccessClass, CalldataAccessClass, MemorySelectors, OperationModelInput,
        OperationPricingInput, OperationRoute, StateAccess, StorageBranch, ValueClass,
        ZkGasAccumulator, ZkGasAccumulatorError, ZkGasModel,
    },
};

/// Pre-step execution facts shared by production and inspected execution.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingPricingStep {
    /// Opcode selected before execution.
    opcode: u8,
    /// Remaining EVM gas before the step.
    gas_remaining: u64,
    /// Allocated memory words before the step.
    memory_words: usize,
    /// Cumulative EVM memory-expansion gas before the step.
    memory_expansion_cost: u64,
    /// Typed inputs that must be captured before interpreter mutation.
    model_input: Result<PendingModelInput, ()>,
    /// Account addressed directly by account-inspection opcodes.
    resource_account: Option<Address>,
}

/// Type-erased hooks let the generic EVM wrapper collect concrete Taiko context facts without
/// weakening non-Taiko test contexts or duplicating the interpreter loop.
pub(crate) struct PricingExecutionHooks<CTX> {
    /// Captures pre-step typed facts for a concrete context.
    capture: fn(&Interpreter<EthInterpreter>, &CTX) -> PendingPricingStep,
    /// Finalizes post-step selectors for a concrete context.
    finish: fn(
        PendingPricingStep,
        &mut Interpreter<EthInterpreter>,
        &CTX,
        &mut ZkGasAccumulator,
    ) -> Result<(), ZkGasAccumulatorError>,
}

impl<CTX> Clone for PricingExecutionHooks<CTX> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<CTX> Copy for PricingExecutionHooks<CTX> {}

impl<DB: Database> PricingExecutionHooks<TaikoEvmContext<DB>> {
    /// Hooks for the concrete production Taiko context.
    pub(crate) const TAIKO: Self =
        Self { capture: PendingPricingStep::capture, finish: PendingPricingStep::finish };
}

#[derive(Clone, Copy, Debug)]
/// Typed model input retained between the pre-step and post-step hooks.
enum PendingModelInput {
    /// Generated route metadata decides whether raw gas is required.
    Default,
    /// Fixed-cost contextual opcode.
    ContextFixed,
    /// Contextual opcode classified by zero/nonzero value.
    ContextValue(ValueClass),
    /// CALLDATALOAD access-width class.
    CalldataLoad(CalldataAccessClass),
    /// CALLDATASIZE input byte length.
    CalldataSize(u64),
    /// EXP exponent byte length.
    Exp(u64),
    /// KECCAK input byte length.
    Keccak(u64),
    /// MLOAD/MSTORE/MSTORE8 memory access.
    MemoryAccess,
    /// MCOPY word count.
    MemoryCopy(u64),
    /// External copy byte count.
    Copy(u64),
    /// LOG topic and payload dimensions.
    Log {
        /// Number of emitted topics.
        topic_count: u64,
        /// Number of logged data bytes.
        data_bytes: u64,
    },
    /// RETURN byte length.
    Return(u64),
    /// SLOAD pre-state.
    StorageLoad(PendingStorageInput),
    /// SSTORE pre-state.
    StorageStore(PendingStorageInput),
}

#[derive(Clone, Copy, Debug)]
/// Storage facts captured before journal mutation.
struct PendingStorageInput {
    /// Contract whose storage is accessed.
    target: Address,
    /// Exact storage key.
    key: U256,
    /// Warm/cold access class at step start.
    access: AccessClass,
    /// Original transaction value when already cached.
    original: Option<U256>,
    /// Current transaction value when already cached.
    current: Option<U256>,
    /// Requested SSTORE value, absent for SLOAD.
    new: Option<U256>,
}

impl PendingPricingStep {
    /// Captures every typed feature that would be destroyed by opcode execution.
    pub(crate) fn capture<DB: Database>(
        interpreter: &Interpreter<EthInterpreter>,
        context: &TaikoEvmContext<DB>,
    ) -> Self {
        let opcode = interpreter.bytecode.opcode();
        let stack_value = |index| interpreter.stack.peek(index).map_err(|_| ());
        let stack_u64 = |index| u64::try_from(stack_value(index)?).map_err(|_| ());
        let model_input = match opcode {
            0x0a => stack_value(1)
                .map(|value| PendingModelInput::Exp(value.bit_len().div_ceil(8) as u64)),
            0x20 => stack_u64(1).map(PendingModelInput::Keccak),
            0x30 | 0x33 => Ok(PendingModelInput::ContextFixed),
            0x34 => {
                Ok(PendingModelInput::ContextValue(classify_value(interpreter.input.call_value())))
            }
            0x35 => stack_value(0).map(|offset| {
                let available = usize::try_from(offset)
                    .ok()
                    .map_or(0, |offset| interpreter.input.input().len().saturating_sub(offset));
                PendingModelInput::CalldataLoad(match available {
                    0 => CalldataAccessClass::Zero,
                    1..=31 => CalldataAccessClass::Partial,
                    _ => CalldataAccessClass::Full,
                })
            }),
            0x36 => Ok(PendingModelInput::CalldataSize(interpreter.input.input().len() as u64)),
            0x42 => Ok(PendingModelInput::ContextValue(classify_value(context.block.timestamp))),
            0x51..=0x53 => Ok(PendingModelInput::MemoryAccess),
            0x54 => capture_storage_input(interpreter, context, None)
                .map(PendingModelInput::StorageLoad),
            0x55 => capture_storage_input(interpreter, context, Some(1))
                .map(PendingModelInput::StorageStore),
            0x5e => stack_u64(2).map(|bytes| PendingModelInput::MemoryCopy(bytes.div_ceil(32))),
            0x37 | 0x3e => stack_u64(2).map(PendingModelInput::Copy),
            0xa0 => Ok(PendingModelInput::Default),
            0xa1..=0xa4 => stack_u64(1).map(|data_bytes| PendingModelInput::Log {
                topic_count: u64::from(opcode - 0xa0),
                data_bytes,
            }),
            0xf3 => stack_u64(1).map(PendingModelInput::Return),
            _ => Ok(PendingModelInput::Default),
        };
        let resource_account = matches!(opcode, 0x31 | 0x3b | 0x3c | 0x3f | 0xff)
            .then(|| interpreter.stack.peek(0).ok())
            .flatten()
            .map(|value| Address::from_word(B256::from(value.to_be_bytes())));
        Self {
            opcode,
            gas_remaining: interpreter.gas.remaining(),
            memory_words: interpreter.memory.len().div_ceil(32),
            memory_expansion_cost: interpreter.gas.memory().expansion_cost,
            model_input,
            resource_account,
        }
    }

    /// Finalizes the shared snapshot and stages one priced event and its exact resources.
    pub(crate) fn finish<DB: Database>(
        self,
        interpreter: &mut Interpreter<EthInterpreter>,
        context: &TaikoEvmContext<DB>,
        accumulator: &mut ZkGasAccumulator,
    ) -> Result<(), ZkGasAccumulatorError> {
        let raw_gas = self.gas_remaining.saturating_sub(interpreter.gas.remaining());
        let selected_spawn = is_spawn_opcode(self.opcode) &&
            matches!(interpreter.bytecode.action(), Some(InterpreterAction::NewFrame(_)));
        if selected_spawn {
            accumulator.select_spawn(self.opcode)?;
            return Ok(());
        }

        if interpreter.bytecode.instruction_result().is_none_or(InstructionResult::is_ok) &&
            let Some(address) = self.resource_account
        {
            accumulator.observe_state_access(StateAccess::Account(address))?;
        }
        match self.model_input {
            Ok(PendingModelInput::StorageLoad(storage)) => {
                accumulator.observe_state_access(StateAccess::StorageRead {
                    address: storage.target,
                    slot: storage.key,
                })?
            }
            Ok(PendingModelInput::StorageStore(storage)) => {
                accumulator.observe_state_access(StateAccess::StorageWrite {
                    address: storage.target,
                    slot: storage.key,
                })?
            }
            _ => {}
        }

        let model_input = self
            .model_input
            .and_then(|pending| self.finish_model_input(pending, raw_gas, interpreter, context));
        let instruction_result = interpreter.bytecode.instruction_result();
        let return_halted =
            self.opcode == 0xf3 && instruction_result != Some(InstructionResult::Return);
        let storage_halted = matches!(self.opcode, 0x54 | 0x55) && instruction_result.is_some();
        let unexpected_halt = instruction_result.is_some_and(|result| {
            !(result.is_ok() || self.opcode == 0xfd && result == InstructionResult::Revert)
        });
        let (model_input, feature_error) = match model_input {
            Ok(input) if !return_halted && !storage_halted && !unexpected_halt => (input, false),
            Ok(_) | Err(()) => (OperationModelInput::None, true),
        };
        accumulator.observe_step(OperationPricingInput {
            route: OperationRoute::Opcode(self.opcode),
            interpreter_raw_gas: Some(raw_gas),
            native_gas: None,
            model_input,
            feature_error,
        })
    }

    /// Resolves post-step memory and storage facts into a sealed model input.
    fn finish_model_input<DB: Database>(
        self,
        pending: PendingModelInput,
        raw_gas: u64,
        interpreter: &Interpreter<EthInterpreter>,
        context: &TaikoEvmContext<DB>,
    ) -> Result<OperationModelInput, ()> {
        let after_words = interpreter.memory.len().div_ceil(32);
        let grew = after_words > self.memory_words;
        let memory = MemorySelectors {
            growth_events: u64::from(grew),
            evm_gas_delta: if grew {
                interpreter.gas.memory().expansion_cost.saturating_sub(self.memory_expansion_cost)
            } else {
                0
            },
            boundary_4k_events: u64::from(
                extra_4k_pages(after_words) > extra_4k_pages(self.memory_words),
            ),
        };
        Ok(match pending {
            PendingModelInput::Default => {
                ZkGasModel.default_opcode_model_input(self.opcode, raw_gas).map_err(|_| ())?
            }
            PendingModelInput::ContextFixed => OperationModelInput::ContextFixed,
            PendingModelInput::ContextValue(class) => OperationModelInput::ContextValue { class },
            PendingModelInput::CalldataLoad(access) => OperationModelInput::CalldataLoad { access },
            PendingModelInput::CalldataSize(input_length) => {
                OperationModelInput::CalldataSize { input_length }
            }
            PendingModelInput::Exp(exponent_byte_length) => {
                OperationModelInput::Exp { exponent_byte_length }
            }
            PendingModelInput::Keccak(input_length) => {
                OperationModelInput::Keccak { input_length, memory }
            }
            PendingModelInput::MemoryAccess => OperationModelInput::MemoryAccess { memory },
            PendingModelInput::MemoryCopy(copy_words) => {
                OperationModelInput::MemoryCopy { copy_words, memory }
            }
            PendingModelInput::Copy(copied_bytes) => OperationModelInput::Copy {
                copied_bytes,
                copy_words: copied_bytes.div_ceil(32),
                memory,
            },
            PendingModelInput::Log { topic_count, data_bytes } => OperationModelInput::Log {
                topic_count,
                data_bytes,
                data_words: data_bytes.div_ceil(32),
                memory,
            },
            PendingModelInput::Return(returned_bytes) => OperationModelInput::Return {
                returned_bytes,
                return_words: returned_bytes.div_ceil(32),
                memory,
            },
            PendingModelInput::StorageLoad(storage) => {
                ensure_storage_post_state(storage, context)?;
                OperationModelInput::StorageLoad { access: storage.access }
            }
            PendingModelInput::StorageStore(storage) => OperationModelInput::StorageStore {
                access: storage.access,
                branch: finish_storage_store(storage, context)?,
            },
        })
    }
}

/// Classifies a context value for the zero/nonzero selector.
fn classify_value(value: U256) -> ValueClass {
    if value.is_zero() { ValueClass::Zero } else { ValueClass::Nonzero }
}

/// Captures storage warmness and cached pre-state without forcing an early database load.
fn capture_storage_input<DB: Database>(
    interpreter: &Interpreter<EthInterpreter>,
    context: &TaikoEvmContext<DB>,
    new_stack_index: Option<usize>,
) -> Result<PendingStorageInput, ()> {
    let key = interpreter.stack.peek(0).map_err(|_| ())?;
    let new =
        new_stack_index.map(|index| interpreter.stack.peek(index).map_err(|_| ())).transpose()?;
    let target = interpreter.input.target_address();
    let journal = &context.journaled_state.inner;
    let access_list_warm = journal.warm_addresses.is_storage_warm(&target, &key);
    let cached = journal.state.get(&target).and_then(|account| account.storage.get(&key));
    let (access, original, current) = cached.map_or_else(
        || (if access_list_warm { AccessClass::Warm } else { AccessClass::Cold }, None, None),
        |slot| {
            let stale = slot.is_cold_transaction_id(journal.transaction_id);
            (
                if stale && !access_list_warm { AccessClass::Cold } else { AccessClass::Warm },
                Some(if stale { slot.present_value() } else { slot.original_value() }),
                Some(slot.present_value()),
            )
        },
    );
    Ok(PendingStorageInput { target, key, access, original, current, new })
}

/// Verifies that a previously uncached SLOAD materialized the expected journal slot.
fn ensure_storage_post_state<DB: Database>(
    storage: PendingStorageInput,
    context: &TaikoEvmContext<DB>,
) -> Result<(), ()> {
    match (storage.original, storage.current) {
        (Some(_), Some(_)) => Ok(()),
        (None, None)
            if context
                .journaled_state
                .inner
                .state
                .get(&storage.target)
                .and_then(|account| account.storage.get(&storage.key))
                .is_some() =>
        {
            Ok(())
        }
        _ => Err(()),
    }
}

/// Classifies an attempted SSTORE against its captured pre-state.
fn finish_storage_store<DB: Database>(
    storage: PendingStorageInput,
    context: &TaikoEvmContext<DB>,
) -> Result<StorageBranch, ()> {
    let (original, current) = match (storage.original, storage.current) {
        (Some(original), Some(current)) => (original, current),
        (None, None) => {
            let slot = context
                .journaled_state
                .inner
                .state
                .get(&storage.target)
                .and_then(|account| account.storage.get(&storage.key))
                .ok_or(())?;
            (slot.original_value(), slot.original_value())
        }
        _ => return Err(()),
    };
    let new = storage.new.ok_or(())?;
    if current != original {
        if new == current {
            return Ok(StorageBranch::Noop);
        }
        if storage.access == AccessClass::Cold {
            return Err(());
        }
        return Ok(if new == original {
            StorageBranch::RestoreOriginal
        } else {
            StorageBranch::DirtyRewrite
        });
    }
    Ok(if new == current {
        StorageBranch::Noop
    } else if current.is_zero() {
        StorageBranch::Set
    } else if new.is_zero() {
        StorageBranch::Clear
    } else {
        StorageBranch::Reset
    })
}

/// Returns allocated 4KiB pages beyond the uncharged first page.
const fn extra_4k_pages(memory_words: usize) -> usize {
    memory_words.div_ceil(128).saturating_sub(1)
}

/// BN254 scalar field order used by the G1 multiplication precompile.
const BN254_GROUP_ORDER: U256 = U256::from_be_bytes(alloy_primitives::hex!(
    "30644e72e131a029b85045b68181585d2833e84879b9709143e1f593f0000001"
));

/// Returns the authenticated BN254 G1 multiplication scalar work selector.
pub(crate) fn bn254_work_units(input: &[u8]) -> u64 {
    #[cfg(test)]
    BN254_WORK_UNIT_OBSERVATIONS.set(BN254_WORK_UNIT_OBSERVATIONS.get() + 1);
    let mut padded = [0_u8; 96];
    let copied = input.len().min(padded.len());
    padded[..copied].copy_from_slice(&input[..copied]);
    let mut scalar = U256::from_be_slice(&padded[64..96]);
    while scalar >= BN254_GROUP_ORDER {
        scalar -= BN254_GROUP_ORDER;
    }
    scalar.bit_len() as u64 +
        scalar.to_be_bytes::<32>().iter().map(|byte| u64::from(byte.count_ones())).sum::<u64>()
}

#[cfg(test)]
/// Resets the thread-local BN254 selector-observation counter used by overhead tests.
pub(crate) fn reset_bn254_work_unit_observations() {
    BN254_WORK_UNIT_OBSERVATIONS.set(0);
}

#[cfg(test)]
/// Returns and clears the thread-local BN254 selector-observation counter.
pub(crate) fn take_bn254_work_unit_observations() -> usize {
    BN254_WORK_UNIT_OBSERVATIONS.replace(0)
}

/// Runs the interpreter loop while charging zk gas directly in the production path.
///
/// Mirrors revm's `Interpreter::run_plain` control flow — every step signals loop exit through
/// its `Err(InstructionResult)` — with one deliberate difference: an exceptional halt is
/// materialized *before* the step's zk gas charge instead of after the loop.
/// [`Interpreter::halt`] spends all remaining gas for `OutOfGas` results, and that forfeited
/// gas is part of the step's consensus charge: pre-2.4.0 revm called `halt_oog()` (spend-all)
/// inside `step` itself, taiko-geth's zk mirror follows that reference behavior, and these
/// totals feed `header.difficulty` on Unzen. revm's inspected loop materializes the halt
/// before `step_end` for the same reason, which keeps [`super::adapter::ZkGasInspector`]'s
/// measurements identical to this loop's.
#[inline]
pub(crate) fn run_metered_plain<CTX: ContextTr>(
    context: &mut CTX,
    interpreter: &mut Interpreter<EthInterpreter>,
    instruction_table: &InstructionTable<EthInterpreter, CTX>,
    gas_table: &GasTable,
    mut meter: Option<&mut ZkGasMeter<'static>>,
    mut pricing: Option<&mut ZkGasAccumulator>,
    pricing_hooks: Option<PricingExecutionHooks<CTX>>,
) -> InterpreterAction {
    loop {
        let pricing_step = if pricing.is_some() {
            pricing_hooks.map(|hooks| (hooks.capture)(interpreter, context))
        } else {
            None
        };
        let opcode = interpreter.bytecode.opcode();
        let gas_before = interpreter.gas.remaining();

        let step_result = interpreter.step(instruction_table, gas_table, context);

        // Materialize an exceptional halt before charging so the `OutOfGas` spend-all is
        // reflected in the measured step gas (see the function docs).
        if let Err(result) = step_result &&
            interpreter.bytecode.action().is_none()
        {
            interpreter.halt(result);
        }

        if let Some(meter) = meter.as_deref_mut() {
            let charge = if is_spawn_opcode(opcode) &&
                matches!(interpreter.bytecode.action(), Some(InterpreterAction::NewFrame(_)))
            {
                meter.charge_spawn_opcode(opcode)
            } else {
                meter.charge_opcode(opcode, gas_before.saturating_sub(interpreter.gas.remaining()))
            };

            if let Err(ZkGasOutcome::LimitExceeded) = charge {
                halt_for_zk_gas_limit(context, interpreter);
                break;
            }
        }

        if let (Some(step), Some(accumulator), Some(hooks)) =
            (pricing_step, pricing.as_deref_mut(), pricing_hooks) &&
            let Err(error) = (hooks.finish)(step, interpreter, context, accumulator)
        {
            accumulator.retain_diagnostic(Some(error));
        }

        if step_result.is_err() {
            break;
        }
    }

    interpreter.take_next_action()
}

/// Replaces any pending interpreter action with the dedicated zk-gas fatal halt.
///
/// Spawn opcodes install `NewFrame` before the wrapper charge runs. Consuming that action first is
/// required in debug builds (where replacing an action directly is rejected) and prevents the
/// child frame from being dispatched after the wrapper charge exceeds the block budget.
pub(crate) fn halt_for_zk_gas_limit<CTX: ContextTr>(
    context: &mut CTX,
    interpreter: &mut Interpreter<EthInterpreter>,
) {
    set_custom_error(context);
    if interpreter.bytecode.action().is_some() {
        let _ = interpreter.take_next_action();
    }
    interpreter.halt_fatal();
}

/// Sets the dedicated custom zk gas limit error on the EVM context when none is present yet.
pub(crate) fn set_custom_error<CTX: ContextTr>(context: &mut CTX) {
    set_custom_error_message(context, ZK_GAS_LIMIT_ERR);
}

/// Sets a custom context error only when execution has not already recorded one.
pub(crate) fn set_custom_error_message<CTX: ContextTr>(
    context: &mut CTX,
    message: impl Into<String>,
) {
    let err_slot = context.error();
    if err_slot.is_ok() {
        *err_slot = Err(ContextError::Custom(message.into()));
    }
}
