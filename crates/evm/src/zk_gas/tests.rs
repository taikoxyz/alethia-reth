//! Tests for fork-scoped zk gas schedule selection.

use crate::spec::TaikoSpecId;

use super::meter::{UzenZkGasMeter, ZkGasOutcome};
use super::schedule::schedule_for;

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
    let mut meter = UzenZkGasMeter::new(schedule);

    meter.charge_opcode(0x01, 3).expect("charge");
    meter.commit_transaction().expect("commit");

    assert_eq!(meter.block_zk_gas_used(), 3 * u64::from(schedule.opcode_multipliers[0x01]));
}

#[test]
fn meter_treats_opcode_multiplication_overflow_as_limit_exceeded() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
    let mut meter = UzenZkGasMeter::new(schedule);
    let raw_gas = (u64::MAX / u64::from(schedule.opcode_multipliers[0x01])) + 1;

    assert!(matches!(meter.charge_opcode(0x01, raw_gas), Err(ZkGasOutcome::LimitExceeded)));
}

#[test]
fn meter_treats_precompile_multiplication_overflow_as_limit_exceeded() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
    let mut meter = UzenZkGasMeter::new(schedule);
    let raw_gas = (u64::MAX / u64::from(schedule.precompile_multipliers[0x01])) + 1;

    assert!(matches!(
        meter.charge_precompile(0x01, raw_gas),
        Err(ZkGasOutcome::LimitExceeded)
    ));
}

#[test]
fn meter_resets_transaction_usage_without_affecting_block_usage() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
    let mut meter = UzenZkGasMeter::new(schedule);

    meter.charge_opcode(0x01, 2).expect("charge");
    meter.reset_transaction();

    assert_eq!(meter.tx_zk_gas_used(), 0);
    assert_eq!(meter.block_zk_gas_used(), 0);
}

#[test]
fn meter_allows_exactly_remaining_block_budget() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
    let mut meter = UzenZkGasMeter::new(schedule);

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
    let mut meter = UzenZkGasMeter::new(schedule);

    meter.charge_opcode(0xf0, schedule.block_limit - 1).expect("prefill");
    meter.commit_transaction().expect("commit");

    assert!(matches!(meter.charge_opcode(0xf0, 2), Err(ZkGasOutcome::LimitExceeded)));
}

#[test]
fn meter_returns_limit_exceeded_for_precompile_over_block_budget() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
    let mut meter = UzenZkGasMeter::new(schedule);

    assert!(matches!(
        meter.charge_precompile(0x01, schedule.block_limit),
        Err(ZkGasOutcome::LimitExceeded)
    ));
}
