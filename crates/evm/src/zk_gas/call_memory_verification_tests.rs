//! Independent behavioral checks for parent memory ownership by CALL-family wrappers.

use super::*;

const FAMILIES: [SpawnFamily; 4] =
    [SpawnFamily::Call, SpawnFamily::CallCode, SpawnFamily::DelegateCall, SpawnFamily::StaticCall];

// Independent literal tariff oracle; C_mem(words) is defined by the EVM.
fn expected_memory_growth(before: u64, after: u64) -> i128 {
    let cost = |words: u64| 3 * words + words * words / 512;
    let boundary = u64::from(after.div_ceil(128) > before.div_ceil(128).max(1));
    161_142_459_292_709_706_509 * i128::from(after > before) +
        2_719_407_004_771_945_406 * i128::from(cost(after) - cost(before)) +
        1_925_131_900_689_577_103_907 * i128::from(boundary)
}

#[test]
fn independent_call_memory_charges_only_partial_reservation_and_boundary_delta() {
    for family in FAMILIES {
        let base =
            call_memory_wrapper(&call_memory_snapshot(family, &CallMemoryCase::default()), family);
        for (before_bytes, after_bytes) in [(32, 64), (4096, 4097), (4097, 8192), (8192, 32768)] {
            let snapshot = call_memory_snapshot(
                family,
                &CallMemoryCase {
                    output: (0, after_bytes),
                    preexpanded_bytes: before_bytes,
                    ..Default::default()
                },
            );
            let actual = call_memory_wrapper(&snapshot, family);
            assert_eq!(
                actual.accepted_absolute_scaled - base.accepted_absolute_scaled,
                expected_memory_growth(
                    u64::from(before_bytes).div_ceil(32),
                    u64::from(after_bytes).div_ceil(32)
                ),
                "{} partial growth {before_bytes}->{after_bytes}",
                family.name()
            );
            assert_eq!(
                snapshot.diagnostic,
                Some(ZkGasAccumulatorError::MissingSignedTransactionPayload)
            );
        }
    }
}

fn execute_code(code: Vec<u8>, child_code: Bytecode) -> super::super::pricing::ZkGasBlockSnapshot {
    let child = Address::with_last_byte(0xD1);
    let mut db = db_with_contract(Bytecode::new_raw(Bytes::from(code)));
    insert_contract(&mut db, child, child_code);
    let mut plain = TaikoEvmFactory.create_evm(db.clone(), evm_env(TaikoSpecId::UNZEN));
    plain.set_zk_gas_pricing_enabled(true);
    assert!(plain.transact(tx_env(500_000)).expect("plain execution").result.is_success());
    plain.record_transaction_payload(super::super::pricing::TransactionPayloadSelectors {
        canonical_transaction_bytes: 100,
        nonempty_payload_transactions: 0,
    });
    plain.commit_transaction_zk_gas_pricing().expect("plain commit");
    let mut inspected = TaikoEvmFactory.create_evm_with_inspector(
        db,
        evm_env(TaikoSpecId::UNZEN),
        NoOpInspector {},
    );
    inspected.set_zk_gas_pricing_enabled(true);
    assert!(inspected.transact(tx_env(500_000)).expect("inspected execution").result.is_success());
    inspected.record_transaction_payload(super::super::pricing::TransactionPayloadSelectors {
        canonical_transaction_bytes: 100,
        nonempty_payload_transactions: 0,
    });
    inspected.commit_transaction_zk_gas_pricing().expect("inspected commit");
    let snapshot = plain.zk_gas_pricing_snapshot().expect("snapshot");
    assert_eq!(Some(snapshot.clone()), inspected.zk_gas_pricing_snapshot());
    assert_eq!(snapshot.diagnostic, None);
    snapshot
}

fn append_call(code: &mut Vec<u8>, family: SpawnFamily, output_size: u16, huge_zero_offsets: bool) {
    code.push(opcode::PUSH2);
    code.extend_from_slice(&output_size.to_be_bytes());
    if huge_zero_offsets {
        code.push(opcode::PUSH32);
        code.extend_from_slice(&[0xff; 32]);
    } else {
        code.push(opcode::PUSH0);
    }
    code.push(opcode::PUSH0); // input size
    if huge_zero_offsets {
        code.push(opcode::PUSH32);
        code.extend_from_slice(&[0xff; 32]);
    } else {
        code.push(opcode::PUSH0);
    }
    if matches!(family, SpawnFamily::Call | SpawnFamily::CallCode) {
        code.push(opcode::PUSH0);
    }
    code.push(opcode::PUSH20);
    code.extend_from_slice(Address::with_last_byte(0xD1).as_slice());
    code.extend_from_slice(&[opcode::PUSH2, 0xff, 0xff, family.opcode(), opcode::POP]);
}

#[test]
fn independent_call_memory_ignores_full_width_offsets_for_empty_ranges() {
    for family in FAMILIES {
        let baseline =
            call_memory_wrapper(&call_memory_snapshot(family, &CallMemoryCase::default()), family);
        let mut code = Vec::new();
        append_call(&mut code, family, 0, true);
        code.push(opcode::STOP);
        let snapshot = execute_code(code, simple_arithmetic_bytecode());
        assert_eq!(call_memory_wrapper(&snapshot, family), baseline);
    }
}

#[test]
fn independent_call_memory_repeated_calls_and_return_copy_do_not_reserve_twice() {
    // The child returns 32 bytes; CALL has already reserved the parent's whole 32KiB range.
    let child =
        Bytecode::new_raw(Bytes::from(vec![opcode::PUSH1, 32, opcode::PUSH0, opcode::RETURN]));
    for family in FAMILIES {
        let base =
            call_memory_wrapper(&call_memory_snapshot(family, &CallMemoryCase::default()), family);
        let grown = call_memory_wrapper(
            &call_memory_snapshot(
                family,
                &CallMemoryCase { output: (0, 32768), ..Default::default() },
            ),
            family,
        );
        let mut code = Vec::new();
        append_call(&mut code, family, 32768, false);
        append_call(&mut code, family, 32768, false);
        // Copy returned bytes to offset 0, inside the existing CALL output reservation.
        code.extend_from_slice(&[
            opcode::PUSH1,
            32,
            opcode::PUSH0,
            opcode::PUSH0,
            opcode::RETURNDATACOPY,
            opcode::STOP,
        ]);
        let snapshot = execute_code(code, child.clone());
        let row = call_memory_wrapper(&snapshot, family);
        assert_eq!(
            row.accepted_absolute_scaled,
            base.accepted_absolute_scaled + grown.accepted_absolute_scaled
        );
        assert_eq!(row.incremental_scaled, 2 * base.incremental_scaled);
        let copy = snapshot
            .operation_routes
            .iter()
            .find(|row| row.route == OperationRoute::Opcode(opcode::RETURNDATACOPY))
            .expect("return copy");
        // Compare with independently executed RETURNDATACOPY into a small already reserved range.
        let mut small = Vec::new();
        append_call(&mut small, family, 32, false);
        small.extend_from_slice(&[
            opcode::PUSH1,
            32,
            opcode::PUSH0,
            opcode::PUSH0,
            opcode::RETURNDATACOPY,
            opcode::STOP,
        ]);
        let small_snapshot = execute_code(small, child.clone());
        assert_eq!(
            copy,
            small_snapshot
                .operation_routes
                .iter()
                .find(|row| row.route == OperationRoute::Opcode(opcode::RETURNDATACOPY))
                .expect("small return copy")
        );
    }
}

#[test]
fn independent_call_memory_retains_parent_reservation_when_balance_rejects_child() {
    for family in [SpawnFamily::Call, SpawnFamily::CallCode] {
        let expected = call_memory_wrapper(
            &call_memory_snapshot(
                family,
                &CallMemoryCase { output: (0, 32768), ..Default::default() },
            ),
            family,
        );
        let mut code = Vec::new();
        append_call(&mut code, family, 32768, false);
        assert_eq!(code[6], opcode::PUSH0); // value argument, after the four memory arguments
        let value_push = std::iter::once(opcode::PUSH32).chain([0xff; 32]);
        code.splice(6..7, value_push);
        code.push(opcode::STOP);
        let snapshot = execute_code(code, simple_arithmetic_bytecode());
        assert_eq!(call_memory_wrapper(&snapshot, family), expected);
        assert!(
            !snapshot
                .operation_routes
                .iter()
                .any(|row| row.route == OperationRoute::Opcode(opcode::ADD)),
            "insufficient balance must prevent child arithmetic execution"
        );
    }
}
