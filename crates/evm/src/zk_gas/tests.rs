//! Tests for fork-scoped zk gas schedule selection.

use crate::spec::TaikoSpecId;

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
