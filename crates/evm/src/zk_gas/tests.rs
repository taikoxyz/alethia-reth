//! Tests for fork-scoped zk gas schedule selection.

use alloy_evm::{Evm as AlloyEvm, EvmEnv, EvmFactory};
use alloy_primitives::{Address, address};
use reth_revm::{
    Inspector,
    context::TxEnv,
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

use crate::{factory::TaikoEvmFactory, spec::TaikoSpecId};

use super::{
    adapter::ZK_GAS_LIMIT_ERR,
    meter::{ZkGasMeter, ZkGasOutcome},
    schedule::{FAILSAFE_MULTIPLIER, TAIKO_MASAYA_CHAIN_ID, schedule_for},
    unzen::{
        MASAYA_BLOCK_ZK_GAS_LIMIT, MASAYA_TX_INTRINSIC_ZK_GAS, MASAYA_UNZEN_ZK_GAS_SCHEDULE,
        TX_INTRINSIC_ZK_GAS, UNZEN_ZK_GAS_SCHEDULE,
    },
};

#[test]
fn unzen_schedule_is_selected_only_for_unzen() {
    assert!(schedule_for(TaikoSpecId::UNZEN, 167).is_some());
    assert!(schedule_for(TaikoSpecId::SHASTA, 167).is_none());
}

#[test]
fn schedule_for_returns_masaya_schedule_on_masaya_chain_id() {
    let schedule = schedule_for(TaikoSpecId::UNZEN, TAIKO_MASAYA_CHAIN_ID).expect("Unzen schedule");
    assert_eq!(schedule.block_limit, 1_000_000_000);
    assert!(std::ptr::eq(schedule, &MASAYA_UNZEN_ZK_GAS_SCHEDULE));
}

#[test]
fn schedule_for_returns_default_schedule_on_non_masaya_chain_ids() {
    for chain_id in [1u64, 167u64, 167_010u64, 167_009u64] {
        let schedule = schedule_for(TaikoSpecId::UNZEN, chain_id).expect("Unzen schedule");
        assert_eq!(schedule.block_limit, 100_000_000);
        assert!(std::ptr::eq(schedule, &UNZEN_ZK_GAS_SCHEDULE));
    }
}

#[test]
fn schedule_for_returns_none_for_pre_unzen_regardless_of_chain_id() {
    assert!(schedule_for(TaikoSpecId::SHASTA, TAIKO_MASAYA_CHAIN_ID).is_none());
    assert!(schedule_for(TaikoSpecId::SHASTA, 167).is_none());
}

#[test]
fn unzen_schedule_uses_the_spec_block_limit() {
    let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");
    assert_eq!(schedule.block_limit, 100_000_000);
}

#[test]
fn unzen_schedule_uses_spec_opcode_and_precompile_multipliers() {
    let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");

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
    let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");

    assert_eq!(schedule.spawn_estimates.call, 12_500);
    assert_eq!(schedule.spawn_estimates.callcode, 12_500);
    assert_eq!(schedule.spawn_estimates.delegatecall, 3_500);
    assert_eq!(schedule.spawn_estimates.staticcall, 3_500);
    assert_eq!(schedule.spawn_estimates.create, 37_000);
    assert_eq!(schedule.spawn_estimates.create2, 44_500);
}

#[test]
fn masaya_unzen_schedule_uses_one_billion_block_limit() {
    assert_eq!(MASAYA_UNZEN_ZK_GAS_SCHEDULE.block_limit, 1_000_000_000);
    assert_eq!(MASAYA_BLOCK_ZK_GAS_LIMIT, 1_000_000_000);
}

#[test]
fn unzen_schedule_pins_default_tx_intrinsic_zk_gas_at_243_000() {
    assert_eq!(UNZEN_ZK_GAS_SCHEDULE.tx_intrinsic_zk_gas, 243_000);
    assert_eq!(TX_INTRINSIC_ZK_GAS, 243_000);
}

#[test]
fn masaya_unzen_schedule_pins_tx_intrinsic_zk_gas_at_zero() {
    assert_eq!(MASAYA_UNZEN_ZK_GAS_SCHEDULE.tx_intrinsic_zk_gas, 0);
    assert_eq!(MASAYA_TX_INTRINSIC_ZK_GAS, 0);
}

#[test]
fn masaya_unzen_schedule_freezes_pre_recalibration_multipliers() {
    // The recalibration changes the default tables but Masaya stays frozen, so they must now
    // differ.
    assert_ne!(
        UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers,
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers
    );
    assert_ne!(
        UNZEN_ZK_GAS_SCHEDULE.precompile_multipliers,
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.precompile_multipliers
    );
    // Spot-check a known recalibrated entry so this guard fails if the two tables ever realign:
    // keccak256 went 85 -> 31 for the default schedule while Masaya stays at 85, and modexp went
    // 1363 -> 154 while Masaya stays at 1363.
    assert_ne!(
        UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers[0x20],
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers[0x20]
    );
    assert_ne!(
        UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&Address::with_last_byte(0x05)),
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&Address::with_last_byte(0x05))
    );
    // Spawn estimates were not recalibrated, so they remain identical across networks.
    assert_eq!(UNZEN_ZK_GAS_SCHEDULE.spawn_estimates, MASAYA_UNZEN_ZK_GAS_SCHEDULE.spawn_estimates);
}

#[test]
fn masaya_unzen_schedule_pins_spec_opcode_and_precompile_multipliers() {
    assert_eq!(MASAYA_UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers[0x20], 85);
    assert_eq!(MASAYA_UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers[0xf1], 25);
    assert_eq!(MASAYA_UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers[0xfe], 0);
    assert_eq!(MASAYA_UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers[0xac], u16::MAX);

    assert_eq!(
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&Address::with_last_byte(0x05)),
        1363
    );
    assert_eq!(
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&Address::with_last_byte(0x01)),
        81
    );
    assert_eq!(
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&Address::with_last_byte(0x04)),
        2
    );
    assert_eq!(
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&Address::with_last_byte(0x14)),
        u16::MAX
    );
}

#[test]
fn meter_promotes_committed_tx_usage_into_block_usage() {
    let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    meter.charge_opcode(0x01, 3).expect("charge");
    meter.commit_transaction().expect("commit");

    assert_eq!(meter.block_zk_gas_used(), 3 * u64::from(schedule.opcode_multipliers[0x01]));
}

#[test]
fn meter_charge_tx_intrinsic_adds_schedule_value_to_in_flight_tx() {
    let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    meter.charge_tx_intrinsic().expect("intrinsic should fit");
    assert_eq!(meter.tx_zk_gas_used(), schedule.tx_intrinsic_zk_gas);
    assert_eq!(meter.block_zk_gas_used(), 0);

    meter.commit_transaction().expect("commit");
    assert_eq!(meter.block_zk_gas_used(), schedule.tx_intrinsic_zk_gas);
}

#[test]
fn meter_charge_tx_intrinsic_is_noop_on_masaya_schedule() {
    let schedule =
        schedule_for(TaikoSpecId::UNZEN, TAIKO_MASAYA_CHAIN_ID).expect("Masaya Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    meter.charge_tx_intrinsic().expect("zero charge always fits");
    assert_eq!(meter.tx_zk_gas_used(), 0);
}

#[test]
fn meter_charge_tx_intrinsic_returns_limit_exceeded_when_remaining_budget_is_too_small() {
    let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");
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
    let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);
    let raw_gas = (u64::MAX / u64::from(schedule.opcode_multipliers[0x01])) + 1;

    assert!(matches!(meter.charge_opcode(0x01, raw_gas), Err(ZkGasOutcome::LimitExceeded)));
}

#[test]
fn meter_treats_precompile_multiplication_overflow_as_limit_exceeded() {
    let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");
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
    let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    meter.charge_opcode(0x01, 2).expect("charge");
    meter.reset_transaction();

    assert_eq!(meter.tx_zk_gas_used(), 0);
    assert_eq!(meter.block_zk_gas_used(), 0);
    meter.charge_opcode(0xf0, schedule.block_limit).expect("reset should restore full budget");
}

#[test]
fn meter_allows_exactly_remaining_block_budget() {
    let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");
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
    let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    meter.charge_opcode(0xf0, schedule.block_limit - 1).expect("prefill");
    meter.commit_transaction().expect("commit");

    assert!(matches!(meter.charge_opcode(0xf0, 2), Err(ZkGasOutcome::LimitExceeded)));
}

#[test]
fn meter_returns_limit_exceeded_for_precompile_over_block_budget() {
    let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    assert!(matches!(
        meter.charge_precompile(&Address::with_last_byte(0x01), schedule.block_limit),
        Err(ZkGasOutcome::LimitExceeded)
    ));
}

#[test]
fn meter_exposes_its_schedule() {
    let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");
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
    let schedule = schedule_for(TaikoSpecId::UNZEN, 167).expect("Unzen schedule");
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
fn factory_installs_masaya_schedule_when_chain_id_is_masaya() {
    let mut env = evm_env(TaikoSpecId::UNZEN);
    env.cfg_env.chain_id = TAIKO_MASAYA_CHAIN_ID;
    let evm = TaikoEvmFactory.create_evm(db_with_contract(limit_exceeding_keccak_bytecode()), env);
    let meter = evm.meter().expect("Masaya schedule should install a meter");

    assert!(std::ptr::eq(meter.schedule(), &MASAYA_UNZEN_ZK_GAS_SCHEDULE));
    assert_eq!(meter.schedule().block_limit, 1_000_000_000);
}

#[test]
fn factory_installs_default_schedule_when_chain_id_is_not_masaya() {
    let env = evm_env(TaikoSpecId::UNZEN); // helper sets chain_id = 167 (not Masaya)
    let evm = TaikoEvmFactory.create_evm(db_with_contract(limit_exceeding_keccak_bytecode()), env);
    let meter = evm.meter().expect("default Unzen schedule should install a meter");

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

    assert_eq!(
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&ecrecover),
        81,
        "canonical ecrecover should keep its frozen Masaya multiplier"
    );
    assert_eq!(
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&collider),
        FAILSAFE_MULTIPLIER,
        "high-range collider should resolve to the fail-safe on Masaya too"
    );

    // A second collider on a different low byte (identity, 0x04) — confirms the fix is not a
    // one-off carve-out for 0x01: every canonical precompile had a potential high-range collider.
    let identity_collider = address!("0x1670000000000000000000000000000000010004");
    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&identity_collider),
        FAILSAFE_MULTIPLIER,
        "identity collider should resolve to the fail-safe"
    );
    assert_eq!(
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&identity_collider),
        FAILSAFE_MULTIPLIER,
        "identity collider should resolve to the fail-safe on Masaya"
    );
}

#[test]
fn full_address_lookup_preserves_canonical_precompile_multipliers() {
    // Masaya stays byte-identical: every listed precompile keeps its frozen multiplier, so its
    // finalized blocks' zk-gas total (committed to the header `difficulty` field) is preserved.
    // On the default schedule the non-BLS precompiles (0x01..=0x0a) keep their pre-fix multipliers;
    // the BLS12 precompiles keep the same values but now sit at their canonical Osaka addresses
    // (0x0b..=0x11), so the stale draft keys 0x12/0x13 fall back to the fail-safe.
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

    let masaya_expected: [(u8, u16); 17] = [
        (0x01, 81),
        (0x02, 10),
        (0x03, 3),
        (0x04, 2),
        (0x05, 1363),
        (0x06, 38),
        (0x07, 87),
        (0x08, 82),
        (0x09, 243),
        (0x0a, 398),
        (0x0b, 112),
        (0x0c, 52),
        (0x0e, 111),
        (0x0f, 39),
        (0x11, 134),
        (0x12, 159),
        (0x13, 112),
    ];
    for (byte, multiplier) in masaya_expected {
        assert_eq!(
            MASAYA_UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&Address::with_last_byte(byte)),
            multiplier,
            "masaya precompile {byte:#04x}"
        );
    }

    // Default schedule: the obsolete draft keys 0x12/0x13 are no longer listed — the spec moved
    // the BLS12 precompiles down to their canonical Osaka addresses — and out-of-range bytes fall
    // back to the fail-safe.
    for byte in [0x12_u8, 0x13, 0x14] {
        assert_eq!(
            UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&Address::with_last_byte(byte)),
            FAILSAFE_MULTIPLIER,
            "default: unlisted byte {byte:#04x}"
        );
    }

    // Masaya stays frozen on the draft addresses, so the canonical-only bytes 0x0d/0x10 it never
    // adopted, plus out-of-range bytes, fall back to the fail-safe.
    for byte in [0x0d_u8, 0x10, 0x14] {
        assert_eq!(
            MASAYA_UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&Address::with_last_byte(byte)),
            FAILSAFE_MULTIPLIER,
            "masaya: unlisted byte {byte:#04x}"
        );
    }
}

#[test]
fn precompile_tables_have_no_duplicate_addresses() {
    for (label, table) in [
        ("default", UNZEN_ZK_GAS_SCHEDULE.precompile_multipliers),
        ("masaya", MASAYA_UNZEN_ZK_GAS_SCHEDULE.precompile_multipliers),
    ] {
        let mut seen = std::collections::HashSet::new();
        for (addr, _) in table {
            assert!(seen.insert(addr), "{label} precompile table has duplicate address {addr}");
        }
    }
}

#[test]
fn unzen_schedule_meters_p256verify_and_freezes_masaya() {
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

    // Masaya stays frozen: its finalized blocks committed their zk-gas total to the
    // header `difficulty` field, so p256verify must keep resolving to the fail-safe
    // there rather than adopting 163 retroactively.
    assert_eq!(
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&p256verify),
        FAILSAFE_MULTIPLIER,
        "Masaya schedule must keep p256verify frozen at the fail-safe multiplier"
    );
}

#[test]
fn unzen_schedule_meters_clz_and_freezes_masaya() {
    // CLZ (EIP-7939) is added in Osaka at opcode 0x1e, which Unzen maps to. Without an explicit
    // entry the fail-safe multiplier (u16::MAX) would brick any block using the opcode, so the
    // default schedule must meter it at the spec value.
    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers[0x1e], 14,
        "default Unzen schedule must meter clz at 14"
    );

    // Masaya stays frozen: its finalized blocks committed their zk-gas total to the header
    // `difficulty` field, so clz must keep resolving to the fail-safe there rather than adopting
    // 14 retroactively.
    assert_eq!(
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers[0x1e], FAILSAFE_MULTIPLIER,
        "Masaya schedule must keep clz frozen at the fail-safe multiplier"
    );
}
