//! Tests for fork-scoped zk gas schedule selection.

use alloy_evm::{Evm as AlloyEvm, EvmEnv, EvmFactory};
use reth_revm::{
    Inspector,
    context::TxEnv,
    db::InMemoryDB,
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
    schedule::schedule_for,
};

#[test]
fn uzen_schedule_is_selected_only_for_uzen() {
    assert!(schedule_for(TaikoSpecId::UZEN).is_some());
    assert!(schedule_for(TaikoSpecId::SHASTA).is_none());
}

#[test]
fn uzen_schedule_uses_the_spec_block_limit() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
    assert_eq!(schedule.block_limit, 100_000_000);
}

#[test]
fn uzen_schedule_uses_spec_opcode_and_precompile_multipliers() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");

    assert_eq!(schedule.opcode_multipliers[0x20], 85);
    assert_eq!(schedule.opcode_multipliers[0xf1], 25);
    assert_eq!(schedule.opcode_multipliers[0xfe], 0);
    assert_eq!(schedule.opcode_multipliers[0xac], u16::MAX);

    assert_eq!(schedule.precompile_multipliers[0x05], 1363);
    assert_eq!(schedule.precompile_multipliers[0x01], 81);
    assert_eq!(schedule.precompile_multipliers[0x04], 2);
    assert_eq!(schedule.precompile_multipliers[0x14], u16::MAX);
}

#[test]
fn uzen_schedule_uses_spec_spawn_estimates() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");

    assert_eq!(schedule.spawn_estimates.call, 12_500);
    assert_eq!(schedule.spawn_estimates.callcode, 12_500);
    assert_eq!(schedule.spawn_estimates.delegatecall, 3_500);
    assert_eq!(schedule.spawn_estimates.staticcall, 3_500);
    assert_eq!(schedule.spawn_estimates.create, 37_000);
    assert_eq!(schedule.spawn_estimates.create2, 44_500);
}

#[test]
fn meter_promotes_committed_tx_usage_into_block_usage() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    meter.charge_opcode(0x01, 3).expect("charge");
    meter.commit_transaction().expect("commit");

    assert_eq!(meter.block_zk_gas_used(), 3 * u64::from(schedule.opcode_multipliers[0x01]));
}

#[test]
fn meter_treats_opcode_multiplication_overflow_as_limit_exceeded() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
    let mut meter = ZkGasMeter::new(schedule);
    let raw_gas = (u64::MAX / u64::from(schedule.opcode_multipliers[0x01])) + 1;

    assert!(matches!(meter.charge_opcode(0x01, raw_gas), Err(ZkGasOutcome::LimitExceeded)));
}

#[test]
fn meter_treats_precompile_multiplication_overflow_as_limit_exceeded() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
    let mut meter = ZkGasMeter::new(schedule);
    let raw_gas = (u64::MAX / u64::from(schedule.precompile_multipliers[0x01])) + 1;

    assert!(matches!(meter.charge_precompile(0x01, raw_gas), Err(ZkGasOutcome::LimitExceeded)));
}

#[test]
fn meter_resets_transaction_usage_without_affecting_block_usage() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    meter.charge_opcode(0x01, 2).expect("charge");
    meter.reset_transaction();

    assert_eq!(meter.tx_zk_gas_used(), 0);
    assert_eq!(meter.block_zk_gas_used(), 0);
}

#[test]
fn meter_allows_exactly_remaining_block_budget() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
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
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    meter.charge_opcode(0xf0, schedule.block_limit - 1).expect("prefill");
    meter.commit_transaction().expect("commit");

    assert!(matches!(meter.charge_opcode(0xf0, 2), Err(ZkGasOutcome::LimitExceeded)));
}

#[test]
fn meter_returns_limit_exceeded_for_precompile_over_block_budget() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
    let mut meter = ZkGasMeter::new(schedule);

    assert!(matches!(
        meter.charge_precompile(0x01, schedule.block_limit),
        Err(ZkGasOutcome::LimitExceeded)
    ));
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
fn uzen_adapter_uses_spawn_estimate_for_precompile_dispatch() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
    let mut evm = TaikoEvmFactory.create_evm_with_inspector(
        db_with_contract(staticcall_identity_bytecode()),
        evm_env(TaikoSpecId::UZEN),
        StepGasProbeInspector::default(),
    );

    evm.transact(tx_env(100_000)).expect("Uzen tx should execute");

    let meter = evm.shared_meter().expect("Uzen should install a shared meter");
    let meter = meter.lock().expect("meter lock");
    let probe = evm.inspector();
    let precompile_gas_used = probe.precompile_gas_used.expect("precompile gas recorded");

    let expected = probe.step_costs.iter().fold(
        u64::from(schedule.precompile_multipliers[0x04]) * precompile_gas_used,
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
        u64::from(schedule.precompile_multipliers[0x04]) * precompile_gas_used,
        |acc, (opcode, step_gas)| {
            acc + (*step_gas * u64::from(schedule.opcode_multipliers[*opcode as usize]))
        },
    );

    assert_eq!(meter.tx_zk_gas_used(), expected);
    assert_ne!(meter.tx_zk_gas_used(), naive_expected);
}

#[test]
fn uzen_adapter_raises_dedicated_error_when_limit_is_exceeded() {
    let mut evm = TaikoEvmFactory.create_evm(
        db_with_contract(limit_exceeding_keccak_bytecode()),
        evm_env(TaikoSpecId::UZEN),
    );

    let err = evm.transact(tx_env(5_000_000)).expect_err("Uzen tx should abort");

    assert!(matches!(
        err,
        reth_revm::context::result::EVMError::Custom(message)
            if message == ZK_GAS_LIMIT_ERR
    ));
    assert!(evm.shared_meter().is_some());
}

#[test]
fn uzen_default_create_evm_path_is_metered() {
    let mut evm = TaikoEvmFactory.create_evm(
        db_with_contract(limit_exceeding_keccak_bytecode()),
        evm_env(TaikoSpecId::UZEN),
    );

    assert!(evm.shared_meter().is_some());
    assert!(evm.transact(tx_env(5_000_000)).is_err());
}

#[test]
fn non_uzen_default_create_evm_path_keeps_metering_disabled() {
    let mut evm = TaikoEvmFactory.create_evm(
        db_with_contract(limit_exceeding_keccak_bytecode()),
        evm_env(TaikoSpecId::SHASTA),
    );

    assert!(evm.shared_meter().is_none());
    evm.transact(tx_env(5_000_000)).expect("non-Uzen tx should stay on the legacy path");
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
