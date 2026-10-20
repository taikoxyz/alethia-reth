//! Tests for fork-scoped zk gas schedule selection.

use alloy_evm::{Evm as AlloyEvm, EvmEnv, EvmFactory};
use alloy_primitives::{Address, U256, address};
use reth_revm::{
    Inspector,
    context::{
        TxEnv,
        result::{ExecutionResult, HaltReason},
    },
    db::InMemoryDB,
    inspector::NoOpInspector,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter, InterpreterAction,
        interpreter::EthInterpreter,
        interpreter_types::{Jumps, LoopControl},
    },
    primitives::{Bytes, TxKind},
    state::{AccountInfo, Bytecode, bytecode::opcode},
};
use revm_database_interface::{
    BENCH_CALLER, BENCH_CALLER_BALANCE, BENCH_TARGET, BENCH_TARGET_BALANCE,
};
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::{alloy::TaikoZkGasEvm, factory::TaikoEvmFactory, spec::TaikoSpecId};

use super::{
    adapter::ZK_GAS_LIMIT_ERR,
    meter::{ZkGasMeter, ZkGasOutcome},
    pricing::{
        AccessClass, BlockPricingInput, CalldataAccessClass, FIXED_POINT_SCALE, MemorySelectors,
        MptSelectors, OperationModelInput, OperationPricingInput, OperationRoute,
        OperationRoutePricingInput, ProposalPricingInput, RUNTIME_SPEC_SHA256, ResourceSelectors,
        StorageBranch, ZkGasAccumulatorError, ZkGasCompletionError, ZkGasError,
        ZkGasIncompleteness, ZkGasKnownBlockBreakdown, ZkGasMode, ZkGasModel,
        ZkGasShadowDiagnostic, ZkGasShadowReport, ZkGasShadowStatus,
    },
    schedule::{FAILSAFE_MULTIPLIER, ZkGasSchedule, schedule_for},
    unzen::{TX_INTRINSIC_ZK_GAS, UNZEN_ZK_GAS_SCHEDULE},
};

#[test]
fn zk_gas_mode_defaults_to_disabled_and_has_no_admission_variant() {
    assert_eq!(ZkGasMode::default(), ZkGasMode::Disabled);
    assert_eq!(ZkGasMode::ALL, [ZkGasMode::Disabled, ZkGasMode::Shadow]);
}

#[test]
fn zk_gas_shadow_report_marks_exact_resources_and_mpt_as_missing() {
    use super::pricing::{TransactionMetadata, ZkGasAccumulator};

    let mut accumulator = ZkGasAccumulator::default();
    accumulator.begin_transaction(TransactionMetadata::default()).expect("transaction starts");
    accumulator
        .observe_step(OperationPricingInput {
            route: OperationRoute::Opcode(0x01),
            interpreter_raw_gas: Some(3),
            native_gas: None,
            model_input: OperationModelInput::StaticRawGas { raw_gas: 3 },
            feature_error: false,
        })
        .expect("operation prices");
    accumulator.commit_transaction().expect("transaction commits");

    let report = accumulator.committed_snapshot().shadow_report();
    assert_eq!(report.model_id(), RUNTIME_SPEC_SHA256);
    let ZkGasShadowStatus::Incomplete { known, missing } = report.status() else {
        panic!("a snapshot without exact block resources and MPT must remain incomplete")
    };
    assert_eq!(missing, ZkGasIncompleteness::ExactBlockResourcesAndMptSelectorsUnavailable);
    assert!(known.operations_scaled() > 0);
    assert!(known.evm_local_known_subtotal_pgu() > 0);
}

#[test]
fn zk_gas_attempted_snapshot_retains_discarded_execution_scope() {
    use super::pricing::{TransactionMetadata, ZkGasAccumulator};

    let mut accumulator = ZkGasAccumulator::default();
    accumulator
        .begin_transaction(TransactionMetadata { is_anchor: false, native_value_transfer: false })
        .expect("transaction starts");
    accumulator
        .observe_step(OperationPricingInput {
            route: OperationRoute::Opcode(opcode::KECCAK256),
            interpreter_raw_gas: Some(36),
            native_gas: None,
            model_input: OperationModelInput::Keccak {
                input_length: 32,
                memory: MemorySelectors::default(),
            },
            feature_error: false,
        })
        .expect("discarded operation prices");

    accumulator.reset_transaction();

    assert_eq!(accumulator.committed_snapshot(), super::pricing::ZkGasBlockSnapshot::default());
    let attempted = accumulator.attempted_snapshot();
    assert_eq!(attempted.transaction_count, 1);
    assert_eq!(attempted.block.started_non_anchor_transactions, 1);
    assert!(
        attempted
            .block
            .operation_routes
            .iter()
            .any(|row| row.route == OperationRoute::Opcode(opcode::KECCAK256))
    );
}

#[test]
fn zk_gas_successful_commit_counts_once_in_attempted_and_committed_snapshots() {
    use super::pricing::{TransactionMetadata, ZkGasAccumulator};

    let mut accumulator = ZkGasAccumulator::default();
    accumulator
        .begin_transaction(TransactionMetadata { is_anchor: true, native_value_transfer: false })
        .expect("anchor starts");
    accumulator
        .observe_step(OperationPricingInput {
            route: OperationRoute::Opcode(opcode::ADD),
            interpreter_raw_gas: Some(3),
            native_gas: None,
            model_input: OperationModelInput::StaticRawGas { raw_gas: 3 },
            feature_error: false,
        })
        .expect("anchor operation prices");
    accumulator.commit_transaction().expect("anchor commits");

    let committed = accumulator.committed_snapshot();
    let attempted = accumulator.attempted_snapshot();
    assert_eq!(attempted.transaction_count, 1);
    assert_eq!(attempted.block, committed);

    accumulator.reset_transaction();
    assert_eq!(accumulator.attempted_snapshot(), attempted);
}

#[test]
fn zk_gas_reset_without_active_scope_does_not_create_an_attempt() {
    use super::pricing::ZkGasAccumulator;

    let mut accumulator = ZkGasAccumulator::default();
    accumulator.reset_transaction();

    assert_eq!(accumulator.attempted_snapshot().transaction_count, 0);
    assert_eq!(
        accumulator.attempted_snapshot().block,
        super::pricing::ZkGasBlockSnapshot::default()
    );
}

#[test]
fn zk_gas_pricing_complete_shadow_report_adds_exact_mpt_once() {
    use super::pricing::{TransactionMetadata, ZkGasAccumulator};

    let mut accumulator = ZkGasAccumulator::default();
    accumulator.begin_transaction(TransactionMetadata::default()).expect("transaction starts");
    accumulator
        .observe_step(OperationPricingInput {
            route: OperationRoute::Opcode(0x01),
            interpreter_raw_gas: Some(3),
            native_gas: None,
            model_input: OperationModelInput::StaticRawGas { raw_gas: 3 },
            feature_error: false,
        })
        .expect("operation prices");
    accumulator.commit_transaction().expect("transaction commits");
    let snapshot = accumulator.committed_snapshot();
    let mpt = MptSelectors {
        path_node_visits: 11,
        materialization_work_units: 7,
        hydration_hash_blocks: 5,
        dirty_encoded_nodes: 3,
        dirty_hash_blocks: 2,
        structural_branch_extension_churn_nodes: 1,
        dropped_trie_nodes: 4,
    };

    let completed = ZkGasModel
        .complete_block_shadow_report(&snapshot.shadow_report(), snapshot.resources, mpt)
        .expect("typed incomplete report plus exact MPT completes");
    let direct = ZkGasModel
        .price_block(&BlockPricingInput {
            started_non_anchor_transactions: snapshot.started_non_anchor_transactions,
            native_value_transfers: snapshot.native_value_transfers,
            operation_routes: &snapshot.operation_routes,
            resources: snapshot.resources,
            mpt,
        })
        .expect("same selector snapshot prices directly");
    assert_eq!(completed, direct);
    assert!(completed.mpt_scaled > 0);
}

#[test]
fn zk_gas_pricing_completion_replaces_evm_local_resources_with_the_authoritative_snapshot() {
    use alloy_primitives::Address;

    use super::pricing::{StateAccess, TransactionMetadata, ZkGasAccumulator};

    let mut accumulator = ZkGasAccumulator::default();
    accumulator.begin_transaction(TransactionMetadata::default()).expect("transaction starts");
    accumulator
        .observe_step(OperationPricingInput {
            route: OperationRoute::Opcode(0x01),
            interpreter_raw_gas: Some(3),
            native_gas: None,
            model_input: OperationModelInput::StaticRawGas { raw_gas: 3 },
            feature_error: false,
        })
        .expect("operation prices");
    accumulator
        .observe_state_access(StateAccess::Account(Address::with_last_byte(0xaa)))
        .expect("EVM-local resource is observed");
    accumulator.commit_transaction().expect("transaction commits");
    let snapshot = accumulator.committed_snapshot();
    let original = snapshot.shadow_report();
    let authoritative_resources = ResourceSelectors {
        unique_account_count: 9,
        unique_storage_slot_access_count: 8,
        unique_storage_slot_write_count: 6,
        unique_code_hash_count: 3,
        unique_code_bytes: 20_268,
    };

    let completed = ZkGasModel
        .complete_block_shadow_report(&original, authoritative_resources, MptSelectors::default())
        .expect("authoritative block resource snapshot completes");
    let ZkGasShadowStatus::Incomplete { known: before, .. } = original.status() else {
        panic!("fixture report must be incomplete")
    };
    assert_eq!(completed.fixed_scaled, before.fixed_scaled());
    assert_eq!(completed.operations_scaled, before.operations_scaled());
    assert_eq!(completed.feature_error_scaled, before.feature_error_scaled());
    assert!(completed.resources_scaled > before.evm_local_resources_scaled());
    let direct = ZkGasModel
        .price_block(&BlockPricingInput {
            started_non_anchor_transactions: snapshot.started_non_anchor_transactions,
            native_value_transfers: snapshot.native_value_transfers,
            operation_routes: &snapshot.operation_routes,
            resources: authoritative_resources,
            mpt: MptSelectors::default(),
        })
        .expect("same exact selectors price directly");
    assert_eq!(completed, direct, "exact resources replace rather than add to the local subset");
}

#[test]
fn zk_gas_pricing_resource_reconciliation_fails_closed_on_diagnostic() {
    let diagnostic = ZkGasShadowReport::collector_unavailable();
    assert_eq!(
        ZkGasModel.complete_block_shadow_report(
            &diagnostic,
            ResourceSelectors::default(),
            MptSelectors::default(),
        ),
        Err(ZkGasCompletionError::ZkGasShadowDiagnostic(
            ZkGasShadowDiagnostic::CollectorUnavailable
        ))
    );
}

#[test]
fn zk_gas_pricing_completion_rejects_exact_resources_below_any_evm_local_dimension() {
    use alloy_primitives::{Address, B256, U256};

    use super::pricing::{StateAccess, TransactionMetadata, ZkGasAccumulator};

    let mut accumulator = ZkGasAccumulator::default();
    accumulator.begin_transaction(TransactionMetadata::default()).expect("transaction starts");
    accumulator
        .observe_state_access(StateAccess::StorageWrite {
            address: Address::with_last_byte(0xaa),
            slot: U256::from(7),
        })
        .expect("storage resource is observed");
    accumulator
        .observe_state_access(StateAccess::Code {
            hash: B256::with_last_byte(0xbb),
            byte_length: 32,
        })
        .expect("code resource is observed");
    accumulator.commit_transaction().expect("transaction commits");
    let snapshot = accumulator.committed_snapshot();
    let report = snapshot.shadow_report();
    let local = snapshot.resources;
    let cases = [
        ("unique_account_count", ResourceSelectors { unique_account_count: 0, ..local }),
        (
            "unique_storage_slot_access_count",
            ResourceSelectors { unique_storage_slot_access_count: 0, ..local },
        ),
        (
            "unique_storage_slot_write_count",
            ResourceSelectors { unique_storage_slot_write_count: 0, ..local },
        ),
        ("unique_code_hash_count", ResourceSelectors { unique_code_hash_count: 0, ..local }),
        ("unique_code_bytes", ResourceSelectors { unique_code_bytes: 31, ..local }),
    ];
    for (field, exact) in cases {
        assert_eq!(
            ZkGasModel.complete_block_shadow_report(&report, exact, MptSelectors::default(),),
            Err(ZkGasCompletionError::ExactResourcesBelowEvmLocal(field)),
            "{field} must not shrink below the sealed EVM-local subset",
        );
    }
}

#[test]
fn zk_gas_pricing_complete_shadow_report_rejects_diagnostics_and_wrong_identity() {
    let diagnostic = ZkGasShadowReport::collector_unavailable();
    assert_eq!(
        ZkGasModel.complete_block_shadow_report(
            &diagnostic,
            ResourceSelectors::default(),
            MptSelectors::default(),
        ),
        Err(ZkGasCompletionError::ZkGasShadowDiagnostic(
            ZkGasShadowDiagnostic::CollectorUnavailable
        ))
    );

    let wrong_identity = ZkGasShadowReport::from_parts_for_test("wrong-model", diagnostic.status());
    assert_eq!(
        ZkGasModel.complete_block_shadow_report(
            &wrong_identity,
            ResourceSelectors::default(),
            MptSelectors::default(),
        ),
        Err(ZkGasCompletionError::ModelIdentity)
    );
}

#[test]
fn zk_gas_pricing_complete_shadow_report_rejects_tamper_overflow_and_mpt_bounds() {
    let report = |known| {
        ZkGasShadowReport::from_parts_for_test(
            RUNTIME_SPEC_SHA256,
            ZkGasShadowStatus::Incomplete {
                known,
                missing: ZkGasIncompleteness::ExactBlockResourcesAndMptSelectorsUnavailable,
            },
        )
    };
    let known = |fixed, operations, feature_error, resources, subtotal, pgu| {
        ZkGasKnownBlockBreakdown::from_parts_for_test(
            fixed,
            operations,
            feature_error,
            ResourceSelectors::default(),
            resources,
            subtotal,
            pgu,
        )
    };
    let valid = known(
        FIXED_POINT_SCALE,
        2 * FIXED_POINT_SCALE,
        FIXED_POINT_SCALE,
        0,
        3 * FIXED_POINT_SCALE,
        3,
    );

    let inconsistent_raw_resources = ZkGasKnownBlockBreakdown::from_parts_for_test(
        valid.fixed_scaled(),
        valid.operations_scaled(),
        valid.feature_error_scaled(),
        ResourceSelectors { unique_account_count: 1, ..Default::default() },
        valid.evm_local_resources_scaled(),
        valid.evm_local_known_subtotal_scaled(),
        valid.evm_local_known_subtotal_pgu(),
    );
    assert_eq!(
        ZkGasModel.complete_block_shadow_report(
            &report(inconsistent_raw_resources),
            ResourceSelectors { unique_account_count: 1, ..Default::default() },
            MptSelectors::default(),
        ),
        Err(ZkGasCompletionError::InconsistentKnownBreakdown("evm_local_resources_scaled"))
    );

    let tampered_total = known(
        valid.fixed_scaled(),
        valid.operations_scaled(),
        valid.feature_error_scaled(),
        valid.evm_local_resources_scaled(),
        valid.evm_local_known_subtotal_scaled() + 1,
        valid.evm_local_known_subtotal_pgu(),
    );
    assert_eq!(
        ZkGasModel.complete_block_shadow_report(
            &report(tampered_total),
            ResourceSelectors::default(),
            MptSelectors::default(),
        ),
        Err(ZkGasCompletionError::InconsistentKnownBreakdown("evm_local_known_subtotal_scaled"))
    );

    let tampered_ceiling = known(
        valid.fixed_scaled(),
        valid.operations_scaled(),
        valid.feature_error_scaled(),
        valid.evm_local_resources_scaled(),
        valid.evm_local_known_subtotal_scaled(),
        valid.evm_local_known_subtotal_pgu() + 1,
    );
    assert_eq!(
        ZkGasModel.complete_block_shadow_report(
            &report(tampered_ceiling),
            ResourceSelectors::default(),
            MptSelectors::default(),
        ),
        Err(ZkGasCompletionError::InconsistentKnownBreakdown("evm_local_known_subtotal_pgu"))
    );

    let invalid_feature_attribution = known(
        valid.fixed_scaled(),
        valid.operations_scaled(),
        valid.operations_scaled() + 1,
        valid.evm_local_resources_scaled(),
        valid.evm_local_known_subtotal_scaled(),
        valid.evm_local_known_subtotal_pgu(),
    );
    assert_eq!(
        ZkGasModel.complete_block_shadow_report(
            &report(invalid_feature_attribution),
            ResourceSelectors::default(),
            MptSelectors::default()
        ),
        Err(ZkGasCompletionError::InconsistentKnownBreakdown("feature_error_scaled"))
    );

    let overflow = known(i128::MAX, 1, 0, 0, 0, 0);
    assert_eq!(
        ZkGasModel.complete_block_shadow_report(
            &report(overflow),
            ResourceSelectors::default(),
            MptSelectors::default(),
        ),
        Err(ZkGasCompletionError::Pricing(ZkGasError::ArithmeticOverflow))
    );

    assert!(matches!(
        ZkGasModel.complete_block_shadow_report(
            &report(valid),
            ResourceSelectors { unique_account_count: u64::MAX, ..Default::default() },
            MptSelectors::default(),
        ),
        Err(ZkGasCompletionError::Pricing(ZkGasError::SelectorOutOfRange {
            selector: "unique_account_count",
            value: u64::MAX
        }))
    ));

    assert!(matches!(
        ZkGasModel.complete_block_shadow_report(
            &report(valid),
            ResourceSelectors::default(),
            MptSelectors { path_node_visits: u64::MAX, ..Default::default() }
        ),
        Err(ZkGasCompletionError::Pricing(ZkGasError::SelectorOutOfRange {
            selector: "path_node_visits",
            value: u64::MAX
        }))
    ));
}

#[test]
fn zk_gas_shadow_report_preserves_typed_collector_diagnostic() {
    use super::pricing::{TransactionMetadata, ZkGasAccumulator};

    let mut accumulator = ZkGasAccumulator::default();
    accumulator.begin_transaction(TransactionMetadata::default()).expect("transaction starts");
    accumulator
        .observe_step(OperationPricingInput {
            route: OperationRoute::Opcode(0x01),
            interpreter_raw_gas: Some(3),
            native_gas: None,
            model_input: OperationModelInput::StaticRawGas { raw_gas: 3 },
            feature_error: false,
        })
        .expect("known operation prices before the diagnostic");
    let error = accumulator
        .observe_step(OperationPricingInput {
            route: OperationRoute::Opcode(0x0c),
            interpreter_raw_gas: Some(0),
            native_gas: None,
            model_input: OperationModelInput::None,
            feature_error: false,
        })
        .expect_err("undefined opcode has no sealed route");
    accumulator.commit_transaction().expect("diagnostic transaction commits");

    let report = accumulator.committed_snapshot().shadow_report();
    assert_eq!(
        report.status(),
        ZkGasShadowStatus::Diagnostic(ZkGasShadowDiagnostic::Collector(error))
    );
    assert_eq!(report.status().label(), "collector_unknown_route");
}

#[test]
fn zk_gas_pricing_accumulator_commits_only_finished_transactions() {
    use alloy_primitives::{Address, B256, U256};

    use super::pricing::{StateAccess, TransactionMetadata, ZkGasAccumulator};

    let operation = OperationPricingInput {
        route: OperationRoute::Opcode(0x01),
        interpreter_raw_gas: Some(3),
        native_gas: None,
        model_input: OperationModelInput::StaticRawGas { raw_gas: 3 },
        feature_error: false,
    };
    let mut accumulator = ZkGasAccumulator::default();

    accumulator
        .begin_transaction(TransactionMetadata { is_anchor: false, native_value_transfer: true })
        .expect("first transaction starts");
    accumulator.observe_step(operation).expect("operation prices");
    accumulator
        .observe_state_access(StateAccess::Account(Address::with_last_byte(1)))
        .expect("account records");
    accumulator
        .observe_state_access(StateAccess::StorageRead {
            address: Address::with_last_byte(1),
            slot: U256::from(2),
        })
        .expect("slot records");
    accumulator
        .observe_state_access(StateAccess::StorageWrite {
            address: Address::with_last_byte(1),
            slot: U256::from(2),
        })
        .expect("write records");
    accumulator
        .observe_state_access(StateAccess::Code { hash: B256::with_last_byte(3), byte_length: 4 })
        .expect("code records");
    accumulator.commit_transaction().expect("transaction commits");

    accumulator
        .begin_transaction(TransactionMetadata::default())
        .expect("second transaction starts");
    accumulator.observe_step(operation).expect("operation prices");
    accumulator
        .observe_state_access(StateAccess::Account(Address::with_last_byte(9)))
        .expect("account records");
    accumulator.reset_transaction();

    let snapshot = accumulator.committed_snapshot();
    assert_eq!(snapshot.started_non_anchor_transactions, 1);
    assert_eq!(snapshot.native_value_transfers, 1);
    assert_eq!(snapshot.operation_routes.len(), 1);
    assert_eq!(snapshot.resources.unique_account_count, 1);
    assert_eq!(snapshot.resources.unique_storage_slot_access_count, 1);
    assert_eq!(snapshot.resources.unique_storage_slot_write_count, 1);
    assert_eq!(snapshot.resources.unique_code_hash_count, 1);
    assert_eq!(snapshot.resources.unique_code_bytes, 4);
}

#[test]
fn zk_gas_pricing_accumulator_preserves_feature_error_attribution() {
    use super::pricing::{TransactionMetadata, ZkGasAccumulator};

    let mut accumulator = ZkGasAccumulator::default();
    accumulator.begin_transaction(TransactionMetadata::default()).expect("transaction starts");
    accumulator
        .observe_step(OperationPricingInput {
            route: OperationRoute::Opcode(0x01),
            interpreter_raw_gas: Some(3),
            native_gas: None,
            model_input: OperationModelInput::None,
            feature_error: true,
        })
        .expect("feature error prices");
    accumulator.commit_transaction().expect("transaction commits");

    let snapshot = accumulator.committed_snapshot();
    assert_eq!(snapshot.operation_routes.len(), 1);
    assert_eq!(snapshot.operation_routes[0].accepted_absolute_scaled, 0);
    assert!(snapshot.operation_routes[0].feature_error_scaled > 0);
    let ZkGasShadowStatus::Incomplete { known, .. } = snapshot.shadow_report().status() else {
        panic!("runtime snapshot must produce only an incomplete shadow subtotal")
    };
    assert_eq!(known.feature_error_scaled(), snapshot.operation_routes[0].feature_error_scaled);
}

#[test]
fn zk_gas_pricing_resource_overflow_is_diagnostic_and_keeps_committed_prefix_atomic() {
    use alloy_primitives::B256;

    use super::pricing::{StateAccess, TransactionMetadata, ZkGasAccumulator};

    let mut accumulator = ZkGasAccumulator::default();
    accumulator.begin_transaction(TransactionMetadata::default()).expect("first transaction");
    accumulator
        .observe_state_access(StateAccess::Code {
            hash: B256::with_last_byte(1),
            byte_length: u64::MAX,
        })
        .expect("maximum representable code-byte prefix");
    accumulator.commit_transaction().expect("first transaction commits");

    accumulator.begin_transaction(TransactionMetadata::default()).expect("second transaction");
    accumulator
        .observe_state_access(StateAccess::Code { hash: B256::with_last_byte(2), byte_length: 1 })
        .expect("one-byte transaction delta is valid alone");
    accumulator.commit_transaction().expect("overflow is retained as a non-fatal diagnostic");

    let snapshot = accumulator.committed_snapshot();
    assert_eq!(snapshot.diagnostic, Some(ZkGasAccumulatorError::CountOverflow));
    assert_eq!(snapshot.resources.unique_code_hash_count, 1);
    assert_eq!(snapshot.resources.unique_code_bytes, u64::MAX);
}

fn zk_gas_pricing_path_snapshots(
    bytecode: Bytecode,
    gas_limit: u64,
) -> (super::pricing::ZkGasBlockSnapshot, super::pricing::ZkGasBlockSnapshot) {
    let mut plain =
        TaikoEvmFactory.create_evm(db_with_contract(bytecode.clone()), evm_env(TaikoSpecId::UNZEN));
    plain.set_zk_gas_pricing_enabled(true);
    let plain_result = plain.transact(tx_env(gas_limit));
    assert!(
        plain_result.is_ok(),
        "plain pricing execution: {plain_result:?}; meter={:?}",
        plain.meter().map(ZkGasMeter::tx_zk_gas_used)
    );
    plain.commit_transaction_zk_gas_pricing().expect("plain pricing commit");
    let plain = plain.zk_gas_pricing_snapshot().expect("plain pricing snapshot");

    let mut inspected = TaikoEvmFactory.create_evm_with_inspector(
        db_with_contract(bytecode),
        evm_env(TaikoSpecId::UNZEN),
        NoOpInspector,
    );
    inspected.set_zk_gas_pricing_enabled(true);
    let inspected_result = inspected.transact(tx_env(gas_limit));
    assert!(
        inspected_result.is_ok(),
        "inspected pricing execution: {inspected_result:?}; meter={:?}",
        inspected.meter().map(ZkGasMeter::tx_zk_gas_used)
    );
    inspected.commit_transaction_zk_gas_pricing().expect("inspected pricing commit");
    let inspected = inspected.zk_gas_pricing_snapshot().expect("inspected pricing snapshot");
    (plain, inspected)
}

#[test]
fn zk_gas_pricing_collects_without_an_unzen_schedule_on_both_execution_paths() {
    let bytecode = simple_arithmetic_bytecode();
    let mut plain = TaikoEvmFactory
        .create_evm(db_with_contract(bytecode.clone()), evm_env(TaikoSpecId::SHASTA));
    assert!(plain.meter().is_none(), "Shasta must not install the legacy meter");
    plain.set_zk_gas_pricing_enabled(true);
    plain.transact(tx_env(100_000)).expect("plain pricing-only execution");
    plain.commit_transaction_zk_gas_pricing().expect("plain pricing commit");
    let plain = plain.zk_gas_pricing_snapshot().expect("plain collector is active");

    let mut inspected = TaikoEvmFactory.create_evm_with_inspector(
        db_with_contract(bytecode),
        evm_env(TaikoSpecId::SHASTA),
        NoOpInspector,
    );
    assert!(inspected.meter().is_none(), "Shasta must not install the legacy meter");
    inspected.set_zk_gas_pricing_enabled(true);
    inspected.transact(tx_env(100_000)).expect("inspected pricing-only execution");
    inspected.commit_transaction_zk_gas_pricing().expect("inspected pricing commit");
    let inspected = inspected.zk_gas_pricing_snapshot().expect("inspected collector is active");

    assert_eq!(plain, inspected);
    assert!(!plain.operation_routes.is_empty());
}

#[test]
fn zk_gas_pricing_factory_default_is_disabled_until_explicitly_enabled() {
    let mut evm = TaikoEvmFactory
        .create_evm(db_with_contract(simple_arithmetic_bytecode()), evm_env(TaikoSpecId::SHASTA));

    assert!(evm.zk_gas_pricing_snapshot().is_none());
    evm.transact(tx_env(100_000)).expect("ordinary execution remains available");
    assert!(
        evm.zk_gas_pricing_snapshot().is_none(),
        "the factory default must not activate collection implicitly"
    );

    evm.set_zk_gas_pricing_enabled(true);
    evm.transact(tx_env(100_000)).expect("explicit pricing-only execution");
    evm.commit_transaction_zk_gas_pricing().expect("explicit pricing commit");
    assert_eq!(
        evm.zk_gas_pricing_snapshot().expect("explicit collector").started_non_anchor_transactions,
        1
    );
}

#[test]
fn zk_gas_pricing_repeated_enable_and_execution_mode_switch_preserve_committed_prefix() {
    let mut evm = TaikoEvmFactory
        .create_evm(db_with_contract(simple_arithmetic_bytecode()), evm_env(TaikoSpecId::SHASTA));
    evm.set_zk_gas_pricing_enabled(true);
    evm.transact(tx_env(100_000)).expect("first plain transaction");
    evm.commit_transaction_zk_gas_pricing().expect("first commit");

    evm.set_zk_gas_pricing_enabled(true);
    assert_eq!(
        evm.zk_gas_pricing_snapshot().expect("idempotent enable").started_non_anchor_transactions,
        1
    );

    evm.set_inspector_enabled(true);
    assert_eq!(
        evm.zk_gas_pricing_snapshot()
            .expect("collector moved to inspector")
            .started_non_anchor_transactions,
        1
    );
    evm.transact(tx_env(100_000)).expect("inspected transaction");
    evm.commit_transaction_zk_gas_pricing().expect("second commit");
    assert_eq!(
        evm.zk_gas_pricing_snapshot().expect("inspected prefix").started_non_anchor_transactions,
        2
    );

    evm.set_inspector_enabled(false);
    evm.transact(tx_env(100_000)).expect("plain transaction after switch back");
    evm.commit_transaction_zk_gas_pricing().expect("third commit");
    assert_eq!(
        evm.zk_gas_pricing_snapshot()
            .expect("plain prefix restored")
            .started_non_anchor_transactions,
        3
    );
}

#[test]
fn zk_gas_pricing_native_transfer_matches_calibrated_eoa_only_selector() {
    let mut eoa_db = InMemoryDB::default();
    eoa_db.insert_account_info(
        BENCH_CALLER,
        AccountInfo { nonce: 0, balance: BENCH_CALLER_BALANCE, ..Default::default() },
    );
    eoa_db.insert_account_info(
        BENCH_TARGET,
        AccountInfo { balance: BENCH_TARGET_BALANCE, ..Default::default() },
    );
    let mut eoa = TaikoEvmFactory.create_evm(eoa_db, evm_env(TaikoSpecId::SHASTA));
    eoa.set_zk_gas_pricing_enabled(true);
    let mut value_call = tx_env(100_000);
    value_call.value = U256::from(1);
    assert!(eoa.transact(value_call.clone()).expect("EOA transfer executes").result.is_success());
    eoa.commit_transaction_zk_gas_pricing().expect("EOA transfer commits");
    assert_eq!(eoa.zk_gas_pricing_snapshot().expect("EOA snapshot").native_value_transfers, 1);

    let mut contract = TaikoEvmFactory
        .create_evm(db_with_contract(simple_arithmetic_bytecode()), evm_env(TaikoSpecId::SHASTA));
    contract.set_zk_gas_pricing_enabled(true);
    assert!(
        contract.transact(value_call).expect("contract value call executes").result.is_success()
    );
    contract.commit_transaction_zk_gas_pricing().expect("contract call commits");
    assert_eq!(
        contract.zk_gas_pricing_snapshot().expect("contract snapshot").native_value_transfers,
        0,
        "an operation-owning contract call is not the native EOA-transfer marker"
    );
}

#[test]
fn zk_gas_pricing_create_revert_and_halt_never_count_as_native_transfer() {
    let mut create = TaikoEvmFactory
        .create_evm(db_with_contract(simple_arithmetic_bytecode()), evm_env(TaikoSpecId::SHASTA));
    create.set_zk_gas_pricing_enabled(true);
    let create_tx = TxEnv::builder()
        .caller(BENCH_CALLER)
        .kind(TxKind::Create)
        .chain_id(Some(167))
        .gas_limit(100_000)
        .value(U256::from(1))
        .data(Bytes::from(vec![opcode::STOP]))
        .build()
        .unwrap();
    assert!(create.transact(create_tx).expect("create executes").result.is_success());
    create.commit_transaction_zk_gas_pricing().expect("create commits");
    assert_eq!(
        create.zk_gas_pricing_snapshot().expect("create snapshot").native_value_transfers,
        0
    );

    for (bytecode, expected, label) in [
        (
            Bytecode::new_raw(Bytes::from(vec![opcode::PUSH0, opcode::PUSH0, opcode::REVERT])),
            "revert",
            "revert",
        ),
        (stack_underflow_bytecode(), "halt", "halt"),
    ] {
        let mut evm =
            TaikoEvmFactory.create_evm(db_with_contract(bytecode), evm_env(TaikoSpecId::SHASTA));
        evm.set_zk_gas_pricing_enabled(true);
        let mut tx = tx_env(100_000);
        tx.value = U256::from(1);
        let result = evm.transact(tx).expect("valid failed execution is included");
        assert!(
            matches!(
                (&result.result, expected),
                (ExecutionResult::Revert { .. }, "revert") | (ExecutionResult::Halt { .. }, "halt")
            ),
            "{label}: {:?}",
            result.result
        );
        evm.commit_transaction_zk_gas_pricing().expect("failed execution commits workload");
        assert_eq!(
            evm.zk_gas_pricing_snapshot().expect("failed snapshot").native_value_transfers,
            0,
            "{label}"
        );
    }
}

#[test]
fn zk_gas_pricing_does_not_collect_pre_block_system_calls() {
    let mut evm = TaikoEvmFactory
        .create_evm(db_with_contract(simple_arithmetic_bytecode()), evm_env(TaikoSpecId::UNZEN));
    evm.set_zk_gas_pricing_enabled(true);
    evm.transact_system_call(BENCH_CALLER, BENCH_TARGET, Bytes::new())
        .expect("system call executes");
    assert_eq!(
        evm.zk_gas_pricing_snapshot().expect("explicit collector remains installed"),
        super::pricing::ZkGasBlockSnapshot::default(),
        "system call must not collect into pricing"
    );
}

#[test]
fn zk_gas_pricing_feature_error_halt_is_included_and_matches_both_paths() {
    let (plain, inspected) = zk_gas_pricing_path_snapshots(
        Bytecode::new_raw(Bytes::from(vec![opcode::RETURN])),
        100_000,
    );
    assert_eq!(plain, inspected);
    assert_eq!(plain.started_non_anchor_transactions, 1);
    assert!(plain.operation_routes.iter().any(|row| {
        row.route == OperationRoute::Opcode(opcode::RETURN) && row.feature_error_scaled > 0
    }));
}

#[test]
fn zk_gas_pricing_log0_uses_the_generated_linear_input_on_both_paths() {
    let (plain, inspected) = zk_gas_pricing_path_snapshots(
        Bytecode::new_raw(Bytes::from(vec![opcode::PUSH0, opcode::PUSH0, opcode::LOG0])),
        100_000,
    );

    assert_eq!(plain, inspected);
    assert_eq!(plain.diagnostic, None);
    assert!(plain.operation_routes.iter().any(|row| {
        row.route == OperationRoute::Opcode(opcode::LOG0) && row.accepted_absolute_scaled > 0
    }));
}

#[test]
fn zk_gas_pricing_dirty_sstore_noop_is_priced_as_a_warm_noop() {
    let (plain, inspected) = zk_gas_pricing_path_snapshots(
        Bytecode::new_raw(Bytes::from(vec![
            opcode::PUSH1,
            1,
            opcode::PUSH0,
            opcode::SSTORE,
            opcode::PUSH1,
            1,
            opcode::PUSH0,
            opcode::SSTORE,
            opcode::STOP,
        ])),
        100_000,
    );

    assert_eq!(plain, inspected);
    assert_eq!(plain.diagnostic, None);
    let sstore = plain
        .operation_routes
        .iter()
        .find(|row| row.route == OperationRoute::Opcode(opcode::SSTORE))
        .expect("SSTORE route is collected");
    assert_eq!(sstore.feature_error_scaled, 0);
}

#[test]
fn zk_gas_pricing_known_halted_opcode_uses_feature_error_on_both_paths() {
    let (plain, inspected) = zk_gas_pricing_path_snapshots(
        Bytecode::new_raw(Bytes::from(vec![
            opcode::PUSH0,
            opcode::PUSH0,
            opcode::PUSH0,
            opcode::LOG1,
            opcode::STOP,
        ])),
        21_500,
    );

    assert_eq!(plain, inspected);
    assert_eq!(plain.diagnostic, None);
    assert!(plain.operation_routes.iter().any(|row| {
        row.route == OperationRoute::Opcode(opcode::LOG1) && row.feature_error_scaled > 0
    }));
}

#[test]
fn zk_gas_pricing_revert_memory_expansion_uses_feature_error_on_both_paths() {
    let (plain, inspected) = zk_gas_pricing_path_snapshots(
        Bytecode::new_raw(Bytes::from(vec![
            opcode::PUSH1,
            0x01,
            opcode::PUSH1,
            0x40,
            opcode::REVERT,
        ])),
        100_000,
    );

    assert_eq!(plain, inspected);
    assert_eq!(plain.diagnostic, None);
    assert!(plain.operation_routes.iter().any(|row| {
        row.route == OperationRoute::Opcode(opcode::REVERT) && row.feature_error_scaled > 0
    }));
}

#[test]
fn zk_gas_pricing_zero_length_revert_uses_accepted_route_on_both_paths() {
    let (plain, inspected) = zk_gas_pricing_path_snapshots(
        Bytecode::new_raw(Bytes::from(vec![opcode::PUSH0, opcode::PUSH0, opcode::REVERT])),
        100_000,
    );

    assert_eq!(plain, inspected);
    assert_eq!(plain.diagnostic, None);
    let revert = plain
        .operation_routes
        .iter()
        .find(|row| row.route == OperationRoute::Opcode(opcode::REVERT))
        .expect("REVERT route is collected");
    assert!(revert.accepted_absolute_scaled > 0);
    assert_eq!(revert.feature_error_scaled, 0);
}

#[test]
fn zk_gas_pricing_commit_without_local_transaction_is_a_retained_diagnostic() {
    use super::pricing::ZkGasAccumulator;

    let mut accumulator = ZkGasAccumulator::default();
    accumulator.commit_transaction().expect("cross-executor commit must not panic");
    assert_eq!(
        accumulator.committed_snapshot().diagnostic,
        Some(ZkGasAccumulatorError::NoActiveTransaction)
    );
    assert_eq!(
        accumulator.committed_snapshot().shadow_report().status().label(),
        "collector_no_active_transaction"
    );
}

#[test]
fn zk_gas_pricing_unknown_route_is_a_shadow_diagnostic_not_an_evm_error() {
    let mut evm = TaikoEvmFactory.create_evm(
        db_with_contract(Bytecode::new_raw(Bytes::from(vec![0x0c]))),
        evm_env(TaikoSpecId::UNZEN),
    );
    evm.set_zk_gas_pricing_enabled(true);
    let result =
        evm.transact(tx_env(100_000)).expect("pricing diagnostic must not alter execution");
    assert!(matches!(result.result, ExecutionResult::Halt { .. }));
    evm.commit_transaction_zk_gas_pricing().expect("diagnostic transaction still commits");
    assert_eq!(
        evm.zk_gas_pricing_snapshot().expect("snapshot").diagnostic,
        Some(ZkGasAccumulatorError::Pricing(ZkGasError::UnknownRoute(OperationRoute::Opcode(
            0x0c
        ),)))
    );
}

#[test]
fn zk_gas_pricing_included_halt_commits_executed_workload() {
    let mut evm = TaikoEvmFactory
        .create_evm(db_with_contract(stack_underflow_bytecode()), evm_env(TaikoSpecId::UNZEN));
    evm.set_zk_gas_pricing_enabled(true);
    let result = evm.transact(tx_env(100_000)).expect("valid halting transaction is included");
    assert!(matches!(result.result, ExecutionResult::Halt { .. }));
    evm.commit_transaction_zk_gas_pricing().expect("included halt commits pricing");
    let snapshot = evm.zk_gas_pricing_snapshot().expect("snapshot");
    assert_eq!(snapshot.started_non_anchor_transactions, 1);
    assert!(
        snapshot
            .operation_routes
            .iter()
            .any(|row| row.route == OperationRoute::Opcode(opcode::ADD))
    );
}

#[test]
fn zk_gas_pricing_discarded_transactions_do_not_leak_into_block_snapshot() {
    macro_rules! assert_reset {
        ($evm:expr) => {{
            let mut evm = $evm;
            evm.reset_transaction_zk_gas();
            assert_eq!(
                evm.zk_gas_pricing_snapshot().expect("collector remains enabled"),
                super::pricing::ZkGasBlockSnapshot::default()
            );
        }};
    }

    // Invalid nonce is rejected before inclusion.
    let mut invalid_nonce = TaikoEvmFactory
        .create_evm(db_with_contract(simple_arithmetic_bytecode()), evm_env(TaikoSpecId::UNZEN));
    invalid_nonce.set_zk_gas_pricing_enabled(true);
    let mut tx = tx_env(100_000);
    tx.nonce = 1;
    assert!(invalid_nonce.transact(tx).is_err());
    assert_reset!(invalid_nonce);

    // Insufficient balance is likewise a discarded validation failure.
    let mut db = db_with_contract(simple_arithmetic_bytecode());
    db.insert_account_info(BENCH_CALLER, AccountInfo::default());
    let mut insufficient = TaikoEvmFactory.create_evm(db, evm_env(TaikoSpecId::UNZEN));
    insufficient.set_zk_gas_pricing_enabled(true);
    let mut tx = tx_env(100_000);
    tx.value = U256::from(1);
    assert!(insufficient.transact(tx).is_err());
    assert_reset!(insufficient);

    // Legacy zk-gas rejection omits the transaction and resets the pricing shadow scope too.
    let mut rejected = TaikoEvmFactory.create_evm(
        db_with_contract(limit_exceeding_keccak_bytecode()),
        evm_env(TaikoSpecId::UNZEN),
    );
    rejected.set_zk_gas_pricing_enabled(true);
    assert_eq!(rejected.transact(tx_env(5_000_000)).unwrap_err().to_string(), ZK_GAS_LIMIT_ERR);
    assert_reset!(rejected);

    // A successfully executed transaction rejected by a commit predicate follows the same reset.
    let mut commit_false = TaikoEvmFactory
        .create_evm(db_with_contract(simple_arithmetic_bytecode()), evm_env(TaikoSpecId::UNZEN));
    commit_false.set_zk_gas_pricing_enabled(true);
    commit_false.transact(tx_env(100_000)).expect("execution succeeds before predicate");
    assert_reset!(commit_false);

    // Skipped/unattempted work has no transaction scope to leak.
    let mut skipped = TaikoEvmFactory
        .create_evm(db_with_contract(simple_arithmetic_bytecode()), evm_env(TaikoSpecId::UNZEN));
    skipped.set_zk_gas_pricing_enabled(true);
    assert_eq!(
        skipped.zk_gas_pricing_snapshot().expect("explicit collector"),
        super::pricing::ZkGasBlockSnapshot::default()
    );
}

#[test]
fn zk_gas_pricing_plain_and_inspected_paths_match_structured_opcode_selectors() {
    let bytecode = Bytecode::new_raw(Bytes::from(vec![
        opcode::PUSH1,
        0x02,
        opcode::PUSH1,
        0x01,
        opcode::ADD,
        opcode::POP,
        opcode::PUSH1,
        0x05,
        opcode::PUSH1,
        0x02,
        opcode::EXP,
        opcode::POP,
        opcode::PUSH1,
        0x21,
        opcode::PUSH1,
        0x00,
        opcode::KECCAK256,
        opcode::POP,
        opcode::PUSH1,
        0x01,
        opcode::PUSH1,
        0x40,
        opcode::MSTORE,
        opcode::PUSH1,
        0x21,
        opcode::PUSH1,
        0x00,
        opcode::PUSH1,
        0x80,
        opcode::MCOPY,
        opcode::PUSH1,
        0x21,
        opcode::PUSH1,
        0x00,
        opcode::PUSH2,
        0x01,
        0x00,
        opcode::CALLDATACOPY,
        opcode::ADDRESS,
        opcode::POP,
        opcode::CALLER,
        opcode::POP,
        opcode::CALLVALUE,
        opcode::POP,
        opcode::PUSH0,
        opcode::CALLDATALOAD,
        opcode::POP,
        opcode::CALLDATASIZE,
        opcode::POP,
        opcode::TIMESTAMP,
        opcode::POP,
        opcode::PUSH1,
        0x01,
        opcode::PUSH1,
        0x21,
        opcode::PUSH1,
        0x00,
        opcode::LOG1,
        opcode::STOP,
    ]));

    let (plain, inspected) = zk_gas_pricing_path_snapshots(bytecode, 2_000_000);
    assert_eq!(plain, inspected);
    assert!(plain.operation_routes.len() >= 12);
    assert!(plain.resources.unique_code_hash_count > 0);
}

#[test]
fn zk_gas_pricing_successful_storage_models_match_between_paths() {
    let bytecode = Bytecode::new_raw(Bytes::from(vec![
        opcode::PUSH1,
        0x01,
        opcode::PUSH0,
        opcode::SSTORE,
        opcode::PUSH0,
        opcode::SLOAD,
        opcode::POP,
        opcode::STOP,
    ]));
    let (plain, inspected) = zk_gas_pricing_path_snapshots(bytecode, 200_000);
    assert_eq!(plain, inspected);
    for opcode in [opcode::SSTORE, opcode::SLOAD] {
        assert!(
            plain.operation_routes.iter().any(|row| row.route == OperationRoute::Opcode(opcode))
        );
    }
    assert_eq!(plain.resources.unique_storage_slot_access_count, 1);
    assert_eq!(plain.resources.unique_storage_slot_write_count, 1);
}

#[test]
fn zk_gas_pricing_return_boundaries_match_between_plain_and_inspected_paths() {
    for returned_bytes in [0_u64, 1, 31, 32, 33, 4_096, 65_536] {
        let mut code = push_u64(returned_bytes);
        code.push(opcode::PUSH0);
        code.push(opcode::RETURN);
        let (plain, inspected) =
            zk_gas_pricing_path_snapshots(Bytecode::new_raw(Bytes::from(code)), 2_000_000);
        assert_eq!(plain, inspected, "RETURN length {returned_bytes}");
        assert!(plain.operation_routes.iter().any(|row| row.route == OperationRoute::Opcode(0xf3)));
    }
}

#[test]
fn zk_gas_pricing_return_records_one_4k_boundary_event_per_expanding_opcode() {
    let returned_bytes = 65_536_u64;
    let mut code = push_u64(returned_bytes);
    code.push(opcode::PUSH0);
    code.push(opcode::RETURN);
    let (snapshot, inspected) =
        zk_gas_pricing_path_snapshots(Bytecode::new_raw(Bytes::from(code)), 2_000_000);
    assert_eq!(snapshot, inspected);
    let actual = snapshot
        .operation_routes
        .iter()
        .find(|row| row.route == OperationRoute::Opcode(opcode::RETURN))
        .expect("RETURN route");
    let expected = ZkGasModel
        .price_operation(&OperationPricingInput {
            route: OperationRoute::Opcode(opcode::RETURN),
            interpreter_raw_gas: Some(14_336),
            native_gas: None,
            model_input: OperationModelInput::Return {
                returned_bytes,
                return_words: 2_048,
                memory: MemorySelectors {
                    growth_events: 1,
                    evm_gas_delta: 14_336,
                    boundary_4k_events: 1,
                },
            },
            feature_error: false,
        })
        .expect("sealed RETURN input prices");
    assert_eq!(actual.accepted_absolute_scaled, expected.accepted_absolute_scaled);
    assert_eq!(actual.incremental_scaled, expected.incremental_scaled);
    assert_eq!(actual.structural_scaled, expected.structural_scaled);
}

#[test]
fn zk_gas_pricing_failed_memory_expansion_uses_feature_error_without_phantom_boundaries() {
    let bytecode = Bytecode::new_raw(Bytes::from(vec![
        opcode::PUSH1,
        0x01,
        opcode::PUSH8,
        0xff,
        0xff,
        0xff,
        0xff,
        0xff,
        0xff,
        0xff,
        0xff,
        opcode::RETURN,
    ]));
    let (plain, inspected) = zk_gas_pricing_path_snapshots(bytecode, 100_000);
    assert_eq!(plain, inspected);
    let row = plain
        .operation_routes
        .iter()
        .find(|row| row.route == OperationRoute::Opcode(opcode::RETURN))
        .expect("RETURN route");
    assert_eq!(row.accepted_absolute_scaled, 0);
    assert_eq!(row.incremental_scaled, 0);
    assert_eq!(row.structural_scaled, 0);
    assert!(row.feature_error_scaled > 0);
}

#[test]
fn zk_gas_pricing_bn254_scalar_selector_is_reduced_before_bit_work() {
    let order = U256::from_be_bytes(alloy_primitives::hex!(
        "30644e72e131a029b85045b68181585d2833e84879b9709143e1f593f0000001"
    ));
    let mut input = [0_u8; 96];
    input[64..].copy_from_slice(&(order + U256::from(1)).to_be_bytes::<32>());
    assert_eq!(super::runtime::bn254_work_units(&input), 2, "reduced scalar is one");
    input[64..].fill(0);
    assert_eq!(super::runtime::bn254_work_units(&input), 0);
}

#[test]
fn zk_gas_pricing_precompile_address_0x100_does_not_alias_zero() {
    use super::pricing::{TransactionMetadata, ZkGasAccumulator};

    let mut accumulator = ZkGasAccumulator::default();
    accumulator.begin_transaction(TransactionMetadata::default()).unwrap();
    accumulator.observe_precompile(0x100, 1, None).expect("0x100 route is explicit");
    accumulator.commit_transaction().expect("commit");
    let snapshot = accumulator.committed_snapshot();
    assert!(
        snapshot.operation_routes.iter().any(|row| row.route == OperationRoute::Precompile(0x100))
    );
    assert!(
        snapshot.operation_routes.iter().all(|row| row.route != OperationRoute::Precompile(0x00))
    );
}

#[test]
fn disabled_mode_skips_bn254_selector_capture_on_both_execution_paths() {
    for inspected in [false, true] {
        super::runtime::reset_bn254_work_unit_observations();
        let bytecode = staticcall_precompile_bytecode(0x07);
        if inspected {
            let mut evm = TaikoEvmFactory.create_evm_with_inspector(
                db_with_contract(bytecode),
                evm_env(TaikoSpecId::UNZEN),
                NoOpInspector,
            );
            evm.transact(tx_env(100_000)).expect("disabled inspected precompile executes");
        } else {
            let mut evm =
                TaikoEvmFactory.create_evm(db_with_contract(bytecode), evm_env(TaikoSpecId::UNZEN));
            evm.transact(tx_env(100_000)).expect("disabled plain precompile executes");
        }
        assert_eq!(
            super::runtime::take_bn254_work_unit_observations(),
            0,
            "Disabled must not parse the BN254 selector"
        );

        super::runtime::reset_bn254_work_unit_observations();
        let bytecode = staticcall_precompile_bytecode(0x07);
        if inspected {
            let mut evm = TaikoEvmFactory.create_evm_with_inspector(
                db_with_contract(bytecode),
                evm_env(TaikoSpecId::UNZEN),
                NoOpInspector,
            );
            evm.set_zk_gas_pricing_enabled(true);
            evm.transact(tx_env(100_000)).expect("shadow inspected precompile executes");
        } else {
            let mut evm =
                TaikoEvmFactory.create_evm(db_with_contract(bytecode), evm_env(TaikoSpecId::UNZEN));
            evm.set_zk_gas_pricing_enabled(true);
            evm.transact(tx_env(100_000)).expect("shadow plain precompile executes");
        }
        assert_eq!(
            super::runtime::take_bn254_work_unit_observations(),
            1,
            "Shadow must parse the BN254 selector exactly once"
        );
    }
}

#[test]
fn disabled_and_shadow_are_execution_equivalent_on_plain_and_inspected_paths() {
    let cases = [
        ("arithmetic", simple_arithmetic_bytecode()),
        (
            "log0",
            Bytecode::new_raw(Bytes::from(vec![
                opcode::PUSH0,
                opcode::PUSH0,
                opcode::LOG0,
                opcode::STOP,
            ])),
        ),
        (
            "dirty-sstore-noop",
            Bytecode::new_raw(Bytes::from(vec![
                opcode::PUSH1,
                1,
                opcode::PUSH0,
                opcode::SSTORE,
                opcode::PUSH1,
                1,
                opcode::PUSH0,
                opcode::SSTORE,
                opcode::STOP,
            ])),
        ),
        ("precompile", staticcall_identity_bytecode()),
    ];

    for spec in [TaikoSpecId::SHASTA, TaikoSpecId::UNZEN] {
        for (label, bytecode) in &cases {
            let mut plain_disabled =
                TaikoEvmFactory.create_evm(db_with_contract(bytecode.clone()), evm_env(spec));
            let plain_disabled_result = plain_disabled
                .transact(tx_env(200_000))
                .unwrap_or_else(|error| panic!("{spec:?} {label} plain Disabled: {error}"));
            let plain_disabled_legacy = plain_disabled.meter().map(ZkGasMeter::tx_zk_gas_used);

            let mut plain_shadow =
                TaikoEvmFactory.create_evm(db_with_contract(bytecode.clone()), evm_env(spec));
            plain_shadow.set_zk_gas_pricing_enabled(true);
            let plain_shadow_result = plain_shadow
                .transact(tx_env(200_000))
                .unwrap_or_else(|error| panic!("{spec:?} {label} plain Shadow: {error}"));
            let plain_shadow_legacy = plain_shadow.meter().map(ZkGasMeter::tx_zk_gas_used);

            let mut inspected_disabled = TaikoEvmFactory.create_evm_with_inspector(
                db_with_contract(bytecode.clone()),
                evm_env(spec),
                NoOpInspector,
            );
            let inspected_disabled_result = inspected_disabled
                .transact(tx_env(200_000))
                .unwrap_or_else(|error| panic!("{spec:?} {label} inspected Disabled: {error}"));
            let inspected_disabled_legacy =
                inspected_disabled.meter().map(ZkGasMeter::tx_zk_gas_used);

            let mut inspected_shadow = TaikoEvmFactory.create_evm_with_inspector(
                db_with_contract(bytecode.clone()),
                evm_env(spec),
                NoOpInspector,
            );
            inspected_shadow.set_zk_gas_pricing_enabled(true);
            let inspected_shadow_result = inspected_shadow
                .transact(tx_env(200_000))
                .unwrap_or_else(|error| panic!("{spec:?} {label} inspected Shadow: {error}"));
            let inspected_shadow_legacy = inspected_shadow.meter().map(ZkGasMeter::tx_zk_gas_used);

            assert_eq!(plain_shadow_result, plain_disabled_result, "{spec:?} {label} plain");
            assert_eq!(
                inspected_shadow_result, inspected_disabled_result,
                "{spec:?} {label} inspected"
            );
            assert_eq!(
                inspected_disabled_result, plain_disabled_result,
                "{spec:?} {label} execution path"
            );
            assert_eq!(plain_shadow_legacy, plain_disabled_legacy, "{spec:?} {label} plain meter");
            assert_eq!(
                inspected_shadow_legacy, inspected_disabled_legacy,
                "{spec:?} {label} inspected meter"
            );
            assert_eq!(
                inspected_disabled_legacy, plain_disabled_legacy,
                "{spec:?} {label} path meter"
            );
        }
    }
}

#[test]
fn pre_unzen_shadow_preserves_out_of_gas_on_plain_and_inspected_paths() {
    let bytecode = simple_arithmetic_bytecode();
    let gas_limit = 21_007;

    let mut plain_disabled = TaikoEvmFactory
        .create_evm(db_with_contract(bytecode.clone()), evm_env(TaikoSpecId::SHASTA));
    let plain_disabled_result =
        plain_disabled.transact(tx_env(gas_limit)).expect("plain Disabled execution");
    assert!(plain_disabled.meter().is_none(), "Shasta must not install the Unzen meter");

    let mut plain_shadow = TaikoEvmFactory
        .create_evm(db_with_contract(bytecode.clone()), evm_env(TaikoSpecId::SHASTA));
    plain_shadow.set_zk_gas_pricing_enabled(true);
    let plain_shadow_result =
        plain_shadow.transact(tx_env(gas_limit)).expect("plain Shadow execution");
    assert!(plain_shadow.meter().is_none(), "Shasta must not install the Unzen meter");

    let mut inspected_disabled = TaikoEvmFactory.create_evm_with_inspector(
        db_with_contract(bytecode.clone()),
        evm_env(TaikoSpecId::SHASTA),
        NoOpInspector,
    );
    let inspected_disabled_result =
        inspected_disabled.transact(tx_env(gas_limit)).expect("inspected Disabled execution");

    let mut inspected_shadow = TaikoEvmFactory.create_evm_with_inspector(
        db_with_contract(bytecode),
        evm_env(TaikoSpecId::SHASTA),
        NoOpInspector,
    );
    inspected_shadow.set_zk_gas_pricing_enabled(true);
    let inspected_shadow_result =
        inspected_shadow.transact(tx_env(gas_limit)).expect("inspected Shadow execution");

    assert!(matches!(
        plain_disabled_result.result,
        ExecutionResult::Halt { reason: HaltReason::OutOfGas(_), .. }
    ));
    assert_eq!(plain_shadow_result, plain_disabled_result);
    assert_eq!(inspected_disabled_result, plain_disabled_result);
    assert_eq!(inspected_shadow_result, plain_disabled_result);
}

fn push_u64(value: u64) -> Vec<u8> {
    if value == 0 {
        return vec![opcode::PUSH0];
    }
    let bytes = value.to_be_bytes();
    let first = bytes.iter().position(|byte| *byte != 0).expect("nonzero value");
    let significant = &bytes[first..];
    let mut output = Vec::with_capacity(significant.len() + 1);
    output.push(0x5f + significant.len() as u8);
    output.extend_from_slice(significant);
    output
}

const EXPECTED_SPAWN_OPCODES: [u8; 6] = [0xf0, 0xf1, 0xf2, 0xf4, 0xf5, 0xfa];

#[test]
fn zk_gas_model_pins_the_sealed_runtime_spec() {
    assert_eq!(
        RUNTIME_SPEC_SHA256,
        "93d04eddda3779393807808859eb36561d35aad0cdd8a8a3960ec3e279a93046"
    );
    assert_eq!(FIXED_POINT_SCALE, 1_000_000_000_000_000_000);
    assert_eq!(ZkGasModel.route_count(), 174);
}

fn assert_zk_gas_pricing_operation(
    input: OperationPricingInput,
    accepted_absolute_scaled: i128,
    incremental_scaled: i128,
    structural_scaled: i128,
    total_scaled: i128,
) {
    let priced = ZkGasModel
        .price_operation(&input)
        .unwrap_or_else(|error| panic!("golden vector {:?} failed: {error}", input.route));
    assert_eq!(priced.accepted_absolute_scaled, accepted_absolute_scaled);
    assert_eq!(priced.incremental_scaled, incremental_scaled);
    assert_eq!(priced.structural_scaled, structural_scaled);
    assert_eq!(priced.feature_error_scaled, 0);
    assert_eq!(priced.total_scaled, total_scaled);
    assert_eq!(priced.total_pgu, None, "operation scope is not a rounding boundary");
}

#[test]
fn zk_gas_model_matches_python_operation_golden_vectors() {
    let memory = MemorySelectors { growth_events: 1, evm_gas_delta: 5, boundary_4k_events: 1 };
    let opcode = |opcode, raw_gas, model_input| OperationPricingInput {
        route: OperationRoute::Opcode(opcode),
        interpreter_raw_gas: Some(raw_gas),
        native_gas: None,
        model_input,
        feature_error: false,
    };

    let vectors = [
        (
            opcode(0x3d, 5, OperationModelInput::StaticRawGas { raw_gas: 5 }),
            524_974_884_691_486_860_714,
            0,
            0,
            524_974_884_691_486_860_714,
        ),
        (
            opcode(0x01, 3, OperationModelInput::StaticRawGas { raw_gas: 3 }),
            42_682_295_930_216_867_992,
            0,
            0,
            42_682_295_930_216_867_992,
        ),
        (
            opcode(0x0a, 260, OperationModelInput::Exp { exponent_byte_length: 5 }),
            3_534_207_877_640_930_240_549,
            0,
            0,
            3_534_207_877_640_930_240_549,
        ),
        (
            opcode(0x20, 42, OperationModelInput::Keccak { input_length: 1, memory }),
            4_925_105_575_222_873_875_997,
            0,
            0,
            4_925_105_575_222_873_875_997,
        ),
        (
            opcode(0x51, 8, OperationModelInput::MemoryAccess { memory }),
            2_366_352_149_092_294_877_149,
            0,
            0,
            2_366_352_149_092_294_877_149,
        ),
        (
            opcode(0x5e, 10, OperationModelInput::MemoryCopy { copy_words: 2, memory }),
            2_216_774_129_912_161_426_117,
            0,
            0,
            2_216_774_129_912_161_426_117,
        ),
        (
            opcode(0x37, 14, OperationModelInput::Copy { copied_bytes: 33, copy_words: 2, memory }),
            3_264_846_279_697_633_398_160,
            84_000_000_000_000_000_000,
            0,
            3_264_846_279_697_633_398_160,
        ),
        (
            opcode(
                0xa1,
                1_019,
                OperationModelInput::Log { topic_count: 1, data_bytes: 33, data_words: 2, memory },
            ),
            30_784_846_279_697_633_398_160,
            24_456_000_000_000_000_000_000,
            0,
            30_784_846_279_697_633_398_160,
        ),
        (
            opcode(
                0xf3,
                5,
                OperationModelInput::Return { returned_bytes: 33, return_words: 2, memory },
            ),
            7_296_846_279_697_633_398_160,
            0,
            0,
            7_296_846_279_697_633_398_160,
        ),
        (
            opcode(
                0xf3,
                5,
                OperationModelInput::Return { returned_bytes: 65_537, return_words: 2_049, memory },
            ),
            276_481_692_559_395_266_796_320,
            0,
            0,
            276_481_692_559_395_266_796_320,
        ),
        (
            opcode(0x40, 20, OperationModelInput::None),
            32_117_684_584_888_432_025,
            0,
            0,
            32_117_684_584_888_432_025,
        ),
        (
            opcode(
                0xa3,
                1_769,
                OperationModelInput::Log { topic_count: 3, data_bytes: 33, data_words: 2, memory },
            ),
            5_002_099_871_395_006_146_537_446,
            42_456_000_000_000_000_000_000,
            0,
            5_002_099_871_395_006_146_537_446,
        ),
        (
            opcode(0x31, 100, OperationModelInput::None),
            33_750_032_583_812_999_927_600,
            800_000_000_000_000_000_000,
            0,
            33_750_032_583_812_999_927_600,
        ),
        (
            opcode(0x54, 100, OperationModelInput::StorageLoad { access: AccessClass::Warm }),
            3_957_515_207_272_132_022_005,
            0,
            0,
            3_957_515_207_272_132_022_005,
        ),
        (
            opcode(0x54, 2_100, OperationModelInput::StorageLoad { access: AccessClass::Cold }),
            3_336_318_298_669_981_484_371,
            0,
            0,
            3_336_318_298_669_981_484_371,
        ),
        (
            opcode(
                0x55,
                22_100,
                OperationModelInput::StorageStore {
                    access: AccessClass::Cold,
                    branch: StorageBranch::Set,
                },
            ),
            4_032_887_156_573_945_116_245,
            265_200_000_000_000_000_000_000,
            0,
            265_200_000_000_000_000_000_000,
        ),
        (
            opcode(
                0x35,
                3,
                OperationModelInput::CalldataLoad { access: CalldataAccessClass::Partial },
            ),
            239_484_054_712_294_439_156,
            0,
            0,
            239_484_054_712_294_439_156,
        ),
    ];

    for (input, accepted, incremental, structural, total) in vectors {
        assert_zk_gas_pricing_operation(input, accepted, incremental, structural, total);
    }

    assert_zk_gas_pricing_operation(
        OperationPricingInput {
            route: OperationRoute::ConfirmedSpawn(0xf0),
            interpreter_raw_gas: None,
            native_gas: None,
            model_input: OperationModelInput::None,
            feature_error: false,
        },
        3_121_878_014_002_702_493_301_969,
        44_733_000_000_000_000_000_000,
        0,
        3_121_878_014_002_702_493_301_969,
    );
    assert_zk_gas_pricing_operation(
        OperationPricingInput {
            route: OperationRoute::Precompile(0x01),
            interpreter_raw_gas: None,
            native_gas: Some(3_000),
            model_input: OperationModelInput::None,
            feature_error: false,
        },
        20_000_000_000_000_000_000_000_000,
        60_000_000_000_000_000_000_000,
        117_000_000_000_000_000_000_000,
        20_000_000_000_000_000_000_000_000,
    );
    assert_zk_gas_pricing_operation(
        OperationPricingInput {
            route: OperationRoute::Precompile(0x04),
            interpreter_raw_gas: None,
            native_gas: Some(144),
            model_input: OperationModelInput::None,
            feature_error: false,
        },
        72_900_070_381_036_079_843_616,
        3_744_000_000_000_000_000_000,
        7_344_000_000_000_000_000_000,
        72_900_070_381_036_079_843_616,
    );
    assert_zk_gas_pricing_operation(
        OperationPricingInput {
            route: OperationRoute::Precompile(0x07),
            interpreter_raw_gas: None,
            native_gas: Some(6_000),
            model_input: OperationModelInput::WorkUnits { work_units: 254 },
            feature_error: false,
        },
        20_000_000_000_000_000_000_000_000,
        179_406_000_000_000_000_000_000_000,
        145_525_744_000_000_000_000_000_000,
        179_406_000_000_000_000_000_000_000,
    );
}

#[test]
fn zk_gas_model_uses_the_explicit_feature_error_floor() {
    let priced = ZkGasModel
        .price_operation(&OperationPricingInput {
            route: OperationRoute::Opcode(0x01),
            interpreter_raw_gas: Some(3),
            native_gas: None,
            model_input: OperationModelInput::None,
            feature_error: true,
        })
        .expect("feature error prices");
    assert_eq!(priced.accepted_absolute_scaled, 0);
    assert_eq!(priced.incremental_scaled, 0);
    assert_eq!(priced.structural_scaled, 0);
    assert_eq!(priced.feature_error_scaled, 5_000_000_000_000_000_000_000_000);
    assert_eq!(priced.total_scaled, priced.feature_error_scaled);

    let accumulated = [OperationRoutePricingInput {
        route: OperationRoute::Opcode(0x01),
        accepted_absolute_scaled: priced.accepted_absolute_scaled,
        incremental_scaled: priced.incremental_scaled,
        structural_scaled: priced.structural_scaled,
        feature_error_scaled: priced.feature_error_scaled,
    }];
    let block = ZkGasModel
        .price_block(&BlockPricingInput {
            started_non_anchor_transactions: 0,
            native_value_transfers: 0,
            operation_routes: &accumulated,
            resources: ResourceSelectors::default(),
            mpt: MptSelectors::default(),
        })
        .expect("feature error accumulates into block pricing");
    assert_eq!(block.feature_error_scaled, priced.feature_error_scaled);
    assert_eq!(block.operations_scaled, priced.feature_error_scaled);
}

#[test]
fn zk_gas_model_matches_python_block_breakdown_and_single_ceiling() {
    let operation = OperationPricingInput {
        route: OperationRoute::Opcode(0x01),
        interpreter_raw_gas: Some(3),
        native_gas: None,
        model_input: OperationModelInput::StaticRawGas { raw_gas: 3 },
        feature_error: false,
    };
    let operation = ZkGasModel.price_operation(&operation).expect("operation prices");
    let operation_route = OperationRoutePricingInput {
        route: OperationRoute::Opcode(0x01),
        accepted_absolute_scaled: operation.accepted_absolute_scaled,
        incremental_scaled: operation.incremental_scaled,
        structural_scaled: operation.structural_scaled,
        feature_error_scaled: 0,
    };
    let priced = ZkGasModel
        .price_block(&BlockPricingInput {
            started_non_anchor_transactions: 2,
            native_value_transfers: 1,
            operation_routes: &[operation_route],
            resources: ResourceSelectors {
                unique_account_count: 1,
                unique_storage_slot_access_count: 2,
                unique_storage_slot_write_count: 3,
                unique_code_hash_count: 4,
                unique_code_bytes: 5,
            },
            mpt: MptSelectors {
                path_node_visits: 1,
                materialization_work_units: 2,
                hydration_hash_blocks: 3,
                dirty_encoded_nodes: 4,
                dirty_hash_blocks: 5,
                structural_branch_extension_churn_nodes: 6,
                dropped_trie_nodes: 7,
            },
        })
        .expect("block golden vector prices");

    assert_eq!(priced.fixed_scaled, 1_630_529_463_688_974_647_719_310);
    assert_eq!(priced.operations_scaled, 42_682_295_930_216_867_992);
    assert_eq!(priced.resources_scaled, 103_248_000_000_000_000_000_000);
    assert_eq!(priced.mpt_scaled, 5_266_256_000_000_000_000_000_000);
    assert_eq!(priced.total_scaled, 7_000_076_145_984_904_864_587_302);
    assert_eq!(priced.total_pgu, Some(7_000_077));
}

#[test]
fn zk_gas_model_composes_route_aggregates_before_block_sum() {
    let priced = ZkGasModel
        .price_block(&BlockPricingInput {
            started_non_anchor_transactions: 0,
            native_value_transfers: 0,
            operation_routes: &[OperationRoutePricingInput {
                route: OperationRoute::Opcode(0x31),
                accepted_absolute_scaled: 100,
                incremental_scaled: 120,
                structural_scaled: 110,
                feature_error_scaled: 5,
            }],
            resources: ResourceSelectors::default(),
            mpt: MptSelectors::default(),
        })
        .expect("aggregate candidates price");
    assert_eq!(priced.operations_scaled, 125);
    assert_eq!(priced.feature_error_scaled, 5);
}

#[test]
fn zk_gas_model_accepts_the_declared_selector_maximum() {
    let maximum = 1_000_000_000;
    let priced = ZkGasModel
        .price_block(&BlockPricingInput {
            started_non_anchor_transactions: maximum,
            native_value_transfers: maximum,
            operation_routes: &[],
            resources: ResourceSelectors {
                unique_account_count: maximum,
                unique_storage_slot_access_count: maximum,
                unique_storage_slot_write_count: maximum,
                unique_code_hash_count: maximum,
                unique_code_bytes: maximum,
            },
            mpt: MptSelectors {
                path_node_visits: maximum,
                materialization_work_units: maximum,
                hydration_hash_blocks: maximum,
                dirty_encoded_nodes: maximum,
                dirty_hash_blocks: maximum,
                structural_branch_extension_churn_nodes: maximum,
                dropped_trie_nodes: maximum,
            },
        })
        .expect("declared selector maximum remains inside i128");
    assert_eq!(priced.total_scaled, 1_041_285_768_215_445_247_785_283_011_787_524);
    assert_eq!(priced.total_pgu, Some(1_041_285_768_215_446));
}

#[test]
fn zk_gas_model_fails_closed_for_unknown_missing_out_of_range_and_overflow() {
    let model = ZkGasModel;
    let unknown = OperationPricingInput {
        route: OperationRoute::Opcode(0x0c),
        interpreter_raw_gas: Some(1),
        native_gas: None,
        model_input: OperationModelInput::StaticRawGas { raw_gas: 1 },
        feature_error: false,
    };
    assert_eq!(model.price_operation(&unknown), Err(ZkGasError::UnknownRoute(unknown.route)));

    let missing = OperationPricingInput {
        route: OperationRoute::Opcode(0x01),
        interpreter_raw_gas: None,
        native_gas: None,
        model_input: OperationModelInput::None,
        feature_error: false,
    };
    assert_eq!(
        model.price_operation(&missing),
        Err(ZkGasError::MissingInput("interpreter_raw_gas"))
    );

    let out_of_range = OperationPricingInput {
        route: OperationRoute::Opcode(0x01),
        interpreter_raw_gas: Some(1_000_000_001),
        native_gas: None,
        model_input: OperationModelInput::StaticRawGas { raw_gas: 1_000_000_001 },
        feature_error: false,
    };
    assert!(matches!(
        model.price_operation(&out_of_range),
        Err(ZkGasError::SelectorOutOfRange {
            selector: "interpreter_raw_gas",
            value: 1_000_000_001
        })
    ));

    let opcode_with_native_gas = OperationPricingInput {
        route: OperationRoute::Opcode(0x40),
        interpreter_raw_gas: Some(20),
        native_gas: Some(20),
        model_input: OperationModelInput::None,
        feature_error: false,
    };
    assert_eq!(
        model.price_operation(&opcode_with_native_gas),
        Err(ZkGasError::InvalidInput("operation_namespace"))
    );

    let precompile_with_interpreter_gas = OperationPricingInput {
        route: OperationRoute::Precompile(0x04),
        interpreter_raw_gas: Some(144),
        native_gas: Some(144),
        model_input: OperationModelInput::None,
        feature_error: false,
    };
    assert_eq!(
        model.price_operation(&precompile_with_interpreter_gas),
        Err(ZkGasError::InvalidInput("operation_namespace"))
    );

    let spawn_with_raw_gas = OperationPricingInput {
        route: OperationRoute::ConfirmedSpawn(0xf0),
        interpreter_raw_gas: Some(32_000),
        native_gas: None,
        model_input: OperationModelInput::None,
        feature_error: false,
    };
    assert_eq!(
        model.price_operation(&spawn_with_raw_gas),
        Err(ZkGasError::InvalidInput("operation_namespace"))
    );

    let fixed_with_irrelevant_features = OperationPricingInput {
        route: OperationRoute::Opcode(0x40),
        interpreter_raw_gas: Some(20),
        native_gas: None,
        model_input: OperationModelInput::WorkUnits { work_units: 1 },
        feature_error: false,
    };
    assert_eq!(
        model.price_operation(&fixed_with_irrelevant_features),
        Err(ZkGasError::InvalidInput("model_input"))
    );

    let negative_route = [OperationRoutePricingInput {
        route: OperationRoute::Opcode(0x01),
        accepted_absolute_scaled: -1,
        incremental_scaled: 0,
        structural_scaled: 0,
        feature_error_scaled: 0,
    }];
    assert_eq!(
        model.price_block(&BlockPricingInput {
            started_non_anchor_transactions: 0,
            native_value_transfers: 0,
            operation_routes: &negative_route,
            resources: ResourceSelectors::default(),
            mpt: MptSelectors::default(),
        }),
        Err(ZkGasError::NegativeComponent("accepted_absolute"))
    );
}

#[test]
fn zk_gas_model_matches_python_proposal_once_golden_vectors_41710_through_41715() {
    struct Golden {
        proposal_id: u64,
        bytes: u64,
        nonempty: u64,
        path_visits: u64,
        materializations: u64,
        hydration_hash_blocks: u64,
        transaction_payload_scaled: i128,
        parent_mpt_scaled: i128,
        total_scaled: i128,
        total_pgu: u128,
    }

    let vectors = [
        Golden {
            proposal_id: 41_710,
            bytes: 42_228,
            nonempty: 196,
            path_visits: 15,
            materializations: 1_088,
            hydration_hash_blocks: 330,
            transaction_payload_scaled: 14_021_632_000_000_000_000_000_000,
            parent_mpt_scaled: 6_433_840_000_000_000_000_000_000,
            total_scaled: 174_719_342_735_480_402_119_958_866,
            total_pgu: 174_719_343,
        },
        Golden {
            proposal_id: 41_711,
            bytes: 45_617,
            nonempty: 212,
            path_visits: 15,
            materializations: 1_087,
            hydration_hash_blocks: 331,
            transaction_payload_scaled: 15_151_360_000_000_000_000_000_000,
            parent_mpt_scaled: 6_431_840_000_000_000_000_000_000,
            total_scaled: 175_847_070_735_480_402_119_958_866,
            total_pgu: 175_847_071,
        },
        Golden {
            proposal_id: 41_712,
            bytes: 51_060,
            nonempty: 198,
            path_visits: 15,
            materializations: 1_073,
            hydration_hash_blocks: 327,
            transaction_payload_scaled: 16_315_392_000_000_000_000_000_000,
            parent_mpt_scaled: 6_349_840_000_000_000_000_000_000,
            total_scaled: 176_929_102_735_480_402_119_958_866,
            total_pgu: 176_929_103,
        },
        Golden {
            proposal_id: 41_713,
            bytes: 46_530,
            nonempty: 196,
            path_visits: 15,
            materializations: 1_089,
            hydration_hash_blocks: 329,
            transaction_payload_scaled: 15_122_944_000_000_000_000_000_000,
            parent_mpt_scaled: 6_435_840_000_000_000_000_000_000,
            total_scaled: 175_822_654_735_480_402_119_958_866,
            total_pgu: 175_822_655,
        },
        Golden {
            proposal_id: 41_714,
            bytes: 48_884,
            nonempty: 198,
            path_visits: 15,
            materializations: 1_089,
            hydration_hash_blocks: 330,
            transaction_payload_scaled: 15_758_336_000_000_000_000_000_000,
            parent_mpt_scaled: 6_438_840_000_000_000_000_000_000,
            total_scaled: 176_461_046_735_480_402_119_958_866,
            total_pgu: 176_461_047,
        },
        Golden {
            proposal_id: 41_715,
            bytes: 46_778,
            nonempty: 197,
            path_visits: 15,
            materializations: 1_085,
            hydration_hash_blocks: 329,
            transaction_payload_scaled: 15_202_816_000_000_000_000_000_000,
            parent_mpt_scaled: 6_415_840_000_000_000_000_000_000,
            total_scaled: 175_882_526_735_480_402_119_958_866,
            total_pgu: 175_882_527,
        },
    ];

    for golden in vectors {
        let priced = ZkGasModel
            .price_proposal_once(&ProposalPricingInput {
                canonical_transaction_bytes: golden.bytes,
                nonempty_payload_transactions: golden.nonempty,
                true_unattempted_transactions: 0,
                source_blob_counts: &[1],
                proposal_blob_hash_count: 1,
                provider_blob_count: 1,
                provider_commitment_count: 1,
                provider_proof_count: 1,
                parent_mpt: MptSelectors {
                    path_node_visits: golden.path_visits,
                    materialization_work_units: golden.materializations,
                    hydration_hash_blocks: golden.hydration_hash_blocks,
                    ..Default::default()
                },
            })
            .unwrap_or_else(|error| panic!("proposal {} failed: {error}", golden.proposal_id));
        assert_eq!(priced.startup_scaled, 154_263_870_735_480_402_119_958_866);
        assert_eq!(priced.transaction_payload_scaled, golden.transaction_payload_scaled);
        assert_eq!(priced.blob_source_scaled, 0);
        assert_eq!(priced.parent_mpt_scaled, golden.parent_mpt_scaled);
        assert_eq!(priced.total_scaled, golden.total_scaled);
        assert_eq!(priced.total_pgu, Some(golden.total_pgu));
    }
}

#[test]
fn zk_gas_model_rejects_invalid_blob_source_topology() {
    let valid = ProposalPricingInput {
        canonical_transaction_bytes: 0,
        nonempty_payload_transactions: 0,
        true_unattempted_transactions: 0,
        source_blob_counts: &[1, 1, 3],
        proposal_blob_hash_count: 5,
        provider_blob_count: 5,
        provider_commitment_count: 5,
        provider_proof_count: 5,
        parent_mpt: MptSelectors::default(),
    };
    let priced = ZkGasModel.price_proposal_once(&valid).expect("valid BPO2 topology prices");
    assert_eq!(priced.blob_source_scaled, 81_788_928_000_000_000_000_000_000);
    assert_eq!(priced.total_scaled, 236_052_798_735_480_402_119_958_866);
    assert_eq!(priced.total_pgu, Some(236_052_799));

    let invalid = ProposalPricingInput {
        canonical_transaction_bytes: 0,
        nonempty_payload_transactions: 0,
        true_unattempted_transactions: 0,
        source_blob_counts: &[2, 1],
        proposal_blob_hash_count: 3,
        provider_blob_count: 3,
        provider_commitment_count: 3,
        provider_proof_count: 3,
        parent_mpt: MptSelectors::default(),
    };
    assert_eq!(
        ZkGasModel.price_proposal_once(&invalid),
        Err(ZkGasError::InvalidInput("forced_source_blob_count"))
    );
}

#[test]
fn zk_gas_pricing_compilation_does_not_mutate_the_unzen_consensus_schedule() {
    let before = UNZEN_ZK_GAS_SCHEDULE;
    let _ = ZkGasModel.route_count();
    let selected = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");

    assert!(before == UNZEN_ZK_GAS_SCHEDULE);
    assert!(before == *selected);
}

fn expected_is_spawn_opcode(opcode: u8) -> bool {
    EXPECTED_SPAWN_OPCODES.contains(&opcode)
}

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

#[derive(Clone, Copy, Debug)]
struct StepProbe {
    opcode: u8,
    raw_gas: u64,
    selected_new_frame: bool,
}

#[derive(Default, Debug)]
struct StepGasProbeInspector {
    gas_remaining: u64,
    steps: Vec<StepProbe>,
    call_targets: Vec<Address>,
    create_count: usize,
    precompile_gas_used: Option<u64>,
}

impl<CTX> Inspector<CTX, EthInterpreter> for StepGasProbeInspector {
    fn initialize_interp(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        self.gas_remaining = interp.gas.limit();
    }

    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        self.gas_remaining = interp.gas.remaining();
        self.steps.push(StepProbe {
            opcode: interp.bytecode.opcode(),
            raw_gas: 0,
            selected_new_frame: false,
        });
    }

    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        let remaining = interp.gas.remaining();
        let last = self.steps.last_mut().expect("step recorded");
        last.raw_gas = self.gas_remaining.saturating_sub(remaining);
        last.selected_new_frame =
            matches!(interp.bytecode.action(), Some(InterpreterAction::NewFrame(_)));
        self.gas_remaining = remaining;
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.call_targets.push(inputs.bytecode_address);
        None
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if outcome.was_precompile_called {
            self.precompile_gas_used =
                Some(inputs.gas_limit.saturating_sub(outcome.result.gas.remaining()));
        }
    }

    fn create(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.create_count += 1;
        None
    }
}

#[derive(Default)]
struct PrecompileGasMutatingInspector {
    observed_precompile: bool,
}

#[derive(Default)]
struct StepEndGasMutatingInspector;

impl<CTX> Inspector<CTX, EthInterpreter> for StepEndGasMutatingInspector {
    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        interp.gas.set_remaining(0);
    }
}

impl<CTX> Inspector<CTX, EthInterpreter> for PrecompileGasMutatingInspector {
    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, outcome: &mut CallOutcome) {
        if outcome.was_precompile_called {
            self.observed_precompile = true;
            outcome.result.gas.set_remaining(0);
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

    let expected = probe.steps.iter().fold(
        u64::from(schedule.precompile_multiplier(&Address::with_last_byte(0x04))) *
            precompile_gas_used,
        |acc, step| {
            let raw_gas = if step.opcode == opcode::STATICCALL {
                schedule.spawn_estimates.staticcall
            } else {
                step.raw_gas
            };
            acc + raw_gas * u64::from(schedule.opcode_multipliers[step.opcode as usize])
        },
    );
    let naive_expected = probe.steps.iter().fold(
        u64::from(schedule.precompile_multiplier(&Address::with_last_byte(0x04))) *
            precompile_gas_used,
        |acc, step| {
            acc + (step.raw_gas * u64::from(schedule.opcode_multipliers[step.opcode as usize]))
        },
    );

    assert!(
        probe
            .steps
            .iter()
            .any(|step| { step.opcode == opcode::STATICCALL && step.selected_new_frame })
    );
    assert!(probe.call_targets.contains(&Address::with_last_byte(0x04)));
    assert_eq!(meter.tx_zk_gas_used(), expected);
    assert_ne!(meter.tx_zk_gas_used(), naive_expected);
}

#[test]
fn production_metered_path_matches_inspector_path_for_precompile_dispatch() {
    for pricing_enabled in [false, true] {
        let mut production_evm = TaikoEvmFactory.create_evm(
            db_with_contract(staticcall_identity_bytecode()),
            evm_env(TaikoSpecId::UNZEN),
        );
        production_evm.set_zk_gas_pricing_enabled(pricing_enabled);
        production_evm.transact(tx_env(100_000)).expect("production path should execute");
        let production_zk_gas = production_evm
            .meter()
            .expect("production path should install a meter")
            .tx_zk_gas_used();

        let mut inspector_evm = TaikoEvmFactory.create_evm_with_inspector(
            db_with_contract(staticcall_identity_bytecode()),
            evm_env(TaikoSpecId::UNZEN),
            NoOpInspector {},
        );
        inspector_evm.set_zk_gas_pricing_enabled(pricing_enabled);
        inspector_evm.transact(tx_env(100_000)).expect("inspector path should execute");
        let inspector_zk_gas =
            inspector_evm.meter().expect("inspector path should install a meter").tx_zk_gas_used();

        assert_eq!(production_zk_gas, inspector_zk_gas);
        if pricing_enabled {
            production_evm.commit_transaction_zk_gas_pricing().expect("production pricing commit");
            inspector_evm.commit_transaction_zk_gas_pricing().expect("inspector pricing commit");
            let production =
                production_evm.zk_gas_pricing_snapshot().expect("production pricing snapshot");
            let inspected =
                inspector_evm.zk_gas_pricing_snapshot().expect("inspector pricing snapshot");
            assert_eq!(production, inspected);
            assert!(
                production.operation_routes.iter().any(|row| {
                    row.route == OperationRoute::Precompile(0x04) &&
                        row.accepted_absolute_scaled > 0
                }),
                "identity precompile must be priced separately from STATICCALL"
            );
        } else {
            assert!(production_evm.zk_gas_pricing_snapshot().is_none());
            assert!(inspector_evm.zk_gas_pricing_snapshot().is_none());
        }
    }
}

#[test]
fn zk_gas_pricing_reverted_precompile_matches_both_paths() {
    // BLAKE2F requires a 213-byte input, so an empty-input call executes the precompile and
    // returns failure to the outer contract.
    let bytecode = staticcall_precompile_bytecode(0x09);
    let (plain, inspected) = zk_gas_pricing_path_snapshots(bytecode, 100_000);
    assert_eq!(plain, inspected);
    assert!(plain.operation_routes.iter().any(|row| row.route == OperationRoute::Precompile(0x09)));
}

#[test]
fn precompile_metering_snapshots_gas_before_inner_call_end_mutation() {
    for pricing_enabled in [false, true] {
        let mut baseline_evm = TaikoEvmFactory.create_evm_with_inspector(
            db_with_contract(staticcall_identity_bytecode()),
            evm_env(TaikoSpecId::UNZEN),
            NoOpInspector {},
        );
        baseline_evm.set_zk_gas_pricing_enabled(pricing_enabled);
        baseline_evm.transact(tx_env(100_000)).expect("baseline precompile call should execute");
        let baseline_zk_gas =
            baseline_evm.meter().expect("baseline path should install a meter").tx_zk_gas_used();

        let mut mutating_evm = TaikoEvmFactory.create_evm_with_inspector(
            db_with_contract(staticcall_identity_bytecode()),
            evm_env(TaikoSpecId::UNZEN),
            PrecompileGasMutatingInspector::default(),
        );
        mutating_evm.set_zk_gas_pricing_enabled(pricing_enabled);
        mutating_evm.transact(tx_env(100_000)).expect("mutating inspector path should execute");
        let mutated_zk_gas =
            mutating_evm.meter().expect("mutating path should install a meter").tx_zk_gas_used();

        assert!(
            mutating_evm.inspector().observed_precompile,
            "test must mutate a precompile result"
        );
        assert_eq!(
            mutated_zk_gas, baseline_zk_gas,
            "inner inspector output mutation must not rewrite completed precompile work"
        );
        if pricing_enabled {
            baseline_evm.commit_transaction_zk_gas_pricing().expect("baseline pricing commit");
            mutating_evm.commit_transaction_zk_gas_pricing().expect("mutating pricing commit");
            assert_eq!(
                baseline_evm.zk_gas_pricing_snapshot(),
                mutating_evm.zk_gas_pricing_snapshot(),
                "pricing precompile selectors must be captured before call_end mutation"
            );
        } else {
            assert!(baseline_evm.zk_gas_pricing_snapshot().is_none());
            assert!(mutating_evm.zk_gas_pricing_snapshot().is_none());
        }
    }
}

#[test]
fn zk_gas_pricing_finalizes_executed_step_before_inner_step_end_mutation() {
    let code = Bytecode::new_raw(Bytes::from(vec![opcode::STOP]));
    let mut plain =
        TaikoEvmFactory.create_evm(db_with_contract(code.clone()), evm_env(TaikoSpecId::UNZEN));
    plain.set_zk_gas_pricing_enabled(true);
    plain.transact(tx_env(100_000)).expect("plain STOP");
    plain.commit_transaction_zk_gas_pricing().expect("plain pricing commit");

    let mut inspected = TaikoEvmFactory.create_evm_with_inspector(
        db_with_contract(code),
        evm_env(TaikoSpecId::UNZEN),
        StepEndGasMutatingInspector,
    );
    inspected.set_zk_gas_pricing_enabled(true);
    inspected.transact(tx_env(100_000)).expect("mutated STOP remains included");
    inspected.commit_transaction_zk_gas_pricing().expect("inspected pricing commit");

    assert_eq!(plain.zk_gas_pricing_snapshot(), inspected.zk_gas_pricing_snapshot());
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

#[derive(Clone, Copy, Debug)]
enum SpawnFamily {
    Call,
    CallCode,
    DelegateCall,
    StaticCall,
    Create,
    Create2,
}

impl SpawnFamily {
    const ALL: [Self; 6] = [
        Self::Call,
        Self::CallCode,
        Self::DelegateCall,
        Self::StaticCall,
        Self::Create,
        Self::Create2,
    ];

    const fn opcode(self) -> u8 {
        match self {
            Self::Call => opcode::CALL,
            Self::CallCode => opcode::CALLCODE,
            Self::DelegateCall => opcode::DELEGATECALL,
            Self::StaticCall => opcode::STATICCALL,
            Self::Create => opcode::CREATE,
            Self::Create2 => opcode::CREATE2,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Call => "CALL",
            Self::CallCode => "CALLCODE",
            Self::DelegateCall => "DELEGATECALL",
            Self::StaticCall => "STATICCALL",
            Self::Create => "CREATE",
            Self::Create2 => "CREATE2",
        }
    }

    fn spawned_bytecode(self, child: Address) -> Bytecode {
        let mut code = match self {
            Self::Call | Self::CallCode => vec![
                opcode::PUSH1,
                0x00, // return size
                opcode::PUSH1,
                0x00, // return offset
                opcode::PUSH1,
                0x00, // input size
                opcode::PUSH1,
                0x00, // input offset
                opcode::PUSH1,
                0x00, // value
                opcode::PUSH20,
            ],
            Self::DelegateCall | Self::StaticCall => vec![
                opcode::PUSH1,
                0x00, // return size
                opcode::PUSH1,
                0x00, // return offset
                opcode::PUSH1,
                0x00, // input size
                opcode::PUSH1,
                0x00, // input offset
                opcode::PUSH20,
            ],
            Self::Create => {
                return Bytecode::new_raw(Bytes::from(vec![
                    opcode::PUSH1,
                    0x00, // init-code size
                    opcode::PUSH1,
                    0x00, // init-code offset
                    opcode::PUSH1,
                    0x00, // value
                    opcode::CREATE,
                    opcode::STOP,
                ]));
            }
            Self::Create2 => {
                return Bytecode::new_raw(Bytes::from(vec![
                    opcode::PUSH1,
                    0x00, // salt
                    opcode::PUSH1,
                    0x00, // init-code size
                    opcode::PUSH1,
                    0x00, // init-code offset
                    opcode::PUSH1,
                    0x00, // value
                    opcode::CREATE2,
                    opcode::STOP,
                ]));
            }
        };
        code.extend_from_slice(child.as_slice());
        code.extend_from_slice(&[opcode::PUSH2, 0xff, 0xff, self.opcode(), opcode::STOP]);
        Bytecode::new_raw(Bytes::from(code))
    }

    fn prefix_zk_gas(self, schedule: &ZkGasSchedule) -> u64 {
        let charge = |opcode: u8| 3 * u64::from(schedule.opcode_multipliers[usize::from(opcode)]);
        match self {
            Self::Call | Self::CallCode => {
                5 * charge(opcode::PUSH1) + charge(opcode::PUSH20) + charge(opcode::PUSH2)
            }
            Self::DelegateCall | Self::StaticCall => {
                4 * charge(opcode::PUSH1) + charge(opcode::PUSH20) + charge(opcode::PUSH2)
            }
            Self::Create => 3 * charge(opcode::PUSH1),
            Self::Create2 => 4 * charge(opcode::PUSH1),
        }
    }

    const fn spawn_raw_gas(self, schedule: &ZkGasSchedule) -> u64 {
        match self {
            Self::Call => schedule.spawn_estimates.call,
            Self::CallCode => schedule.spawn_estimates.callcode,
            Self::DelegateCall => schedule.spawn_estimates.delegatecall,
            Self::StaticCall => schedule.spawn_estimates.staticcall,
            Self::Create => schedule.spawn_estimates.create,
            Self::Create2 => schedule.spawn_estimates.create2,
        }
    }

    fn was_dispatched(self, probe: &StepGasProbeInspector, child: Address) -> bool {
        match self {
            Self::Call | Self::CallCode | Self::DelegateCall | Self::StaticCall => {
                probe.call_targets.contains(&child)
            }
            Self::Create | Self::Create2 => probe.create_count != 0,
        }
    }
}

fn spawn_case_db(family: SpawnFamily, child: Address) -> InMemoryDB {
    let mut db = db_with_contract(family.spawned_bytecode(child));
    insert_contract(&mut db, child, simple_arithmetic_bytecode());
    db
}

#[test]
fn zk_gas_pricing_attempted_sstore_resources_match_on_static_and_oog_halts() {
    let child = Address::with_last_byte(0xD3);
    let child_code = Bytecode::new_raw(Bytes::from(vec![
        opcode::PUSH1,
        0x01,
        opcode::PUSH0,
        opcode::SSTORE,
        opcode::STOP,
    ]));
    for (spawn, forwarded_gas, label) in
        [(opcode::STATICCALL, 0xffff_u16, "static"), (opcode::CALL, 10_u16, "oog")]
    {
        let mut code = vec![opcode::PUSH0, opcode::PUSH0, opcode::PUSH0, opcode::PUSH0];
        if spawn == opcode::CALL {
            code.push(opcode::PUSH0);
        }
        code.push(opcode::PUSH20);
        code.extend_from_slice(child.as_slice());
        code.extend_from_slice(&[
            opcode::PUSH2,
            (forwarded_gas >> 8) as u8,
            forwarded_gas as u8,
            spawn,
            opcode::STOP,
        ]);
        let parent = Bytecode::new_raw(Bytes::from(code));
        let make_db = || {
            let mut db = db_with_contract(parent.clone());
            insert_contract(&mut db, child, child_code.clone());
            db
        };

        let mut plain = TaikoEvmFactory.create_evm(make_db(), evm_env(TaikoSpecId::UNZEN));
        plain.set_zk_gas_pricing_enabled(true);
        plain.transact(tx_env(200_000)).expect("plain included outer transaction");
        plain.commit_transaction_zk_gas_pricing().expect("plain pricing commit");
        let plain = plain.zk_gas_pricing_snapshot().expect("plain pricing snapshot");

        let mut inspected = TaikoEvmFactory.create_evm_with_inspector(
            make_db(),
            evm_env(TaikoSpecId::UNZEN),
            NoOpInspector,
        );
        inspected.set_zk_gas_pricing_enabled(true);
        inspected.transact(tx_env(200_000)).expect("inspected included outer transaction");
        inspected.commit_transaction_zk_gas_pricing().expect("inspected pricing commit");
        let inspected = inspected.zk_gas_pricing_snapshot().expect("inspected pricing snapshot");

        assert_eq!(plain, inspected, "{label} parity");
        assert_eq!(plain.resources.unique_storage_slot_write_count, 1, "{label}");
        assert!(
            plain.operation_routes.iter().any(|row| {
                row.route == OperationRoute::Opcode(opcode::SSTORE) && row.feature_error_scaled > 0
            }),
            "{label} attempted SSTORE feature error"
        );
    }
}

fn expected_probe_zk_gas(probe: &StepGasProbeInspector, schedule: &ZkGasSchedule) -> u64 {
    let opcode_total = probe.steps.iter().fold(0_u64, |total, step| {
        let raw_gas = if step.selected_new_frame && expected_is_spawn_opcode(step.opcode) {
            SpawnFamily::ALL
                .into_iter()
                .find(|family| family.opcode() == step.opcode)
                .expect("spawn opcode belongs to a family")
                .spawn_raw_gas(schedule)
        } else {
            step.raw_gas
        };
        total + raw_gas * u64::from(schedule.opcode_multipliers[usize::from(step.opcode)])
    });
    let precompile_total = probe.precompile_gas_used.map_or(0, |native_gas| {
        native_gas * u64::from(schedule.precompile_multiplier(&Address::with_last_byte(0x04)))
    });
    opcode_total + precompile_total
}

#[test]
fn spawn_opcode_classifier_matches_independent_protocol_set() {
    for opcode in u8::MIN..=u8::MAX {
        assert_eq!(
            super::meter::is_spawn_opcode(opcode),
            expected_is_spawn_opcode(opcode),
            "unexpected spawn classification for opcode {opcode:#04x}"
        );
    }
}

#[test]
fn metered_paths_select_identical_raw_gas_for_all_spawn_and_nonspawn_actions() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let child = Address::with_last_byte(0xD1);

    for pricing_enabled in [false, true] {
        for family in SpawnFamily::ALL {
            let mut plain = TaikoEvmFactory
                .create_evm(spawn_case_db(family, child), evm_env(TaikoSpecId::UNZEN));
            plain.set_zk_gas_pricing_enabled(pricing_enabled);
            let plain_result = plain
                .transact(tx_env(500_000))
                .unwrap_or_else(|err| panic!("{} plain spawn failed: {err}", family.name()));
            let plain_zk_gas = plain.meter().expect("plain meter").tx_zk_gas_used();

            let mut inspected = TaikoEvmFactory.create_evm_with_inspector(
                spawn_case_db(family, child),
                evm_env(TaikoSpecId::UNZEN),
                StepGasProbeInspector::default(),
            );
            inspected.set_zk_gas_pricing_enabled(pricing_enabled);
            let inspected_result = inspected
                .transact(tx_env(500_000))
                .unwrap_or_else(|err| panic!("{} inspected spawn failed: {err}", family.name()));
            let inspected_zk_gas = inspected.meter().expect("inspected meter").tx_zk_gas_used();
            let probe = inspected.inspector();
            let spawn_step = probe
                .steps
                .iter()
                .find(|step| step.opcode == family.opcode())
                .unwrap_or_else(|| panic!("{} step was not observed", family.name()));

            assert!(plain_result.result.is_success(), "{} plain result", family.name());
            assert!(inspected_result.result.is_success(), "{} inspected result", family.name());
            assert!(spawn_step.selected_new_frame, "{} must select NewFrame", family.name());
            assert_ne!(
                spawn_step.raw_gas,
                family.spawn_raw_gas(schedule),
                "{} test must distinguish interpreter delta from spawn estimate",
                family.name()
            );
            assert!(family.was_dispatched(probe, child), "{} child must dispatch", family.name());
            assert_eq!(
                inspected_zk_gas,
                expected_probe_zk_gas(probe, schedule),
                "{}",
                family.name()
            );
            assert_eq!(plain_zk_gas, inspected_zk_gas, "{} spawn parity", family.name());
            if pricing_enabled {
                plain.commit_transaction_zk_gas_pricing().expect("plain pricing spawn commit");
                inspected
                    .commit_transaction_zk_gas_pricing()
                    .expect("inspected pricing spawn commit");
                let plain_pricing =
                    plain.zk_gas_pricing_snapshot().expect("plain pricing snapshot");
                let inspected_pricing =
                    inspected.zk_gas_pricing_snapshot().expect("inspected pricing snapshot");
                assert_eq!(
                    plain_pricing,
                    inspected_pricing,
                    "{} pricing spawn parity",
                    family.name()
                );
                assert!(
                    plain_pricing
                        .operation_routes
                        .iter()
                        .any(|row| row.route == OperationRoute::ConfirmedSpawn(family.opcode())),
                    "{} confirmed wrapper route",
                    family.name()
                );
                assert!(
                    plain_pricing
                        .operation_routes
                        .iter()
                        .all(|row| row.route != OperationRoute::Opcode(family.opcode())),
                    "{} selected wrapper must replace the deferred base opcode",
                    family.name()
                );
            } else {
                assert!(plain.zk_gas_pricing_snapshot().is_none());
                assert!(inspected.zk_gas_pricing_snapshot().is_none());
            }

            let failing = Bytecode::new_raw(Bytes::from(vec![family.opcode()]));
            let mut plain = TaikoEvmFactory
                .create_evm(db_with_contract(failing.clone()), evm_env(TaikoSpecId::UNZEN));
            plain.set_zk_gas_pricing_enabled(pricing_enabled);
            let plain_result = plain
                .transact(tx_env(100_000))
                .unwrap_or_else(|err| panic!("{} plain nonspawn errored: {err}", family.name()));
            let plain_zk_gas = plain.meter().expect("plain meter").tx_zk_gas_used();

            let mut inspected = TaikoEvmFactory.create_evm_with_inspector(
                db_with_contract(failing),
                evm_env(TaikoSpecId::UNZEN),
                StepGasProbeInspector::default(),
            );
            inspected.set_zk_gas_pricing_enabled(pricing_enabled);
            let inspected_result = inspected.transact(tx_env(100_000)).unwrap_or_else(|err| {
                panic!("{} inspected nonspawn errored: {err}", family.name())
            });
            let inspected_zk_gas = inspected.meter().expect("inspected meter").tx_zk_gas_used();
            let probe = inspected.inspector();
            let failed_step = probe.steps.first().expect("failing opcode step");

            assert!(matches!(plain_result.result, ExecutionResult::Halt { .. }));
            assert!(matches!(inspected_result.result, ExecutionResult::Halt { .. }));
            assert!(!failed_step.selected_new_frame, "{} must not spawn", family.name());
            assert_ne!(
                failed_step.raw_gas,
                family.spawn_raw_gas(schedule),
                "{} nonspawn test must distinguish raw-gas sources",
                family.name()
            );
            assert!(!family.was_dispatched(probe, child), "{} must not dispatch", family.name());
            assert_eq!(
                inspected_zk_gas,
                expected_probe_zk_gas(probe, schedule),
                "{}",
                family.name()
            );
            assert_eq!(plain_zk_gas, inspected_zk_gas, "{} nonspawn parity", family.name());
            if pricing_enabled {
                plain.commit_transaction_zk_gas_pricing().expect("plain pricing nonspawn commit");
                inspected
                    .commit_transaction_zk_gas_pricing()
                    .expect("inspected pricing nonspawn commit");
                let plain_pricing =
                    plain.zk_gas_pricing_snapshot().expect("plain pricing snapshot");
                let inspected_pricing =
                    inspected.zk_gas_pricing_snapshot().expect("inspected pricing snapshot");
                assert_eq!(
                    plain_pricing,
                    inspected_pricing,
                    "{} pricing nonspawn parity",
                    family.name()
                );
                assert!(
                    plain_pricing
                        .operation_routes
                        .iter()
                        .any(|row| row.route == OperationRoute::Opcode(family.opcode())),
                    "{} unconfirmed wrapper uses ordinary opcode route",
                    family.name()
                );
                assert!(
                    plain_pricing
                        .operation_routes
                        .iter()
                        .all(|row| row.route != OperationRoute::ConfirmedSpawn(family.opcode())),
                    "{} unconfirmed wrapper is not a confirmed spawn",
                    family.name()
                );
            } else {
                assert!(plain.zk_gas_pricing_snapshot().is_none());
                assert!(inspected.zk_gas_pricing_snapshot().is_none());
            }
        }
    }
}

#[test]
fn metered_paths_match_for_dynamic_opcode_and_custom_inspector_composition() {
    let dynamic = Bytecode::new_raw(Bytes::from(vec![
        opcode::PUSH2,
        0x01,
        0x00,
        opcode::PUSH1,
        0x00,
        opcode::KECCAK256,
        opcode::STOP,
    ]));
    let mut plain =
        TaikoEvmFactory.create_evm(db_with_contract(dynamic.clone()), evm_env(TaikoSpecId::UNZEN));
    plain.transact(tx_env(100_000)).expect("plain dynamic opcode execution");
    let plain_zk_gas = plain.meter().expect("plain meter").tx_zk_gas_used();

    let mut inspected = TaikoEvmFactory.create_evm_with_inspector(
        db_with_contract(dynamic),
        evm_env(TaikoSpecId::UNZEN),
        StepGasProbeInspector::default(),
    );
    inspected.transact(tx_env(100_000)).expect("inspected dynamic opcode execution");
    let inspected_zk_gas = inspected.meter().expect("inspected meter").tx_zk_gas_used();
    let probe = inspected.inspector();
    let keccak = probe
        .steps
        .iter()
        .find(|step| step.opcode == opcode::KECCAK256)
        .expect("custom inspector observes KECCAK256");

    assert!(keccak.raw_gas > 30, "memory expansion must contribute dynamic gas");
    assert!(!keccak.selected_new_frame);
    assert_eq!(inspected_zk_gas, expected_probe_zk_gas(probe, &UNZEN_ZK_GAS_SCHEDULE));
    assert_eq!(plain_zk_gas, inspected_zk_gas);
}

fn assert_spawn_limit_rejected_before_dispatch(family: SpawnFamily) {
    let schedule = &UNZEN_ZK_GAS_SCHEDULE;
    let child = Address::with_last_byte(0xD2);
    let spawn_charge = family.spawn_raw_gas(schedule) *
        u64::from(schedule.opcode_multipliers[usize::from(family.opcode())]);
    let admitted = family.prefix_zk_gas(schedule) + spawn_charge - 1;
    let reserved = schedule.block_limit - admitted;

    let mut inspected = TaikoEvmFactory.create_evm_with_inspector(
        spawn_case_db(family, child),
        evm_env(TaikoSpecId::UNZEN),
        StepGasProbeInspector::default(),
    );
    inspected.reserve_block_zk_gas(reserved).expect("inspected reserve fits");
    let inspected_result = inspected.transact(tx_env(500_000));
    let inspected_zk_gas = inspected.meter().expect("inspected meter").tx_zk_gas_used();
    let dispatched = family.was_dispatched(inspected.inspector(), child);
    let observed_spawn = inspected
        .inspector()
        .steps
        .iter()
        .find(|step| step.opcode == family.opcode())
        .unwrap_or_else(|| panic!("{} spawn step must be observed", family.name()));

    let mut plain =
        TaikoEvmFactory.create_evm(spawn_case_db(family, child), evm_env(TaikoSpecId::UNZEN));
    plain.reserve_block_zk_gas(reserved).expect("plain reserve fits");
    let plain_run = catch_unwind(AssertUnwindSafe(|| plain.transact(tx_env(500_000))));
    assert!(
        plain_run.is_ok(),
        "{} plain path must replace its pending NewFrame without a debug assertion",
        family.name()
    );
    let plain_result = plain_run.expect("panic checked above");
    let plain_zk_gas = plain.meter().expect("plain meter").tx_zk_gas_used();

    let inspected_error = inspected_result.expect_err("inspected wrapper charge must be fatal");
    let plain_error = plain_result.expect_err("plain wrapper charge must be fatal");
    assert_eq!(inspected_error.to_string(), ZK_GAS_LIMIT_ERR, "{}", family.name());
    assert_eq!(plain_error.to_string(), ZK_GAS_LIMIT_ERR, "{}", family.name());
    assert!(
        observed_spawn.selected_new_frame,
        "{} observer must see the executed NewFrame before the adapter forces a limit halt",
        family.name()
    );
    assert!(!dispatched, "{} must not dispatch child work", family.name());
    assert_eq!(inspected_zk_gas, family.prefix_zk_gas(schedule), "{}", family.name());
    assert_eq!(plain_zk_gas, inspected_zk_gas, "{} fatal parity", family.name());
}

macro_rules! spawn_limit_test {
    ($name:ident, $family:expr) => {
        #[test]
        fn $name() {
            assert_spawn_limit_rejected_before_dispatch($family);
        }
    };
}

spawn_limit_test!(call_limit_rejection_prevents_child_dispatch, SpawnFamily::Call);
spawn_limit_test!(callcode_limit_rejection_prevents_child_dispatch, SpawnFamily::CallCode);
spawn_limit_test!(delegatecall_limit_rejection_prevents_child_dispatch, SpawnFamily::DelegateCall);
spawn_limit_test!(staticcall_limit_rejection_prevents_child_dispatch, SpawnFamily::StaticCall);
spawn_limit_test!(create_limit_rejection_prevents_child_dispatch, SpawnFamily::Create);
spawn_limit_test!(create2_limit_rejection_prevents_child_dispatch, SpawnFamily::Create2);

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
fn reset_clears_step_bookkeeping_after_a_limit_exceeded_nested_call() {
    // A zk gas limit hit inside a nested call aborts execution with per-frame step snapshots still
    // populated. The per-transact reset must drop that bookkeeping so the next transaction starts
    // from a clean metering state.
    let cheap_target = Address::with_last_byte(0xEE);

    assert_no_step_bookkeeping_leak_across_transacts(
        limit_exceeding_keccak_bytecode(),
        cheap_target,
    );
}

#[test]
fn reset_clears_step_bookkeeping_after_repeated_near_spawn_sized_charges() {
    // Variant where ~250k-zk KECCAK256 iterations exhaust the remaining budget inside a nested
    // frame, exercising the same transaction reset after many charges close to one CALL estimate.
    assert_no_step_bookkeeping_leak_across_transacts(
        quarter_million_keccak_loop_bytecode(),
        Address::with_last_byte(0xEF),
    );
}

/// Runs the shared bookkeeping-leak scenario: a nested call chain to `buster_bytecode` busts the
/// zk gas limit on a reused EVM, then a cheap unrelated transaction must meter exactly like a fresh
/// EVM would.
fn assert_no_step_bookkeeping_leak_across_transacts(
    buster_bytecode: Bytecode,
    cheap_target: Address,
) {
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
        "step bookkeeping left by a failed transaction must not leak into the next one"
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
    staticcall_precompile_bytecode(0x04)
}

fn staticcall_precompile_bytecode(address: u8) -> Bytecode {
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
        address,
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

/// `BENCH_TARGET` -> middle -> buster call chain that reaches the zk gas limit in the deepest
/// frame; `cheap_target` holds an unrelated cheap contract for the follow-up transaction.
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
/// gas. The zk budget is exhausted long before the forwarded EVM gas.
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
