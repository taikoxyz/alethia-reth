//! Physical final-state receipts through both real handler paths, independent of opcode pricing.
use super::*;
use crate::zk_gas::pricing::{StorageTransitionDiagnostics, ZkGasModel};

fn commit(evm: &mut impl TaikoZkGasEvm) {
    evm.record_transaction_payload(super::super::pricing::TransactionPayloadSelectors {
        canonical_transaction_bytes: 100,
        nonempty_payload_transactions: 0,
    });
    evm.commit_transaction_zk_gas_pricing().unwrap();
}

fn store(value: u8, key: u8) -> Vec<u8> {
    vec![opcode::PUSH1, value, opcode::PUSH1, key, opcode::SSTORE]
}

#[test]
fn final_physical_changed_receipt_is_captured_on_both_real_paths() {
    let mut code = store(2, 0);
    code.push(opcode::STOP);
    let (plain, inspected) = zk_gas_pricing_path_snapshots(Bytecode::new_raw(code.into()), 100_000);
    assert_eq!(plain, inspected);
    assert_eq!(
        plain.storage_transition,
        StorageTransitionDiagnostics {
            physical_slot_receipts: 1,
            changed_slots: 1,
            observed_storage_slots: 1,
            max_owner_slots: 1,
            max_low32_group: 1,
            comparison_equivalents: 1,
            query_budget: 3,
            work_units: 3,
            completed_transactions: 1,
            incomplete: None,
        }
    );
    let ZkGasShadowStatus::Incomplete { known, .. } = plain.shadow_report().status() else {
        panic!("final receipt must support the ordinary incomplete block quote")
    };
    assert_eq!(known.storage_transition_work_units(), 3);
    assert_eq!(
        known.storage_transition_scaled(),
        ZkGasModel.price_storage_transition_work(3).unwrap()
    );
}

#[test]
fn final_noop_and_revert_keep_physical_read_work_without_semantic_drift() {
    for suffix in [
        vec![opcode::STOP],
        vec![opcode::PUSH0, opcode::PUSH0, opcode::REVERT],
        vec![opcode::INVALID],
    ] {
        let mut code = store(2, 0);
        if suffix == [opcode::STOP] {
            code.extend(store(1, 0));
        }
        code.extend(suffix);
        let mut reference = None;
        for observe in [false, true] {
            let mut db = db_with_contract(Bytecode::new_raw(code.clone().into()));
            db.insert_account_storage(BENCH_TARGET, U256::ZERO, U256::from(1)).unwrap();
            let mut evm = TaikoEvmFactory.create_evm(db, evm_env(TaikoSpecId::UNZEN));
            evm.set_zk_gas_pricing_enabled(observe);
            let result = evm.transact(tx_env(100_000)).unwrap();
            if let Some(old) = &reference {
                assert_eq!(&result, old);
            } else {
                reference = Some(result);
            }
            if observe {
                commit(&mut evm);
                let d = evm.zk_gas_pricing_snapshot().unwrap().storage_transition;
                assert_eq!((d.physical_slot_receipts, d.observed_storage_slots), (1, 1));
                assert_eq!((d.changed_slots, d.query_budget, d.work_units), (0, 1, 1));
                assert_eq!(d.incomplete, None);
            } else {
                assert!(evm.zk_gas_pricing_snapshot().is_none());
            }
        }
    }
}

#[test]
fn cross_transaction_restore_rejected_candidate_and_fresh_block_are_independent() {
    // Read two distinct, concentrated slots, then write slot zero from calldata.
    let mut code = Vec::new();
    for key in [1_u64 << 48, 2_u64 << 48] {
        code.push(opcode::PUSH8);
        code.extend(key.to_be_bytes());
        code.extend([opcode::SLOAD, opcode::POP]);
    }
    code.extend([opcode::PUSH0, opcode::CALLDATALOAD, opcode::PUSH0, opcode::SSTORE, opcode::STOP]);
    for inspect in [false, true] {
        let mut db = db_with_contract(Bytecode::new_raw(code.clone().into()));
        db.insert_account_storage(BENCH_TARGET, U256::ZERO, U256::from(1)).unwrap();
        let mut evm = if inspect {
            TaikoEvmFactory.create_evm_with_inspector(
                db,
                evm_env(TaikoSpecId::UNZEN),
                NoOpInspector,
            )
        } else {
            TaikoEvmFactory.create_evm(db, evm_env(TaikoSpecId::UNZEN))
        };
        evm.set_zk_gas_pricing_enabled(true);
        for (nonce, value) in [2_u64, 1, 1].into_iter().enumerate() {
            let mut tx = tx_env(100_000);
            tx.nonce = nonce as u64;
            tx.data = U256::from(value).to_be_bytes::<32>().to_vec().into();
            let result = evm.transact(tx).unwrap();
            assert!(result.result.is_success());
            reth_revm::DatabaseCommit::commit(evm.db_mut(), result.state);
            commit(&mut evm);
        }
        let d = evm.zk_gas_pricing_snapshot().unwrap().storage_transition;
        assert_eq!(
            (d.physical_slot_receipts, d.changed_slots, d.observed_storage_slots),
            (9, 2, 3)
        );
        assert_eq!((d.comparison_equivalents, d.query_budget, d.work_units), (3, 13, 39));
        let mut tx = tx_env(100_000);
        tx.nonce = 3;
        tx.data = U256::from(3).to_be_bytes::<32>().to_vec().into();
        evm.transact(tx).unwrap();
        evm.reset_transaction_zk_gas();
        assert_eq!(evm.zk_gas_pricing_snapshot().unwrap().storage_transition, d);
        assert_eq!(
            evm.zk_gas_attempted_pricing_snapshot().unwrap().block.storage_transition.work_units,
            54
        );
        evm.set_zk_gas_pricing_enabled(false);
        evm.set_zk_gas_pricing_enabled(true);
        assert_eq!(
            evm.zk_gas_pricing_snapshot().unwrap().storage_transition,
            StorageTransitionDiagnostics::EMPTY
        );
    }
}

#[test]
fn reverted_child_uses_storage_owner_and_retains_read_population_only() {
    let child = Address::with_last_byte(0xc5);
    for spawn in [opcode::CALL, opcode::DELEGATECALL] {
        let mut parent = store(1, 0);
        parent.extend([opcode::PUSH0; 4]);
        if spawn == opcode::CALL {
            parent.push(opcode::PUSH0);
        }
        parent.push(opcode::PUSH20);
        parent.extend(child.as_slice());
        parent.extend([opcode::PUSH3, 0x03, 0x00, 0x00, spawn, opcode::POP, opcode::STOP]);
        let mut child_code = store(2, 1);
        child_code.extend([opcode::PUSH0, opcode::PUSH0, opcode::REVERT]);
        let mut snapshots = Vec::new();
        for inspect in [false, true] {
            let mut db = db_with_contract(Bytecode::new_raw(parent.clone().into()));
            insert_contract(&mut db, child, Bytecode::new_raw(child_code.clone().into()));
            let mut evm = if inspect {
                TaikoEvmFactory.create_evm_with_inspector(
                    db,
                    evm_env(TaikoSpecId::UNZEN),
                    NoOpInspector,
                )
            } else {
                TaikoEvmFactory.create_evm(db, evm_env(TaikoSpecId::UNZEN))
            };
            evm.set_zk_gas_pricing_enabled(true);
            let result = evm.transact(tx_env(300_000)).unwrap();
            assert!(result.result.is_success());
            assert_eq!(
                result.state[&BENCH_TARGET].storage[&U256::ZERO].present_value(),
                U256::from(1)
            );
            let owner = if spawn == opcode::CALL { child } else { BENCH_TARGET };
            assert_eq!(result.state[&owner].storage[&U256::from(1)].present_value(), U256::ZERO);
            commit(&mut evm);
            snapshots.push(evm.zk_gas_pricing_snapshot().unwrap().storage_transition);
        }
        assert_eq!(snapshots[0], snapshots[1]);
        let d = snapshots[0];
        assert_eq!(
            (d.physical_slot_receipts, d.changed_slots, d.observed_storage_slots, d.query_budget),
            (2, 1, 2, 4)
        );
        assert_eq!(
            (d.max_owner_slots, d.comparison_equivalents, d.work_units),
            if spawn == opcode::CALL { (1, 1, 4) } else { (2, 2, 8) }
        );
    }
}
