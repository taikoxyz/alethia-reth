//! Deterministic ordinary-storage diagnostics through the actual EVM wrapper and shared hooks.

use super::*;
use crate::zk_gas::pricing::StorageJournalDiagnostics;

fn commit_pricing(evm: &mut impl TaikoZkGasEvm) {
    evm.record_transaction_payload(super::super::pricing::TransactionPayloadSelectors {
        canonical_transaction_bytes: 100,
        nonempty_payload_transactions: 0,
    });
    evm.commit_transaction_zk_gas_pricing().unwrap();
}

fn read(key: u8) -> Vec<u8> {
    vec![opcode::PUSH1, key, opcode::SLOAD, opcode::POP]
}

#[test]
fn cold_insert_and_warm_query_have_literal_units_on_both_paths() {
    let mut code = read(0);
    code.extend(read(1));
    code.extend(read(0));
    code.push(opcode::STOP);
    let (plain, inspected) = zk_gas_pricing_path_snapshots(Bytecode::new_raw(code.into()), 100_000);
    assert_eq!(plain, inspected);
    assert_eq!(
        plain.storage_journal,
        StorageJournalDiagnostics {
            potential_sload_count: 3,
            direct_lookup_units: 6,
            max_occupancy_before: 2,
            high_water_upper: 3,
            potential_rollback_entries: 2,
            rollback_lookup_units_upper: 6,
            materialization_count_upper: 2,
            max_low32_group_upper: 1,
            comparison_equivalents_high_water_upper: 2,
            comparison_work_units_upper: 14,
            ..Default::default()
        }
    );
}

#[test]
fn zero_and_nonzero_prestate_values_give_same_map_occupancy_and_execution() {
    let mut code = read(0);
    code.extend(read(0));
    code.push(opcode::STOP);
    let mut summaries = Vec::new();
    for value in [U256::ZERO, U256::from(7)] {
        let mut db = db_with_contract(Bytecode::new_raw(code.clone().into()));
        db.insert_account_storage(BENCH_TARGET, U256::ZERO, value).unwrap();
        let mut observing = TaikoEvmFactory.create_evm(db.clone(), evm_env(TaikoSpecId::UNZEN));
        observing.set_zk_gas_pricing_enabled(true);
        let observed = observing.transact(tx_env(100_000)).unwrap();
        let mut disabled = TaikoEvmFactory.create_evm(db, evm_env(TaikoSpecId::UNZEN));
        let baseline = disabled.transact(tx_env(100_000)).unwrap();
        assert_eq!(observed, baseline, "collector must preserve state, result and native gas");
        assert_eq!(
            observing.meter().unwrap().tx_zk_gas_used(),
            disabled.meter().unwrap().tx_zk_gas_used()
        );
        assert!(disabled.zk_gas_pricing_snapshot().is_none());
        commit_pricing(&mut observing);
        summaries.push(observing.zk_gas_pricing_snapshot().unwrap().storage_journal);
    }
    assert_eq!(summaries[0], summaries[1]);
    assert_eq!(summaries[0].direct_lookup_units, 3);
}

#[test]
fn nested_revert_retains_map_occupancy_and_uses_storage_owner_for_delegatecall() {
    let child = Address::repeat_byte(0x17);
    let mut child_code = read(1);
    child_code.extend([opcode::PUSH0, opcode::PUSH0, opcode::REVERT]);
    for (spawn, expected_direct, expected_reserve) in
        [(opcode::CALL, 4, 5), (opcode::DELEGATECALL, 6, 9)]
    {
        let mut parent = read(0);
        parent.extend([opcode::PUSH0, opcode::PUSH0, opcode::PUSH0, opcode::PUSH0]);
        if spawn == opcode::CALL {
            parent.push(opcode::PUSH0);
        }
        parent.push(opcode::PUSH20);
        parent.extend(child.as_slice());
        parent.extend([opcode::PUSH2, 0xff, 0xff, spawn, opcode::POP]);
        parent.extend(read(1));
        parent.push(opcode::STOP);
        let make_db = || {
            let mut db = db_with_contract(Bytecode::new_raw(parent.clone().into()));
            insert_contract(&mut db, child, Bytecode::new_raw(child_code.clone().into()));
            db
        };
        let mut plain = TaikoEvmFactory.create_evm(make_db(), evm_env(TaikoSpecId::UNZEN));
        plain.set_zk_gas_pricing_enabled(true);
        assert!(plain.transact(tx_env(200_000)).unwrap().result.is_success());
        commit_pricing(&mut plain);
        let plain = plain.zk_gas_pricing_snapshot().unwrap();
        let mut inspected = TaikoEvmFactory.create_evm_with_inspector(
            make_db(),
            evm_env(TaikoSpecId::UNZEN),
            NoOpInspector,
        );
        inspected.set_zk_gas_pricing_enabled(true);
        inspected.transact(tx_env(200_000)).unwrap();
        commit_pricing(&mut inspected);
        assert_eq!(plain, inspected.zk_gas_pricing_snapshot().unwrap());
        assert_eq!(plain.storage_journal.potential_sload_count, 3);
        assert_eq!(plain.storage_journal.direct_lookup_units, expected_direct);
        assert_eq!(plain.storage_journal.rollback_lookup_units_upper, expected_reserve);
        assert_eq!(plain.storage_journal.incomplete, None);
    }
}

#[test]
fn included_outer_revert_halt_and_prelookup_stack_underflow_keep_known_work() {
    for suffix in [
        vec![opcode::PUSH0, opcode::PUSH0, opcode::REVERT],
        vec![opcode::INVALID],
        vec![opcode::SLOAD],
    ] {
        let mut code = read(0);
        code.extend(suffix);
        let (plain, inspected) =
            zk_gas_pricing_path_snapshots(Bytecode::new_raw(code.into()), 100_000);
        assert_eq!(plain, inspected);
        assert_eq!(plain.storage_journal.potential_sload_count, 1);
        assert_eq!(plain.storage_journal.direct_lookup_units, 1);
        assert_eq!(plain.storage_journal.rollback_lookup_units_upper, 1);
        assert_eq!(plain.storage_journal.incomplete, None);
        assert!(matches!(plain.shadow_report().status(), ZkGasShadowStatus::Incomplete { .. }));
    }
}

#[test]
fn noop_changed_store_and_transient_exclusion_match_on_both_paths() {
    let code = vec![
        opcode::PUSH0,
        opcode::PUSH0,
        opcode::SSTORE,
        opcode::PUSH1,
        1,
        opcode::PUSH0,
        opcode::SSTORE,
        opcode::PUSH0,
        opcode::SLOAD,
        opcode::POP,
        opcode::PUSH1,
        1,
        opcode::PUSH0,
        opcode::TSTORE,
        opcode::PUSH0,
        opcode::TLOAD,
        opcode::POP,
        opcode::STOP,
    ];
    let (plain, inspected) = zk_gas_pricing_path_snapshots(Bytecode::new_raw(code.into()), 100_000);
    assert_eq!(plain, inspected);
    assert_eq!(plain.storage_journal.potential_sload_count, 1);
    assert_eq!(plain.storage_journal.potential_sstore_count, 2);
    assert_eq!(plain.storage_journal.direct_lookup_units, 5);
    assert_eq!(plain.storage_journal.potential_rollback_entries, 3);
    assert_eq!(plain.storage_journal.rollback_lookup_units_upper, 6);
}

#[test]
fn access_list_warm_vacant_lookup_excludes_dormant_declared_slots() {
    let mut code = read(0);
    code.push(opcode::STOP);
    let mut evm = TaikoEvmFactory
        .create_evm(db_with_contract(Bytecode::new_raw(code.into())), evm_env(TaikoSpecId::UNZEN));
    evm.set_zk_gas_pricing_enabled(true);
    let mut tx = tx_env(200_000);
    tx.access_list.0.push(Default::default());
    tx.access_list.0[0].address = BENCH_TARGET;
    tx.access_list.0[0].storage_keys =
        (0_u64..16).map(|n| alloy_primitives::B256::from(U256::from(n).to_be_bytes())).collect();
    tx.derive_tx_type().unwrap();
    evm.transact(tx).unwrap();
    commit_pricing(&mut evm);
    let snapshot = evm.zk_gas_pricing_snapshot().unwrap();
    assert_eq!(snapshot.resources.unique_storage_slot_access_count, 16);
    assert_eq!(snapshot.storage_journal.potential_sload_count, 1);
    assert_eq!(snapshot.storage_journal.direct_lookup_units, 1);
    assert_eq!(snapshot.storage_journal.potential_rollback_entries, 0);
    assert_eq!(snapshot.storage_journal.incomplete, None);
}

#[test]
fn transaction_reset_and_committed_value_persistence_do_not_carry_journal_occupancy() {
    let code = vec![opcode::PUSH1, 1, opcode::PUSH0, opcode::SSTORE, opcode::STOP];
    let mut evm = TaikoEvmFactory
        .create_evm(db_with_contract(Bytecode::new_raw(code.into())), evm_env(TaikoSpecId::UNZEN));
    evm.set_zk_gas_pricing_enabled(true);
    let result = evm.transact(tx_env(100_000)).unwrap();
    assert_eq!(result.state[&BENCH_TARGET].storage[&U256::ZERO].present_value(), U256::from(1));
    reth_revm::DatabaseCommit::commit(evm.db_mut(), result.state);
    commit_pricing(&mut evm);
    let first = evm.zk_gas_pricing_snapshot().unwrap().storage_journal;
    let mut second_tx = tx_env(100_000);
    second_tx.nonce = 1;
    let second = evm.transact(second_tx).unwrap();
    assert_eq!(second.state[&BENCH_TARGET].storage[&U256::ZERO].original_value(), U256::from(1));
    commit_pricing(&mut evm);
    let committed = evm.zk_gas_pricing_snapshot().unwrap().storage_journal;
    assert_eq!(committed.direct_lookup_units, 2);
    assert_eq!(committed.rollback_lookup_units_upper, 4);
    assert_eq!(committed.high_water_upper, 1);
    assert_eq!(first.direct_lookup_units, 1);
    assert_eq!(first.materialization_count_upper, 1);
    assert_eq!(first.comparison_work_units_upper, 4);
    assert_eq!(committed.materialization_count_upper, 2);
    assert_eq!(committed.comparison_work_units_upper, 8);

    // A subsequent candidate rejected by the executor's commit predicate is attempted only.
    let mut candidate = tx_env(100_000);
    candidate.nonce = 1;
    evm.transact(candidate).unwrap();
    evm.reset_transaction_zk_gas();
    assert_eq!(evm.zk_gas_pricing_snapshot().unwrap().storage_journal, committed);
    let attempted = evm.zk_gas_attempted_pricing_snapshot().unwrap();
    assert_eq!(attempted.transaction_count, 3);
    assert_eq!(attempted.block.storage_journal.direct_lookup_units, 3);
    assert_eq!(attempted.block.storage_journal.rollback_lookup_units_upper, 6);
}

type StorageEvm = <TaikoEvmFactory as EvmFactory>::Evm<InMemoryDB, NoOpInspector>;

fn storage_evm(db: InMemoryDB, inspect: bool) -> StorageEvm {
    let evm = if inspect {
        TaikoEvmFactory.create_evm_with_inspector(db, evm_env(TaikoSpecId::UNZEN), NoOpInspector)
    } else {
        TaikoEvmFactory.create_evm(db, evm_env(TaikoSpecId::UNZEN))
    };
    assert!(evm.meter().is_some(), "both real paths retain their legacy meter");
    evm
}

fn assert_storage_routing(evm: StorageEvm, inspect: bool) {
    let base = evm.into_inner();
    assert_eq!(base.zk_gas_meter().is_some(), !inspect, "production meter routing");
    assert_eq!(base.inner.inspector.meter().is_some(), inspect, "inspector meter routing");
}

fn storage_call(target: Address, opcode: u8) -> Vec<u8> {
    let mut code = vec![opcode::PUSH0; 4];
    if matches!(opcode, opcode::CALL | opcode::CALLCODE) {
        code.push(opcode::PUSH0);
    }
    code.push(opcode::PUSH20);
    code.extend(target.as_slice());
    code.extend([opcode::PUSH3, 0x03, 0x00, 0x00, opcode, opcode::POP]);
    code
}

#[test]
fn independent_sibling_reverts_retain_occupied_slots_and_rewarm_per_storage_owner() {
    let child = Address::with_last_byte(0xb2);
    // Parent: read0, reverted child(read1), reverted child(read1), read2, read0.
    // Shared owner lengths: 0,1,2,2,3 -> 13; separate owners: 0,0,1,1,2 -> 9.
    for (opcode, direct, max_occupancy, reserve) in [
        (opcode::CALL, 9, 2, 10),
        (opcode::STATICCALL, 9, 2, 10),
        (opcode::CALLCODE, 13, 3, 16),
        (opcode::DELEGATECALL, 13, 3, 16),
    ] {
        let expected = StorageJournalDiagnostics {
            potential_sload_count: 5,
            potential_sstore_count: 0,
            direct_lookup_units: direct,
            max_occupancy_before: max_occupancy,
            high_water_upper: max_occupancy + 1,
            potential_rollback_entries: 4,
            rollback_lookup_units_upper: reserve,
            materialization_count_upper: 3,
            max_low32_group_upper: 1,
            comparison_equivalents_high_water_upper: 2,
            comparison_work_units_upper: if opcode == opcode::CALL || opcode == opcode::STATICCALL {
                19
            } else {
                24
            },
            incomplete: None,
        };
        let mut parent = read(0);
        parent.extend(storage_call(child, opcode));
        parent.extend(storage_call(child, opcode));
        parent.extend(read(2));
        parent.extend(read(0));
        parent.push(opcode::STOP);
        let mut child_code = read(1);
        child_code.extend([opcode::PUSH0, opcode::PUSH0, opcode::REVERT]);
        let mut snapshots = Vec::new();
        for inspect in [false, true] {
            let mut db = db_with_contract(Bytecode::new_raw(parent.clone().into()));
            insert_contract(&mut db, child, Bytecode::new_raw(child_code.clone().into()));
            let mut evm = storage_evm(db, inspect);
            evm.set_zk_gas_pricing_enabled(true);
            assert!(evm.transact(tx_env(1_000_000)).unwrap().result.is_success());
            commit_pricing(&mut evm);
            let snapshot = evm.zk_gas_pricing_snapshot().unwrap();
            assert_eq!(snapshot.storage_journal, expected, "opcode={opcode:#x} inspect={inspect}");
            snapshots.push(snapshot);
            assert_storage_routing(evm, inspect);
        }
        assert_eq!(snapshots[0], snapshots[1]);
    }
}

#[test]
fn independent_reverted_write_restores_value_but_keeps_physical_growth_and_meter_identity() {
    let child = Address::with_last_byte(0xb3);
    for (opcode, direct, max_occupancy, reserve) in
        [(opcode::CALL, 9, 2, 12), (opcode::DELEGATECALL, 14, 3, 20)]
    {
        let expected = StorageJournalDiagnostics {
            potential_sload_count: 4,
            potential_sstore_count: 1,
            direct_lookup_units: direct,
            max_occupancy_before: max_occupancy,
            high_water_upper: max_occupancy + 1,
            potential_rollback_entries: 5,
            rollback_lookup_units_upper: reserve,
            materialization_count_upper: if opcode == opcode::CALL { 4 } else { 3 },
            max_low32_group_upper: 1,
            comparison_equivalents_high_water_upper: 2,
            comparison_work_units_upper: if opcode == opcode::CALL { 28 } else { 26 },
            incomplete: None,
        };
        let mut parent = read(0);
        parent.extend(storage_call(child, opcode));
        parent.extend(read(2));
        parent.extend(read(0));
        parent.push(opcode::STOP);
        let mut child_code = read(1);
        child_code.extend([
            opcode::PUSH1,
            9,
            opcode::PUSH1,
            2,
            opcode::SSTORE,
            opcode::PUSH0,
            opcode::PUSH0,
            opcode::REVERT,
        ]);
        let mut reference = None;
        let mut reference_meter = None;
        for (observe, inspect) in [(false, false), (true, false), (true, true)] {
            let mut db = db_with_contract(Bytecode::new_raw(parent.clone().into()));
            insert_contract(&mut db, child, Bytecode::new_raw(child_code.clone().into()));
            let mut evm = storage_evm(db, inspect);
            evm.set_zk_gas_pricing_enabled(observe);
            let result = evm.transact(tx_env(1_000_000)).unwrap();
            assert!(result.result.is_success());
            let owner = if opcode == opcode::CALL { child } else { BENCH_TARGET };
            if let Some(account) = result.state.get(&owner) {
                if let Some(slot) = account.storage.get(&U256::from(2)) {
                    assert_eq!(slot.present_value(), U256::ZERO);
                }
            }
            let meter = evm.meter().map(|meter| meter.tx_zk_gas_used());
            if let Some(ref previous) = reference {
                assert_eq!(&result, previous);
                assert_eq!(meter, reference_meter.unwrap());
            } else {
                reference = Some(result);
                reference_meter = Some(meter);
            }
            if observe {
                commit_pricing(&mut evm);
                assert_eq!(evm.zk_gas_pricing_snapshot().unwrap().storage_journal, expected);
            } else {
                assert!(evm.zk_gas_pricing_snapshot().is_none());
            }
            assert_storage_routing(evm, inspect);
        }
    }
}

fn storage_limit_crossing(opcode: u8) {
    let recovery_owner = Address::with_last_byte(0xc4);
    let store = opcode == opcode::SSTORE;
    let mut storage_code = if store { vec![opcode::PUSH1, 1] } else { Vec::new() };
    storage_code.extend([opcode::PUSH0, opcode, opcode::STOP]);
    // The rejected candidate has four extra raw gas of non-storage work. After reset, a
    // shorter fresh transaction fits the same reserved budget without changing the schedule.
    let mut crossing_code = vec![opcode::PUSH0, opcode::POP];
    crossing_code.extend(storage_code.clone());
    let schedule = &UNZEN_ZK_GAS_SCHEDULE;
    let charge = |op: u8, gas| gas * u64::from(schedule.opcode_multipliers[usize::from(op)]);
    let storage_gas = if store { 22_100 } else { 2_100 };
    let admitted = charge(opcode::PUSH0, 2) +
        charge(opcode::POP, 2) +
        u64::from(store) * charge(opcode::PUSH1, 3) +
        charge(opcode::PUSH0, 2) +
        charge(opcode, storage_gas) -
        1;
    let expected = StorageJournalDiagnostics {
        potential_sload_count: u64::from(!store),
        potential_sstore_count: u64::from(store),
        direct_lookup_units: 1,
        max_occupancy_before: 0,
        high_water_upper: 1,
        potential_rollback_entries: 1 + u64::from(store),
        rollback_lookup_units_upper: 1 + u64::from(store),
        materialization_count_upper: 1,
        max_low32_group_upper: 1,
        comparison_equivalents_high_water_upper: 1,
        comparison_work_units_upper: 3 + u64::from(store),
        incomplete: None,
    };
    let mut reference_error = None;
    let mut reference_recovery = None;
    let mut reference_attempt = None;
    for inspect in [false, true] {
        for observe in [false, true] {
            let mut db = db_with_contract(Bytecode::new_raw(crossing_code.clone().into()));
            insert_contract(
                &mut db,
                recovery_owner,
                Bytecode::new_raw(storage_code.clone().into()),
            );
            let mut evm = storage_evm(db, inspect);
            evm.set_zk_gas_pricing_enabled(observe);
            evm.reserve_block_zk_gas(schedule.block_limit - admitted).unwrap();
            let error = evm.transact(tx_env(100_000)).unwrap_err().to_string();
            assert_eq!(error, ZK_GAS_LIMIT_ERR);
            assert_eq!(
                evm.meter().unwrap().tx_zk_gas_used(),
                admitted - charge(opcode, storage_gas) + 1
            );
            if let Some(previous) = &reference_error {
                assert_eq!(&error, previous, "observation and routing preserve rejection");
            } else {
                reference_error = Some(error);
            }
            evm.record_transaction_payload(super::super::pricing::TransactionPayloadSelectors {
                canonical_transaction_bytes: 100,
                nonempty_payload_transactions: 0,
            });
            evm.reset_transaction_zk_gas();
            if observe {
                let committed = evm.zk_gas_pricing_snapshot().unwrap();
                assert_eq!(committed, super::super::pricing::ZkGasBlockSnapshot::default());
                let attempt = evm.zk_gas_attempted_pricing_snapshot().unwrap();
                assert_eq!(attempt.transaction_count, 1);
                assert_eq!(attempt.block.storage_journal, expected);
                assert!(
                    !attempt
                        .block
                        .operation_routes
                        .iter()
                        .any(|row| { row.route == OperationRoute::Opcode(opcode) }),
                    "limit-crossing storage step remains excluded from old priced routes"
                );
                if let Some(previous) = &reference_attempt {
                    assert_eq!(&attempt, previous, "actual path price and diagnostic parity");
                } else {
                    reference_attempt = Some(attempt);
                }
            }
            let mut recovery = tx_env(100_000);
            recovery.kind = TxKind::Call(recovery_owner);
            let result = evm.transact(recovery).unwrap();
            assert!(result.result.is_success());
            let recovery_meter = evm.meter().unwrap().tx_zk_gas_used();
            assert!(recovery_meter < admitted);
            if let Some((previous, meter)) = &reference_recovery {
                assert_eq!(&result, previous, "fresh-scope result, gas and state parity");
                assert_eq!(&recovery_meter, meter);
            } else {
                reference_recovery = Some((result, recovery_meter));
            }
            if observe {
                commit_pricing(&mut evm);
                assert_eq!(evm.zk_gas_pricing_snapshot().unwrap().storage_journal, expected);
                let attempted = evm.zk_gas_attempted_pricing_snapshot().unwrap();
                assert_eq!(attempted.transaction_count, 2);
                assert_eq!(attempted.block.storage_journal.direct_lookup_units, 2);
                assert_eq!(
                    attempted.block.storage_journal.potential_sload_count,
                    2 * u64::from(!store)
                );
                assert_eq!(
                    attempted.block.storage_journal.potential_sstore_count,
                    2 * u64::from(store)
                );
                assert_eq!(
                    attempted.block.storage_journal.rollback_lookup_units_upper,
                    2 * (1 + u64::from(store))
                );
                assert_eq!(attempted.block.storage_journal.incomplete, None);
            }
            assert_storage_routing(evm, inspect);
        }
    }
}

#[test]
fn storage_journal_sload_crossing_legacy_limit_retains_attempted_work_and_recovers() {
    storage_limit_crossing(opcode::SLOAD);
}

#[test]
fn storage_journal_sstore_crossing_legacy_limit_retains_attempted_work_and_recovers() {
    storage_limit_crossing(opcode::SSTORE);
}

#[test]
fn initially_cached_slots_are_seeded_once_on_both_real_execution_paths() {
    use reth_revm::state::{Account, EvmStorageSlot};

    let mut code = read(0);
    code.extend(read(0));
    code.push(opcode::STOP);
    let mut reference = None;
    for inspect in [false, true] {
        let mut db = db_with_contract(Bytecode::new_raw(code.clone().into()));
        let info = reth_revm::Database::basic(&mut db, BENCH_TARGET).unwrap().unwrap();
        let mut evm = storage_evm(db, inspect);
        let transaction_id = evm.ctx().journaled_state.inner.transaction_id;
        let mut account = Account::from(info);
        account.transaction_id = transaction_id;
        account.storage.insert(U256::ZERO, EvmStorageSlot::new(U256::ZERO, transaction_id));
        evm.ctx_mut().journaled_state.inner.state.insert(BENCH_TARGET, account);
        evm.set_zk_gas_pricing_enabled(true);
        assert!(evm.transact(tx_env(100_000)).unwrap().result.is_success());
        commit_pricing(&mut evm);
        let report = evm.zk_gas_pricing_snapshot().unwrap().storage_journal;
        assert_eq!(report.materialization_count_upper, 1, "preloaded key must not count again");
        assert_eq!(report.max_low32_group_upper, 1);
        assert_eq!(report.comparison_equivalents_high_water_upper, 1);
        assert_eq!(report.potential_sload_count, 2);
        assert_eq!(report.comparison_work_units_upper, 3 + report.potential_rollback_entries);
        assert!(report.incomplete.is_none());
        if let Some(previous) = reference {
            assert_eq!(report, previous);
        }
        reference = Some(report);
        assert_storage_routing(evm, inspect);
    }
}

#[test]
fn low32_materialization_capture_has_literal_units_on_both_paths() {
    for same_prefix in [false, true] {
        let mut code = Vec::new();
        for n in 0..64 {
            let key: U256 = if same_prefix { U256::from(n) << 224 } else { U256::from(n) };
            code.push(opcode::PUSH32);
            code.extend(key.to_be_bytes::<32>());
            code.extend([opcode::SLOAD, opcode::POP]);
        }
        for _ in 0..3 {
            code.extend(read(0));
        }
        code.push(opcode::STOP);
        let (plain, inspected) =
            zk_gas_pricing_path_snapshots(Bytecode::new_raw(code.into()), 1_000_000);
        assert_eq!(plain, inspected);
        let report = plain.storage_journal;
        let c = if same_prefix { 64 } else { 9 };
        assert_eq!(report.potential_sload_count, 67);
        assert_eq!(report.materialization_count_upper, 64);
        assert_eq!(report.max_low32_group_upper, if same_prefix { 64 } else { 1 });
        assert_eq!(report.comparison_equivalents_high_water_upper, c);
        assert_eq!(report.comparison_work_units_upper, c * 195);
        assert!(report.incomplete.is_none());
    }
}

#[test]
fn caught_storage_stack_underflow_has_known_zero_journal_work_not_missing_metadata() {
    let child = Address::with_last_byte(0xd2);
    for child_code in
        [vec![opcode::SLOAD], vec![opcode::SSTORE], vec![opcode::PUSH1, 1, opcode::SSTORE]]
    {
        let mut parent = storage_call(child, opcode::CALL);
        parent.extend(read(0));
        parent.push(opcode::STOP);
        for inspect in [false, true] {
            let mut db = db_with_contract(Bytecode::new_raw(parent.clone().into()));
            insert_contract(&mut db, child, Bytecode::new_raw(child_code.clone().into()));
            let mut evm = storage_evm(db, inspect);
            evm.set_zk_gas_pricing_enabled(true);
            assert!(evm.transact(tx_env(1_000_000)).unwrap().result.is_success());
            commit_pricing(&mut evm);
            let snapshot = evm.zk_gas_pricing_snapshot().unwrap();
            assert_eq!(snapshot.storage_journal.potential_sload_count, 1);
            assert_eq!(snapshot.storage_journal.potential_sstore_count, 0);
            assert_eq!(snapshot.storage_journal.materialization_count_upper, 1);
            assert_eq!(snapshot.storage_journal.comparison_work_units_upper, 3);
            assert!(snapshot.storage_journal.incomplete.is_none());
            let ZkGasShadowStatus::Incomplete { known, .. } = snapshot.shadow_report().status()
            else {
                panic!("caught pre-lookup stack failure must preserve a priced known subtotal");
            };
            assert_eq!(known.storage_journal_work_units(), 3);
            assert_eq!(known.storage_journal_scaled(), 3 * 384 * FIXED_POINT_SCALE);
            assert!(known.feature_error_scaled() > 0, "opcode fallback remains charged");
            assert_storage_routing(evm, inspect);
        }
    }
}

fn transient_store(value: u8, key: u8) -> Vec<u8> {
    vec![opcode::PUSH1, value, opcode::PUSH1, key, opcode::TSTORE]
}

#[test]
fn transient_clear_refill_is_linear_and_never_populates_persistent_resource_sets() {
    for rounds in [1_u64, 64, 512] {
        let mut code = Vec::new();
        for _ in 0..rounds {
            code.extend(transient_store(1, 0));
            code.extend(transient_store(0, 0));
        }
        code.push(opcode::STOP);
        let (plain, inspected) =
            zk_gas_pricing_path_snapshots(Bytecode::new_raw(code.into()), 300_000);
        assert_eq!(plain, inspected);
        let d = plain.transient_storage_journal;
        assert_eq!(d.potential_tstore_count, 2 * rounds);
        assert_eq!(d.materialization_count_upper, 1);
        assert_eq!(d.max_low32_group_upper, 1);
        assert_eq!(d.comparison_equivalents_high_water_upper, 1);
        assert_eq!(d.comparison_work_units_upper, 4 * rounds + 1);
        assert_eq!(plain.storage_journal, StorageJournalDiagnostics::default());
        assert_eq!(plain.resources.unique_storage_slot_access_count, 0);
        assert_eq!(plain.resources.unique_storage_slot_write_count, 0);
        assert_eq!(plain.storage_journal_work_units().unwrap(), 4 * rounds + 1);
        let ZkGasShadowStatus::Incomplete { known, .. } = plain.shadow_report().status() else {
            panic!("complete journal work must have a known quote");
        };
        assert_eq!(known.storage_journal_work_units(), 4 * rounds + 1);
        assert_eq!(
            known.storage_journal_scaled(),
            i128::from(4 * rounds + 1) * 384 * FIXED_POINT_SCALE
        );
    }
}

#[test]
fn transient_miss_and_absent_zero_store_do_not_preconsume_nonzero_materialization() {
    let mut code = vec![opcode::PUSH0, opcode::TLOAD, opcode::POP];
    code.extend(transient_store(0, 0));
    code.extend(transient_store(1, 0));
    code.extend([opcode::PUSH0, opcode::TLOAD, opcode::POP, opcode::STOP]);
    let (plain, inspected) = zk_gas_pricing_path_snapshots(Bytecode::new_raw(code.into()), 100_000);
    assert_eq!(plain, inspected);
    assert_eq!(plain.transient_storage_journal.potential_tload_count, 2);
    assert_eq!(plain.transient_storage_journal.potential_tstore_count, 2);
    assert_eq!(plain.transient_storage_journal.materialization_count_upper, 1);
    assert_eq!(plain.transient_storage_journal.comparison_work_units_upper, 7);
    assert_eq!(plain.storage_journal, StorageJournalDiagnostics::default());
}

#[test]
fn transient_caught_revert_uses_effective_owner_for_call_delegatecall_and_callcode() {
    let child = Address::repeat_byte(0x4b);
    let mut child_code = transient_store(2, 0);
    child_code.extend([opcode::PUSH0, opcode::PUSH0, opcode::REVERT]);
    for (spawn, units, materialized) in
        [(opcode::CALL, 9, 2), (opcode::CALLCODE, 8, 1), (opcode::DELEGATECALL, 8, 1)]
    {
        let mut code = transient_store(1, 0);
        code.extend(storage_call(child, spawn));
        code.extend([opcode::PUSH0, opcode::TLOAD, opcode::PUSH0, opcode::MSTORE]);
        code.extend(transient_store(1, 0));
        code.extend([opcode::PUSH1, 32, opcode::PUSH0, opcode::RETURN]);
        let mut snapshots = Vec::new();
        for inspected in [false, true] {
            let mut db = db_with_contract(Bytecode::new_raw(code.clone().into()));
            insert_contract(&mut db, child, Bytecode::new_raw(child_code.clone().into()));
            let mut evm = storage_evm(db, inspected);
            evm.set_zk_gas_pricing_enabled(true);
            let result = evm.transact(tx_env(1_000_000)).unwrap();
            assert!(result.result.is_success());
            assert_eq!(
                result.result.output().unwrap().as_ref(),
                U256::from(1).to_be_bytes::<32>().as_slice()
            );
            commit_pricing(&mut evm);
            let snapshot = evm.zk_gas_pricing_snapshot().unwrap();
            assert_eq!(snapshot.transient_storage_journal.potential_tstore_count, 3);
            assert_eq!(snapshot.transient_storage_journal.potential_tload_count, 1);
            assert_eq!(
                snapshot.transient_storage_journal.materialization_count_upper,
                materialized
            );
            assert_eq!(snapshot.transient_storage_journal.comparison_work_units_upper, units);
            assert_eq!(snapshot.resources.unique_storage_slot_access_count, 0);
            snapshots.push(snapshot);
        }
        assert_eq!(snapshots[0], snapshots[1]);
    }
}

#[test]
fn transient_operand_failures_and_valid_operand_halts_retain_known_attempt_semantics() {
    for code in [vec![opcode::TLOAD], vec![opcode::TSTORE], vec![opcode::PUSH0, opcode::TSTORE]] {
        let (plain, inspected) =
            zk_gas_pricing_path_snapshots(Bytecode::new_raw(code.into()), 100_000);
        assert_eq!(plain, inspected);
        assert_eq!(plain.transient_storage_journal, Default::default());
        assert_eq!(plain.storage_journal_work_units().unwrap(), 0);
    }
    for terminal in [vec![opcode::PUSH0, opcode::PUSH0, opcode::REVERT], vec![opcode::INVALID]] {
        let mut code = transient_store(1, 0);
        code.extend(terminal);
        let (plain, inspected) =
            zk_gas_pricing_path_snapshots(Bytecode::new_raw(code.into()), 100_000);
        assert_eq!(plain, inspected);
        assert_eq!(plain.transient_storage_journal.potential_tstore_count, 1);
        assert_eq!(plain.transient_storage_journal.comparison_work_units_upper, 3);
        assert!(plain.transient_storage_journal.incomplete.is_none());
    }
    // The first TSTORE has valid operands but cannot pay100 gas after two PUSH1s.
    let mut code = transient_store(1, 0);
    code.push(opcode::STOP);
    let (plain, inspected) = zk_gas_pricing_path_snapshots(Bytecode::new_raw(code.into()), 21_105);
    assert_eq!(plain, inspected);
    assert_eq!(plain.transient_storage_journal.potential_tstore_count, 1);
    assert_eq!(plain.transient_storage_journal.materialization_count_upper, 1);
    assert_eq!(plain.transient_storage_journal.comparison_work_units_upper, 3);
}

#[test]
fn transient_transaction_reset_and_candidate_exclusion_preserve_legacy_execution_and_units() {
    let mut code = transient_store(1, 0);
    code.push(opcode::STOP);
    for inspected in [false, true] {
        let db = db_with_contract(Bytecode::new_raw(code.clone().into()));
        let mut on = storage_evm(db.clone(), inspected);
        let mut off = storage_evm(db, inspected);
        on.set_zk_gas_pricing_enabled(true);
        let first = on.transact(tx_env(100_000)).unwrap();
        let baseline = off.transact(tx_env(100_000)).unwrap();
        assert_eq!(first, baseline);
        assert_eq!(on.meter().unwrap().tx_zk_gas_used(), off.meter().unwrap().tx_zk_gas_used());
        assert!(off.zk_gas_pricing_snapshot().is_none());
        reth_revm::DatabaseCommit::commit(on.db_mut(), first.state);
        commit_pricing(&mut on);
        let mut tx = tx_env(100_000);
        tx.nonce = 1;
        on.transact(tx.clone()).unwrap();
        commit_pricing(&mut on);
        let included = on.zk_gas_pricing_snapshot().unwrap();
        assert_eq!(included.transient_storage_journal.materialization_count_upper, 2);
        assert_eq!(included.storage_journal_work_units().unwrap(), 6);
        on.transact(tx).unwrap();
        on.reset_transaction_zk_gas();
        assert_eq!(on.zk_gas_pricing_snapshot().unwrap(), included);
        let attempted = on.zk_gas_attempted_pricing_snapshot().unwrap();
        assert_eq!(attempted.transaction_count, 3);
        assert_eq!(attempted.block.transient_storage_journal.materialization_count_upper, 3);
        assert_eq!(attempted.block.storage_journal_work_units().unwrap(), 9);
    }
}

#[test]
fn transient_static_store_keeps_attempted_work_without_persistent_storage_or_new_owner_state() {
    let child = Address::repeat_byte(0x4c);
    let mut parent = storage_call(child, opcode::STATICCALL);
    parent.push(opcode::STOP);
    let mut child_code = transient_store(1, 7);
    child_code.push(opcode::STOP);
    let mut snapshots = Vec::new();
    for inspected in [false, true] {
        let mut db = db_with_contract(Bytecode::new_raw(parent.clone().into()));
        insert_contract(&mut db, child, Bytecode::new_raw(child_code.clone().into()));
        let mut evm = storage_evm(db, inspected);
        evm.set_zk_gas_pricing_enabled(true);
        assert!(evm.transact(tx_env(1_000_000)).unwrap().result.is_success());
        commit_pricing(&mut evm);
        let snapshot = evm.zk_gas_pricing_snapshot().unwrap();
        assert_eq!(snapshot.transient_storage_journal.potential_tstore_count, 1);
        assert_eq!(snapshot.transient_storage_journal.materialization_count_upper, 1);
        assert_eq!(snapshot.transient_storage_journal.comparison_work_units_upper, 3);
        assert_eq!(snapshot.resources.unique_storage_slot_write_count, 0);
        snapshots.push(snapshot);
    }
    assert_eq!(snapshots[0], snapshots[1]);
}

#[test]
fn initial_physically_present_transient_key_is_seeded_only_in_observing_mode() {
    let code = vec![
        opcode::PUSH1,
        7,
        opcode::TLOAD,
        opcode::PUSH0,
        opcode::MSTORE,
        opcode::PUSH1,
        32,
        opcode::PUSH0,
        opcode::RETURN,
    ];
    for inspected in [false, true] {
        let db = db_with_contract(Bytecode::new_raw(code.clone().into()));
        let mut on = storage_evm(db.clone(), inspected);
        let mut off = storage_evm(db, inspected);
        on.set_zk_gas_pricing_enabled(true);
        on.ctx_mut().journaled_state.inner.transient_storage.insert_value(
            BENCH_TARGET,
            U256::from(7),
            U256::from(9),
        );
        off.ctx_mut().journaled_state.inner.transient_storage.insert_value(
            BENCH_TARGET,
            U256::from(7),
            U256::from(9),
        );
        let result = on.transact(tx_env(100_000)).unwrap();
        assert_eq!(result, off.transact(tx_env(100_000)).unwrap());
        assert_eq!(
            result.result.output().unwrap().as_ref(),
            U256::from(9).to_be_bytes::<32>().as_slice()
        );
        assert_eq!(on.meter().unwrap().tx_zk_gas_used(), off.meter().unwrap().tx_zk_gas_used());
        assert!(off.zk_gas_pricing_snapshot().is_none());
        commit_pricing(&mut on);
        let snapshot = on.zk_gas_pricing_snapshot().unwrap();
        assert_eq!(snapshot.transient_storage_journal.materialization_count_upper, 1);
        assert_eq!(snapshot.transient_storage_journal.potential_tload_count, 1);
        assert_eq!(snapshot.transient_storage_journal.comparison_work_units_upper, 2);
        assert_eq!(snapshot.resources.unique_storage_slot_access_count, 0);
    }
}

#[test]
fn transient_legacy_limit_crossing_retains_attempted_step_on_both_paths_and_keeps_off_identical() {
    for op in [opcode::TLOAD, opcode::TSTORE] {
        let store = op == opcode::TSTORE;
        let mut code = vec![opcode::PUSH0, opcode::POP];
        if store {
            code.extend([opcode::PUSH1, 1]);
        }
        code.extend([opcode::PUSH0, op]);
        if !store {
            code.push(opcode::POP);
        }
        code.push(opcode::STOP);
        let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen has a legacy schedule");
        let charge = |opcode: u8, gas: u64| {
            gas * u64::from(schedule.opcode_multipliers[usize::from(opcode)])
        };
        let admitted = charge(opcode::PUSH0, 2) +
            charge(opcode::POP, 2) +
            u64::from(store) * charge(opcode::PUSH1, 3) +
            charge(opcode::PUSH0, 2) +
            charge(op, 100) -
            1;
        let mut previous_error = None;
        let mut previous_snapshot = None;
        for inspected in [false, true] {
            for observing in [false, true] {
                let mut evm = storage_evm(
                    db_with_contract(Bytecode::new_raw(code.clone().into())),
                    inspected,
                );
                evm.set_zk_gas_pricing_enabled(observing);
                evm.reserve_block_zk_gas(schedule.block_limit - admitted).unwrap();
                let error = evm.transact(tx_env(100_000)).unwrap_err().to_string();
                assert_eq!(error, ZK_GAS_LIMIT_ERR);
                let result = (error, evm.meter().unwrap().tx_zk_gas_used());
                if let Some(previous) = &previous_error {
                    assert_eq!(&result, previous);
                } else {
                    previous_error = Some(result);
                }
                evm.record_transaction_payload(
                    super::super::pricing::TransactionPayloadSelectors {
                        canonical_transaction_bytes: 100,
                        nonempty_payload_transactions: 0,
                    },
                );
                evm.reset_transaction_zk_gas();
                if observing {
                    assert_eq!(evm.zk_gas_pricing_snapshot().unwrap(), Default::default());
                    let attempted = evm.zk_gas_attempted_pricing_snapshot().unwrap();
                    assert_eq!(
                        attempted.block.transient_storage_journal.potential_tload_count,
                        u64::from(!store)
                    );
                    assert_eq!(
                        attempted.block.transient_storage_journal.potential_tstore_count,
                        u64::from(store)
                    );
                    assert_eq!(
                        attempted.block.transient_storage_journal.materialization_count_upper,
                        u64::from(store)
                    );
                    assert_eq!(
                        attempted.block.transient_storage_journal.comparison_work_units_upper,
                        3 * u64::from(store)
                    );
                    assert!(attempted.block.storage_journal.incomplete.is_none());
                    assert!(
                        !attempted
                            .block
                            .operation_routes
                            .iter()
                            .any(|route| route.route == OperationRoute::Opcode(op))
                    );
                    if let Some(previous) = &previous_snapshot {
                        assert_eq!(&attempted, previous);
                    } else {
                        previous_snapshot = Some(attempted);
                    }
                } else {
                    assert!(evm.zk_gas_pricing_snapshot().is_none());
                }
            }
        }
    }
}

#[test]
fn repeated_caught_reverts_removing_the_same_transient_key_keep_distinct_seen() {
    let child = Address::repeat_byte(0x4d);
    let mut child_code = transient_store(1, 7);
    child_code.extend([opcode::PUSH0, opcode::PUSH0, opcode::REVERT]);
    for spawn in [opcode::CALL, opcode::CALLCODE, opcode::DELEGATECALL] {
        let mut code = Vec::new();
        for _ in 0..8 {
            code.extend(storage_call(child, spawn));
        }
        code.push(opcode::STOP);
        let mut snapshots = Vec::new();
        for inspected in [false, true] {
            let mut db = db_with_contract(Bytecode::new_raw(code.clone().into()));
            insert_contract(&mut db, child, Bytecode::new_raw(child_code.clone().into()));
            let mut evm = storage_evm(db, inspected);
            evm.set_zk_gas_pricing_enabled(true);
            assert!(evm.transact(tx_env(1_000_000)).unwrap().result.is_success());
            commit_pricing(&mut evm);
            let snapshot = evm.zk_gas_pricing_snapshot().unwrap();
            assert_eq!(snapshot.transient_storage_journal.potential_tstore_count, 8);
            assert_eq!(snapshot.transient_storage_journal.materialization_count_upper, 1);
            assert_eq!(snapshot.transient_storage_journal.max_low32_group_upper, 1);
            assert_eq!(snapshot.transient_storage_journal.comparison_work_units_upper, 17);
            assert_eq!(snapshot.resources.unique_storage_slot_access_count, 0);
            snapshots.push(snapshot);
        }
        assert_eq!(snapshots[0], snapshots[1]);
    }
}
