//! Deterministic ordinary-storage diagnostics through the actual EVM wrapper and shared hooks.

use super::*;
use crate::zk_gas::pricing::{StorageJournalDiagnostics, StorageJournalIncompleteness};

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
fn included_outer_revert_and_halt_keep_lookup_work_and_missing_stack_is_explicit() {
    for (suffix, missing_stack) in [
        (vec![opcode::PUSH0, opcode::PUSH0, opcode::REVERT], false),
        (vec![opcode::INVALID], false),
        (vec![opcode::SLOAD], true),
    ] {
        let mut code = read(0);
        code.extend(suffix);
        let (plain, inspected) =
            zk_gas_pricing_path_snapshots(Bytecode::new_raw(code.into()), 100_000);
        assert_eq!(plain, inspected);
        assert_eq!(plain.storage_journal.potential_sload_count, 1);
        assert_eq!(plain.storage_journal.direct_lookup_units, 1);
        assert_eq!(plain.storage_journal.rollback_lookup_units_upper, 1);
        if missing_stack {
            assert_eq!(
                plain.storage_journal.incomplete,
                Some(StorageJournalIncompleteness::MissingStorageInput)
            );
        } else {
            assert_eq!(plain.storage_journal.incomplete, None);
        }
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
