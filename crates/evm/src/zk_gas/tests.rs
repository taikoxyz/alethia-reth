//! Tests for fork-scoped zk gas schedule selection.

use alloy_evm::{Evm as AlloyEvm, EvmEnv, EvmFactory};
use alloy_primitives::{Address, address};
use reth_revm::{
    Inspector,
    context::{
        TxEnv,
        result::{ExecutionResult, HaltReason},
    },
    db::InMemoryDB,
    inspector::NoOpInspector,
    interpreter::{
        CallInputs, CallOutcome, Interpreter, interpreter::EthInterpreter, interpreter_types::Jumps,
    },
    primitives::{Bytes, TxKind},
    state::{AccountInfo, Bytecode, bytecode::opcode},
};
use revm_database_interface::{
    BENCH_CALLER, BENCH_CALLER_BALANCE, BENCH_TARGET, BENCH_TARGET_BALANCE,
};

use crate::{alloy::TaikoZkGasEvm, factory::TaikoEvmFactory, spec::TaikoSpecId};

use super::{
    adapter::ZK_GAS_LIMIT_ERR,
    meter::{ZkGasMeter, ZkGasOutcome},
    schedule::{FAILSAFE_MULTIPLIER, ZkGasSchedule, schedule_for},
    unzen::{TX_INTRINSIC_ZK_GAS, UNZEN_ZK_GAS_SCHEDULE},
};

#[test]
fn unzen_schedule_is_selected_only_for_unzen() {
    assert!(std::ptr::eq(
        schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule"),
        &UNZEN_ZK_GAS_SCHEDULE
    ));
    assert!(schedule_for(TaikoSpecId::SHASTA).is_none());
}

#[test]
fn unzen_schedule_uses_the_spec_block_limit() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    assert_eq!(schedule.block_limit, 100_000_000);
}

#[test]
fn unzen_schedule_uses_spec_opcode_and_precompile_multipliers() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");

    assert_eq!(schedule.opcode_multipliers[0x20], 31); // keccak256
    assert_eq!(schedule.opcode_multipliers[0xf1], 20); // call
    assert_eq!(schedule.opcode_multipliers[0xfe], 0); // invalid (terminal)
    assert_eq!(schedule.opcode_multipliers[0xac], u16::MAX); // unlisted -> failsafe

    assert_eq!(schedule.precompile_multiplier(&Address::with_last_byte(0x05)), 154); // modexp
    assert_eq!(schedule.precompile_multiplier(&Address::with_last_byte(0x01)), 47); // ecrecover
    assert_eq!(schedule.precompile_multiplier(&Address::with_last_byte(0x04)), 6); // identity
    assert_eq!(schedule.precompile_multiplier(&Address::with_last_byte(0x14)), u16::MAX); // failsafe
}

#[test]
fn unzen_schedule_uses_spec_spawn_estimates() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");

    assert_eq!(schedule.spawn_estimates.call, 12_500);
    assert_eq!(schedule.spawn_estimates.callcode, 12_500);
    assert_eq!(schedule.spawn_estimates.delegatecall, 3_500);
    assert_eq!(schedule.spawn_estimates.staticcall, 3_500);
    assert_eq!(schedule.spawn_estimates.create, 37_000);
    assert_eq!(schedule.spawn_estimates.create2, 44_500);
}

#[test]
fn unzen_schedule_pins_default_tx_intrinsic_zk_gas_at_243_000() {
    assert_eq!(UNZEN_ZK_GAS_SCHEDULE.tx_intrinsic_zk_gas, 243_000);
    assert_eq!(TX_INTRINSIC_ZK_GAS, 243_000);
}

#[test]
fn meter_promotes_committed_tx_usage_into_block_usage() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    meter.charge_opcode(0x01, 3).expect("charge");
    meter.commit_transaction().expect("commit");

    assert_eq!(meter.block_zk_gas_used(), 3 * u64::from(schedule.opcode_multipliers[0x01]));
}

#[test]
fn meter_reserves_finalized_block_budget_without_touching_in_flight_usage() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);
    meter.charge_opcode(0xf0, 7).expect("in-flight charge");

    meter.reserve_block_budget(2_000_000).expect("reserve");

    assert_eq!(meter.block_zk_gas_used(), 2_000_000);
    assert_eq!(meter.tx_zk_gas_used(), 7);
}

#[test]
fn meter_rejects_reserve_past_remaining_budget_without_mutation() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);
    meter.reserve_block_budget(schedule.block_limit - 1).expect("initial reserve");

    assert_eq!(meter.reserve_block_budget(2), Err(ZkGasOutcome::LimitExceeded));
    assert_eq!(meter.block_zk_gas_used(), schedule.block_limit - 1);
}

#[test]
fn meter_charge_tx_intrinsic_adds_schedule_value_to_in_flight_tx() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    meter.charge_tx_intrinsic().expect("intrinsic should fit");
    assert_eq!(meter.tx_zk_gas_used(), schedule.tx_intrinsic_zk_gas);
    assert_eq!(meter.block_zk_gas_used(), 0);

    meter.commit_transaction().expect("commit");
    assert_eq!(meter.block_zk_gas_used(), schedule.tx_intrinsic_zk_gas);
}

#[test]
fn meter_charge_tx_intrinsic_returns_limit_exceeded_when_remaining_budget_is_too_small() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    // Fill the block budget to within (intrinsic - 1) of the limit so the next intrinsic
    // charge alone would bust it. CREATE has a multiplier of 1, which makes the arithmetic
    // trivial.
    let prefill = schedule.block_limit - schedule.tx_intrinsic_zk_gas + 1;
    assert_eq!(u64::from(schedule.opcode_multipliers[0xf0]), 1);
    meter.charge_opcode(0xf0, prefill).expect("prefill");
    meter.commit_transaction().expect("commit prefill");

    assert!(matches!(meter.charge_tx_intrinsic(), Err(ZkGasOutcome::LimitExceeded)));
}

#[test]
fn meter_treats_opcode_multiplication_overflow_as_limit_exceeded() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);
    let raw_gas = (u64::MAX / u64::from(schedule.opcode_multipliers[0x01])) + 1;

    assert!(matches!(meter.charge_opcode(0x01, raw_gas), Err(ZkGasOutcome::LimitExceeded)));
}

#[test]
fn meter_treats_precompile_multiplication_overflow_as_limit_exceeded() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);
    let raw_gas =
        (u64::MAX / u64::from(schedule.precompile_multiplier(&Address::with_last_byte(0x01)))) + 1;

    assert!(matches!(
        meter.charge_precompile(&Address::with_last_byte(0x01), raw_gas),
        Err(ZkGasOutcome::LimitExceeded)
    ));
}

#[test]
fn meter_resets_transaction_usage_without_affecting_block_usage() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    meter.charge_opcode(0x01, 2).expect("charge");
    meter.reset_transaction();

    assert_eq!(meter.tx_zk_gas_used(), 0);
    assert_eq!(meter.block_zk_gas_used(), 0);
    meter.charge_opcode(0xf0, schedule.block_limit).expect("reset should restore full budget");
}

#[test]
fn meter_allows_exactly_remaining_block_budget() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    meter.charge_opcode(0xf0, schedule.block_limit - 1).expect("prefill");
    meter.commit_transaction().expect("commit");

    assert_eq!(meter.block_zk_gas_used(), schedule.block_limit - 1);

    meter.charge_opcode(0xf0, 1).expect("remaining budget");
    meter.commit_transaction().expect("commit");

    assert_eq!(meter.block_zk_gas_used(), schedule.block_limit);
}

#[test]
fn meter_rejects_block_budget_plus_one() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    meter.charge_opcode(0xf0, schedule.block_limit - 1).expect("prefill");
    meter.commit_transaction().expect("commit");

    assert!(matches!(meter.charge_opcode(0xf0, 2), Err(ZkGasOutcome::LimitExceeded)));
}

#[test]
fn meter_commit_would_exceed_agrees_with_commit_transaction_at_boundaries() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");

    // In-flight usage that lands exactly on the block limit commits.
    let mut at_limit = ZkGasMeter::with_usage_for_tests(schedule, schedule.block_limit - 1, 1);
    assert!(!at_limit.commit_would_exceed_block_limit());
    assert!(at_limit.commit_transaction().is_ok());
    assert_eq!(at_limit.block_zk_gas_used(), schedule.block_limit);

    // One zk gas past the limit is rejected, and the predicate agrees before the commit runs.
    let mut past_limit = ZkGasMeter::with_usage_for_tests(schedule, schedule.block_limit - 1, 2);
    assert!(past_limit.commit_would_exceed_block_limit());
    assert!(matches!(past_limit.commit_transaction(), Err(ZkGasOutcome::LimitExceeded)));

    // A `u64` overflow in the running block total also counts as exceeding the limit.
    let overflow_schedule = ZkGasSchedule { block_limit: u64::MAX, ..UNZEN_ZK_GAS_SCHEDULE };
    let mut overflowing = ZkGasMeter::with_usage_for_tests(&overflow_schedule, u64::MAX, 1);
    assert!(overflowing.commit_would_exceed_block_limit());
    assert!(matches!(overflowing.commit_transaction(), Err(ZkGasOutcome::LimitExceeded)));
}

#[test]
fn meter_returns_limit_exceeded_for_precompile_over_block_budget() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    assert!(matches!(
        meter.charge_precompile(&Address::with_last_byte(0x01), schedule.block_limit),
        Err(ZkGasOutcome::LimitExceeded)
    ));
}

#[test]
fn meter_exposes_its_schedule() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let meter = ZkGasMeter::new(schedule);

    assert!(std::ptr::eq(meter.schedule(), schedule));
}

#[derive(Default, Debug)]
struct StepGasProbeInspector {
    gas_remaining: u64,
    step_costs: Vec<(u8, u64)>,
    precompile_gas_used: Option<u64>,
}

impl<CTX> Inspector<CTX, EthInterpreter> for StepGasProbeInspector {
    fn initialize_interp(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        self.gas_remaining = interp.gas.limit();
    }

    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        self.gas_remaining = interp.gas.remaining();
        self.step_costs.push((interp.bytecode.opcode(), 0));
    }

    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        let remaining = interp.gas.remaining();
        let last = self.step_costs.last_mut().expect("step recorded");
        last.1 = self.gas_remaining.saturating_sub(remaining);
        self.gas_remaining = remaining;
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if outcome.was_precompile_called {
            self.precompile_gas_used =
                Some(inputs.gas_limit.saturating_sub(outcome.result.gas.remaining()));
        }
    }
}

#[test]
fn unzen_adapter_uses_spawn_estimate_for_precompile_dispatch() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let mut evm = TaikoEvmFactory.create_evm_with_inspector(
        db_with_contract(staticcall_identity_bytecode()),
        evm_env(TaikoSpecId::UNZEN),
        StepGasProbeInspector::default(),
    );

    evm.transact(tx_env(100_000)).expect("Unzen tx should execute");

    let meter = evm.meter().expect("Unzen should install a meter");
    let probe = evm.inspector();
    let precompile_gas_used = probe.precompile_gas_used.expect("precompile gas recorded");

    let expected = probe.step_costs.iter().fold(
        u64::from(schedule.precompile_multiplier(&Address::with_last_byte(0x04))) *
            precompile_gas_used,
        |acc, (opcode, step_gas)| {
            let raw_gas = if *opcode == opcode::STATICCALL {
                schedule.spawn_estimates.staticcall
            } else {
                *step_gas
            };
            acc + raw_gas * u64::from(schedule.opcode_multipliers[*opcode as usize])
        },
    );
    let naive_expected = probe.step_costs.iter().fold(
        u64::from(schedule.precompile_multiplier(&Address::with_last_byte(0x04))) *
            precompile_gas_used,
        |acc, (opcode, step_gas)| {
            acc + (*step_gas * u64::from(schedule.opcode_multipliers[*opcode as usize]))
        },
    );

    assert_eq!(meter.tx_zk_gas_used(), expected);
    assert_ne!(meter.tx_zk_gas_used(), naive_expected);
}

#[test]
fn production_metered_path_matches_inspector_path_for_precompile_dispatch() {
    let mut production_evm = TaikoEvmFactory
        .create_evm(db_with_contract(staticcall_identity_bytecode()), evm_env(TaikoSpecId::UNZEN));
    production_evm.transact(tx_env(100_000)).expect("production path should execute");
    let production_zk_gas =
        production_evm.meter().expect("production path should install a meter").tx_zk_gas_used();

    let mut inspector_evm = TaikoEvmFactory.create_evm_with_inspector(
        db_with_contract(staticcall_identity_bytecode()),
        evm_env(TaikoSpecId::UNZEN),
        NoOpInspector {},
    );
    inspector_evm.transact(tx_env(100_000)).expect("inspector path should execute");
    let inspector_zk_gas =
        inspector_evm.meter().expect("inspector path should install a meter").tx_zk_gas_used();

    assert_eq!(production_zk_gas, inspector_zk_gas);
}

#[test]
fn production_metered_path_matches_inspector_path_for_ordinary_opcodes() {
    let mut production_evm = TaikoEvmFactory
        .create_evm(db_with_contract(simple_arithmetic_bytecode()), evm_env(TaikoSpecId::UNZEN));
    production_evm.transact(tx_env(100_000)).expect("production path should execute");
    let production_zk_gas =
        production_evm.meter().expect("production path should install a meter").tx_zk_gas_used();

    let mut inspector_evm = TaikoEvmFactory.create_evm_with_inspector(
        db_with_contract(simple_arithmetic_bytecode()),
        evm_env(TaikoSpecId::UNZEN),
        NoOpInspector {},
    );
    inspector_evm.transact(tx_env(100_000)).expect("inspector path should execute");
    let inspector_zk_gas =
        inspector_evm.meter().expect("inspector path should install a meter").tx_zk_gas_used();

    assert_eq!(production_zk_gas, inspector_zk_gas);
}

#[test]
fn transact_meters_each_run_from_zero_on_a_reused_evm() {
    // RPC helpers like `eth_estimateGas` build one EVM and re-run the same transaction many
    // times (initial run, optimistic run, binary search). Every run must meter against a fresh
    // transaction budget instead of inheriting the in-flight zk gas of the previous runs.
    let mut evm = TaikoEvmFactory
        .create_evm(db_with_contract(simple_arithmetic_bytecode()), evm_env(TaikoSpecId::UNZEN));

    evm.transact(tx_env(100_000)).expect("first run should execute");
    let first_run = evm.meter().expect("Unzen should install a meter").tx_zk_gas_used();
    assert!(first_run > 0, "the metered run must charge zk gas");

    evm.transact(tx_env(100_000)).expect("second run should execute");
    let second_run = evm.meter().expect("Unzen should install a meter").tx_zk_gas_used();

    assert_eq!(
        second_run, first_run,
        "a reused EVM must not accumulate in-flight zk gas across transact calls"
    );
}

#[test]
fn inspected_transact_meters_each_run_from_zero_on_a_reused_evm() {
    // Same reused-EVM shape as the production-path test above, on the inspector metering path.
    let mut evm = TaikoEvmFactory.create_evm_with_inspector(
        db_with_contract(simple_arithmetic_bytecode()),
        evm_env(TaikoSpecId::UNZEN),
        NoOpInspector {},
    );

    evm.transact(tx_env(100_000)).expect("first run should execute");
    let first_run = evm.meter().expect("Unzen should install a meter").tx_zk_gas_used();
    assert!(first_run > 0, "the metered run must charge zk gas");

    evm.transact(tx_env(100_000)).expect("second run should execute");
    let second_run = evm.meter().expect("Unzen should install a meter").tx_zk_gas_used();

    assert_eq!(
        second_run, first_run,
        "a reused EVM must not accumulate in-flight zk gas across transact calls"
    );
}

#[test]
fn disabled_per_transact_reset_preserves_executor_accumulation_semantics() {
    // The block executor calls `set_per_transact_zk_gas_reset_enabled(false)` and brackets
    // each transaction itself (reset, intrinsic charge, commit); with the entry reset
    // disabled, in-flight usage must keep accumulating across transact calls.
    let mut evm = TaikoEvmFactory
        .create_evm(db_with_contract(simple_arithmetic_bytecode()), evm_env(TaikoSpecId::UNZEN));
    evm.set_per_transact_zk_gas_reset_enabled(false);

    evm.transact(tx_env(100_000)).expect("first run should execute");
    let first_run = evm.meter().expect("Unzen should install a meter").tx_zk_gas_used();
    assert!(first_run > 0, "the metered run must charge zk gas");

    evm.transact(tx_env(100_000)).expect("second run should execute");
    let second_run = evm.meter().expect("Unzen should install a meter").tx_zk_gas_used();

    assert_eq!(
        second_run,
        first_run * 2,
        "a disabled entry reset must preserve cross-transact accumulation for the executor"
    );
}

#[test]
fn reset_clears_deferred_charges_left_by_a_limit_exceeded_nested_call() {
    // A zk gas limit hit inside a nested call aborts execution while CALL-family steps are
    // still deferred (`flush_deferred_steps` deliberately keeps later entries on error). The
    // per-transact reset must drop that bookkeeping too, or the next transaction on the same
    // EVM starts by flushing the previous transaction's charges into its fresh meter.
    let cheap_target = Address::with_last_byte(0xEE);

    assert_no_deferred_leak_across_transacts(limit_exceeding_keccak_bytecode(), cheap_target);
}

#[test]
fn reset_clears_deferred_charges_when_the_flush_itself_is_over_budget() {
    // Variant where the budget is ground down by ~250k-zk keccak iterations, so the remaining
    // budget at failure time is below one CALL spawn estimate and the unwind-time flush of the
    // deferred CALL charges errors instead of draining them.
    assert_no_deferred_leak_across_transacts(
        quarter_million_keccak_loop_bytecode(),
        Address::with_last_byte(0xEF),
    );
}

/// Runs the shared deferred-leak scenario: a nested call chain to `buster_bytecode` busts the
/// zk gas limit on a reused EVM, then a cheap unrelated transaction must meter exactly like a
/// fresh EVM would — any difference is leftover bookkeeping leaking across transacts.
fn assert_no_deferred_leak_across_transacts(buster_bytecode: Bytecode, cheap_target: Address) {
    let mut evm = TaikoEvmFactory.create_evm_with_inspector(
        nested_limit_db(cheap_target, buster_bytecode.clone()),
        evm_env(TaikoSpecId::UNZEN),
        NoOpInspector {},
    );
    let err = evm.transact(tx_env(16_000_000)).expect_err("nested call must bust the limit");
    assert!(err.to_string().contains(ZK_GAS_LIMIT_ERR), "unexpected error: {err}");

    evm.transact(cheap_tx_env(cheap_target)).expect("cheap tx should execute after the failure");
    let reused = evm.meter().expect("Unzen should install a meter").tx_zk_gas_used();

    let mut fresh_evm = TaikoEvmFactory.create_evm_with_inspector(
        nested_limit_db(cheap_target, buster_bytecode),
        evm_env(TaikoSpecId::UNZEN),
        NoOpInspector {},
    );
    fresh_evm.transact(cheap_tx_env(cheap_target)).expect("cheap tx should execute");
    let fresh = fresh_evm.meter().expect("Unzen should install a meter").tx_zk_gas_used();

    assert_eq!(
        reused, fresh,
        "deferred steps left by a failed transaction must not leak into the next one"
    );
}

#[test]
fn reused_evm_replays_a_heavy_tx_without_tripping_the_block_limit() {
    // Regression for the observed `eth_estimateGas` failure: a transaction whose single run
    // costs ~68M zk gas (under the 100M Unzen block limit) was pushed over the limit by the
    // estimate flow's repeated simulations because the meter carried usage between runs.
    let mut evm = TaikoEvmFactory
        .create_evm(db_with_contract(half_limit_keccak_bytecode()), evm_env(TaikoSpecId::UNZEN));

    for run in 0..4u32 {
        let result = evm
            .transact(tx_env(5_000_000))
            .unwrap_or_else(|err| panic!("run {run} must not hit the zk gas limit: {err}"));
        assert!(result.result.is_success(), "run {run} must succeed: {:?}", result.result);
    }
}

/// Executes `bytecode` on Unzen through the production (no-inspector) and the inspected
/// metered path, asserting both halt with `expected_halt` and returning the per-tx zk gas
/// charged by each.
fn metered_paths_zk_gas_for_halting_tx(
    bytecode: Bytecode,
    gas_limit: u64,
    expected_halt: fn(&HaltReason) -> bool,
) -> (u64, u64) {
    let mut production_evm =
        TaikoEvmFactory.create_evm(db_with_contract(bytecode.clone()), evm_env(TaikoSpecId::UNZEN));
    let result = production_evm.transact(tx_env(gas_limit)).expect("production path executes");
    assert!(
        matches!(&result.result, ExecutionResult::Halt { reason, .. } if expected_halt(reason)),
        "production path halted unexpectedly: {:?}",
        result.result,
    );
    let production_zk_gas =
        production_evm.meter().expect("production path installs a meter").tx_zk_gas_used();

    let mut inspector_evm = TaikoEvmFactory.create_evm_with_inspector(
        db_with_contract(bytecode),
        evm_env(TaikoSpecId::UNZEN),
        NoOpInspector {},
    );
    let result = inspector_evm.transact(tx_env(gas_limit)).expect("inspector path executes");
    assert!(
        matches!(&result.result, ExecutionResult::Halt { reason, .. } if expected_halt(reason)),
        "inspector path halted unexpectedly: {:?}",
        result.result,
    );
    let inspector_zk_gas =
        inspector_evm.meter().expect("inspector path installs a meter").tx_zk_gas_used();

    (production_zk_gas, inspector_zk_gas)
}

#[test]
fn metered_paths_charge_forfeited_gas_for_an_oog_halting_step() {
    // The gas limit admits both `PUSH1` steps (3 gas each) but dies on `ADD`'s table-driven
    // static charge with 1 gas left. An `OutOfGas` halt forfeits all remaining gas
    // (`Interpreter::halt` spends it), and that forfeiture is part of the step's zk gas
    // charge: pre-2.4.0 revm ran `halt_oog()` (spend-all) inside `step` itself, and
    // taiko-geth's zk mirror follows that reference behavior. These committed values feed
    // `header.difficulty` on Unzen, so any drift here is a consensus change.
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let push_multiplier = u64::from(schedule.opcode_multipliers[usize::from(opcode::PUSH1)]);
    let add_multiplier = u64::from(schedule.opcode_multipliers[usize::from(opcode::ADD)]);
    let forfeited_gas = 1;
    let expected = 2 * 3 * push_multiplier + forfeited_gas * add_multiplier;

    let (production_zk_gas, inspector_zk_gas) =
        metered_paths_zk_gas_for_halting_tx(simple_arithmetic_bytecode(), 21_000 + 7, |reason| {
            matches!(reason, HaltReason::OutOfGas(_))
        });

    assert_eq!(
        production_zk_gas, expected,
        "OOG-halting step must charge the gas it forfeited via spend-all"
    );
    assert_eq!(
        inspector_zk_gas, expected,
        "inspector path must charge the OOG-halting step exactly like production"
    );
}

#[test]
fn metered_paths_charge_static_gas_zk_gas_for_a_stack_underflow_step() {
    // `ADD` on an empty stack: the GasTable static charge (3 gas) lands before revm validates
    // the stack, so the halting step contributes `3 x multiplier`. Known cross-client
    // divergence: taiko-geth validates the stack before charging gas and meters 0 for this
    // step; the fix is tracked on the geth side, so this pins reth's committed value.
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let add_multiplier = u64::from(schedule.opcode_multipliers[usize::from(opcode::ADD)]);
    let expected = 3 * add_multiplier;

    let (production_zk_gas, inspector_zk_gas) =
        metered_paths_zk_gas_for_halting_tx(stack_underflow_bytecode(), 100_000, |reason| {
            matches!(reason, HaltReason::StackUnderflow)
        });

    assert_eq!(production_zk_gas, expected, "stack-underflow step must charge its static gas cost");
    assert_eq!(
        inspector_zk_gas, expected,
        "inspector path must charge the underflow-halting step exactly like production"
    );
}

#[test]
fn unzen_adapter_raises_dedicated_error_when_limit_is_exceeded() {
    let mut evm = TaikoEvmFactory.create_evm(
        db_with_contract(limit_exceeding_keccak_bytecode()),
        evm_env(TaikoSpecId::UNZEN),
    );

    let err = evm.transact(tx_env(16_000_000)).expect_err("Unzen tx should abort");

    assert!(matches!(
        err,
        reth_revm::context::result::EVMError::Custom(message)
            if message == ZK_GAS_LIMIT_ERR
    ));
    assert!(evm.meter().is_some());
}

#[test]
fn unzen_default_create_evm_path_is_metered() {
    let mut evm = TaikoEvmFactory.create_evm(
        db_with_contract(limit_exceeding_keccak_bytecode()),
        evm_env(TaikoSpecId::UNZEN),
    );

    assert!(evm.meter().is_some());
    assert!(evm.transact(tx_env(16_000_000)).is_err());
}

#[test]
fn production_metered_path_stays_metered_when_noop_inspector_is_enabled() {
    let mut evm = TaikoEvmFactory.create_evm(
        db_with_contract(limit_exceeding_keccak_bytecode()),
        evm_env(TaikoSpecId::UNZEN),
    );

    evm.enable_inspector();
    let err = evm.transact(tx_env(16_000_000)).expect_err("Unzen tx should stay metered");

    assert!(matches!(
        err,
        reth_revm::context::result::EVMError::Custom(message)
            if message == ZK_GAS_LIMIT_ERR
    ));
}

#[test]
fn factory_installs_unzen_schedule() {
    let env = evm_env(TaikoSpecId::UNZEN);
    let evm = TaikoEvmFactory.create_evm(db_with_contract(limit_exceeding_keccak_bytecode()), env);
    let meter = evm.meter().expect("Unzen schedule should install a meter");

    assert!(std::ptr::eq(meter.schedule(), &UNZEN_ZK_GAS_SCHEDULE));
    assert_eq!(meter.schedule().block_limit, 100_000_000);
}

#[test]
fn taiko_zk_gas_evm_charge_tx_intrinsic_adds_intrinsic_to_in_flight_tx() {
    use crate::alloy::TaikoZkGasEvm;

    let mut evm = TaikoEvmFactory
        .create_evm(db_with_contract(staticcall_identity_bytecode()), evm_env(TaikoSpecId::UNZEN));

    evm.charge_tx_intrinsic_zk_gas().expect("intrinsic should fit");
    let meter = evm.meter().expect("Unzen schedule installs a meter");
    assert_eq!(meter.tx_zk_gas_used(), meter.schedule().tx_intrinsic_zk_gas);
}

#[test]
fn taiko_zk_gas_evm_charge_tx_intrinsic_is_ok_when_metering_is_disabled() {
    use crate::alloy::TaikoZkGasEvm;

    let mut evm = TaikoEvmFactory
        .create_evm(db_with_contract(staticcall_identity_bytecode()), evm_env(TaikoSpecId::SHASTA));

    assert!(evm.meter().is_none());
    evm.charge_tx_intrinsic_zk_gas().expect("disabled metering should be a no-op");
}

#[test]
fn non_unzen_default_create_evm_path_keeps_metering_disabled() {
    let mut evm = TaikoEvmFactory
        .create_evm(db_with_contract(simple_arithmetic_bytecode()), evm_env(TaikoSpecId::SHASTA));

    assert!(evm.meter().is_none());
    evm.transact(tx_env(5_000_000)).expect("non-Unzen tx should stay on the legacy path");
}

fn evm_env(spec: TaikoSpecId) -> EvmEnv<TaikoSpecId> {
    let mut env: EvmEnv<TaikoSpecId> = EvmEnv::default();
    env.cfg_env.spec = spec;
    env.cfg_env.chain_id = 167;
    env.block_env.gas_limit = 30_000_000;
    env
}

fn tx_env(gas_limit: u64) -> TxEnv {
    TxEnv::builder()
        .caller(BENCH_CALLER)
        .kind(TxKind::Call(BENCH_TARGET))
        .chain_id(Some(167))
        .gas_limit(gas_limit)
        .build()
        .unwrap()
}

fn db_with_contract(bytecode: Bytecode) -> InMemoryDB {
    let mut db = InMemoryDB::default();
    let code_hash = bytecode.hash_slow();
    db.insert_account_info(
        BENCH_TARGET,
        AccountInfo {
            nonce: 1,
            balance: BENCH_TARGET_BALANCE,
            code_hash,
            code: Some(bytecode),
            ..Default::default()
        },
    );
    db.insert_account_info(
        BENCH_CALLER,
        AccountInfo { nonce: 0, balance: BENCH_CALLER_BALANCE, ..Default::default() },
    );
    db
}

fn staticcall_identity_bytecode() -> Bytecode {
    Bytecode::new_raw(Bytes::from(vec![
        opcode::PUSH1,
        0x00,
        opcode::PUSH1,
        0x00,
        opcode::PUSH1,
        0x00,
        opcode::PUSH1,
        0x00,
        opcode::PUSH1,
        0x04,
        opcode::PUSH2,
        0xff,
        0xff,
        opcode::STATICCALL,
        opcode::STOP,
    ]))
}

fn limit_exceeding_keccak_bytecode() -> Bytecode {
    // Hash 0x18_0000 (1.5 MiB) of zero memory from offset 0x20. Sized so the metered KECCAK256
    // cost busts the 100M Unzen block zk gas limit even after the recalibration lowered the
    // keccak256 opcode multiplier (85 -> 31).
    Bytecode::new_raw(Bytes::from(vec![
        opcode::PUSH1,
        0x20,
        opcode::PUSH3,
        0x18,
        0x00,
        0x00,
        opcode::KECCAK256,
        opcode::STOP,
    ]))
}

/// Pushes a zero-arg, zero-value `CALL` to `target` forwarding `gas`, then `STOP`.
fn call_into_bytecode(target: Address, gas: u32) -> Bytecode {
    let mut code = vec![
        opcode::PUSH1,
        0x00, // retLen
        opcode::PUSH1,
        0x00, // retOff
        opcode::PUSH1,
        0x00, // argLen
        opcode::PUSH1,
        0x00, // argOff
        opcode::PUSH1,
        0x00, // value
        opcode::PUSH20,
    ];
    code.extend_from_slice(target.as_slice());
    code.push(opcode::PUSH4);
    code.extend_from_slice(&gas.to_be_bytes());
    code.push(opcode::CALL);
    code.push(opcode::STOP);
    Bytecode::new_raw(Bytes::from(code))
}

/// `BENCH_TARGET` -> middle -> buster call chain, so two CALL-family steps are still deferred
/// when the deepest frame busts the block zk gas limit; `cheap_target` holds an unrelated
/// cheap contract for the follow-up transaction.
fn nested_limit_db(cheap_target: Address, buster_bytecode: Bytecode) -> InMemoryDB {
    let middle = Address::with_last_byte(0xCA);
    let buster = Address::with_last_byte(0xDB);
    let mut db = db_with_contract(call_into_bytecode(middle, 15_000_000));
    insert_contract(&mut db, middle, call_into_bytecode(buster, 14_000_000));
    insert_contract(&mut db, buster, buster_bytecode);
    insert_contract(&mut db, cheap_target, simple_arithmetic_bytecode());
    db
}

/// Loops `KECCAK256` over a fixed ~42 KiB span so every iteration charges just under 250k zk
/// gas. When the budget dies, the remaining zk gas is below one CALL spawn estimate (250k), so
/// the deferred CALL charges cannot be flushed during the failure unwind. The zk budget is
/// exhausted long before the forwarded EVM gas.
fn quarter_million_keccak_loop_bytecode() -> Bytecode {
    Bytecode::new_raw(Bytes::from(vec![
        opcode::PUSH2,
        0xA7,
        0x60, // size: 42,848 bytes
        opcode::PUSH1,
        0x00,             // offset
        opcode::JUMPDEST, // pc = 5
        opcode::KECCAK256,
        opcode::POP,
        opcode::PUSH2,
        0xA7,
        0x60,
        opcode::PUSH1,
        0x00,
        opcode::PUSH1,
        0x05,
        opcode::JUMP,
    ]))
}

fn insert_contract(db: &mut InMemoryDB, address: Address, bytecode: Bytecode) {
    let code_hash = bytecode.hash_slow();
    db.insert_account_info(
        address,
        AccountInfo { nonce: 1, code_hash, code: Some(bytecode), ..Default::default() },
    );
}

/// A 100k-gas call from `BENCH_CALLER` to `target`.
fn cheap_tx_env(target: Address) -> TxEnv {
    TxEnv::builder()
        .caller(BENCH_CALLER)
        .kind(TxKind::Call(target))
        .chain_id(Some(167))
        .gas_limit(100_000)
        .build()
        .unwrap()
}

fn half_limit_keccak_bytecode() -> Bytecode {
    // Same shape as `limit_exceeding_keccak_bytecode`, sized down so the memory expansion stops
    // near 1 MiB: one metered run costs ~68M zk gas — under the 100M Unzen block limit — while
    // two accumulated runs would bust it.
    Bytecode::new_raw(Bytes::from(vec![
        opcode::PUSH1,
        0x20,
        opcode::PUSH3,
        0x10,
        0x00,
        0x00,
        opcode::KECCAK256,
        opcode::STOP,
    ]))
}

fn simple_arithmetic_bytecode() -> Bytecode {
    Bytecode::new_raw(Bytes::from(vec![
        opcode::PUSH1,
        0x01,
        opcode::PUSH1,
        0x02,
        opcode::ADD,
        opcode::STOP,
    ]))
}

/// `ADD` with nothing on the stack: halts with a stack underflow after its static gas charge.
fn stack_underflow_bytecode() -> Bytecode {
    Bytecode::new_raw(Bytes::from(vec![opcode::ADD]))
}

#[test]
fn high_range_precompile_collision_resolves_to_failsafe_not_canonical() {
    // An `L1Sload`-style address whose low byte (0x01) collides with `ecrecover` but whose upper
    // bytes differ. Under the old low-byte keying this was charged `ecrecover`'s multiplier; with
    // full-address keying it must fall back to the fail-safe, since no such precompile exists in
    // the Unzen fork.
    let collider = address!("0x1670000000000000000000000000000000010001");
    let ecrecover = Address::with_last_byte(0x01);

    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&ecrecover),
        47,
        "canonical ecrecover should keep its multiplier"
    );
    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&collider),
        FAILSAFE_MULTIPLIER,
        "high-range collider should resolve to the fail-safe, not ecrecover's multiplier"
    );

    // A second collider on a different low byte (identity, 0x04) — confirms the fix is not a
    // one-off carve-out for 0x01: every canonical precompile had a potential high-range collider.
    let identity_collider = address!("0x1670000000000000000000000000000000010004");
    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&identity_collider),
        FAILSAFE_MULTIPLIER,
        "identity collider should resolve to the fail-safe"
    );
}

#[test]
fn full_address_lookup_preserves_canonical_precompile_multipliers() {
    // The non-BLS precompiles (0x01..=0x0a) keep their pre-fix multipliers; the BLS12 precompiles
    // keep the same values but now sit at their canonical Osaka addresses (0x0b..=0x11), so the
    // stale draft keys 0x12/0x13 fall back to the fail-safe.
    let default_expected: [(u8, u16); 17] = [
        (0x01, 47),
        (0x02, 10),
        (0x03, 4),
        (0x04, 6),
        (0x05, 154),
        (0x06, 19),
        (0x07, 58),
        (0x08, 54),
        (0x09, 166),
        (0x0a, 859),
        (0x0b, 201),
        (0x0c, 93),
        (0x0d, 230),
        (0x0e, 71),
        (0x0f, 365),
        (0x10, 246),
        (0x11, 208),
    ];
    for (byte, multiplier) in default_expected {
        assert_eq!(
            UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&Address::with_last_byte(byte)),
            multiplier,
            "default precompile {byte:#04x}"
        );
    }

    // The obsolete draft keys 0x12/0x13 are no longer listed — the spec moved the BLS12
    // precompiles down to their canonical Osaka addresses — and out-of-range bytes fall back to
    // the fail-safe.
    for byte in [0x12_u8, 0x13, 0x14] {
        assert_eq!(
            UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&Address::with_last_byte(byte)),
            FAILSAFE_MULTIPLIER,
            "default: unlisted byte {byte:#04x}"
        );
    }
}

#[test]
fn precompile_tables_have_no_duplicate_addresses() {
    let mut seen = std::collections::HashSet::new();
    for (addr, _) in UNZEN_ZK_GAS_SCHEDULE.precompile_multipliers {
        assert!(seen.insert(addr), "default precompile table has duplicate address {addr}");
    }
}

#[test]
fn unzen_schedule_meters_p256verify() {
    // p256verify (RIP-7212) lives at 0x0000…0100 — outside the canonical 0x..XX
    // precompile range. revm's Osaka set (which Unzen maps to) makes it active, so
    // it must carry a real multiplier on the default schedule instead of the
    // fail-safe that would truncate any calling transaction.
    let p256verify = address!("0x0000000000000000000000000000000000000100");

    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&p256verify),
        163,
        "default Unzen schedule must meter p256verify at 163"
    );
}

#[test]
fn unzen_schedule_meters_clz() {
    // CLZ (EIP-7939) is added in Osaka at opcode 0x1e, which Unzen maps to. Without an explicit
    // entry the fail-safe multiplier (u16::MAX) would brick any block using the opcode, so the
    // default schedule must meter it at the spec value.
    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers[0x1e], 14,
        "default Unzen schedule must meter clz at 14"
    );
}
