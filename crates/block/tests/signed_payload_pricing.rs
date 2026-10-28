#![cfg(all(feature = "std", feature = "prover"))]

//! Independent signed-transaction integration verification against rebuilt native libraries.
use alethia_reth_block::{executor::TaikoBlockExecutor, factory::TaikoBlockExecutionCtx};
use alethia_reth_chainspec::{TAIKO_DEVNET, hardfork::TaikoHardfork};
use alethia_reth_evm::{
    alloy::TaikoZkGasEvm, factory::TaikoEvmFactory, spec::TaikoSpecId, zk_gas::pricing::ZkGasMode,
};
use alloy_consensus::{Signed, TxLegacy, transaction::Recovered};
use alloy_eips::Encodable2718;
use alloy_evm::{EvmEnv, EvmFactory};
use alloy_hardforks::ForkCondition;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::block::{BlockExecutor, CommitChanges};
use reth_evm_ethereum::RethReceiptBuilder;
use reth_revm::{
    State,
    db::InMemoryDB,
    state::{AccountInfo, Bytecode},
};
use std::sync::Arc;

const CALLER: Address = Address::with_last_byte(0x40);
const STOP: Address = Address::with_last_byte(0x41);
const REVERT: Address = Address::with_last_byte(0x42);
const HALT: Address = Address::with_last_byte(0x43);
const CHARGED_HALT: Address = Address::with_last_byte(0x44);

fn signed(to: Address, nonce: u64, input: &[u8]) -> Recovered<TransactionSigned> {
    let tx = TxLegacy {
        chain_id: Some(167),
        nonce,
        gas_price: 1,
        gas_limit: 100_000,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input: Bytes::copy_from_slice(input),
    };
    let sig = Signature::new(U256::from(1), U256::from(2), false);
    Recovered::new_unchecked(Signed::new_unchecked(tx, sig, B256::ZERO).into(), CALLER)
}

macro_rules! with_executor {
    ($mode:expr, $executor:ident, $body:block) => {{
        let mut spec = (*TAIKO_DEVNET).as_ref().clone();
        spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(0));
        let spec = Arc::new(spec);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            Address::from(alethia_reth_evm::alloy::TAIKO_GOLDEN_TOUCH_ADDRESS),
            AccountInfo { nonce: 7, ..Default::default() },
        );
        db.insert_account_info(
            CALLER,
            AccountInfo { balance: U256::from(10_000_000_000_u64), ..Default::default() },
        );
        for (address, code) in [
            (STOP, vec![0x00]),
            (REVERT, vec![0x5f, 0x5f, 0xfd]),
            (HALT, vec![0xfe]),
            (CHARGED_HALT, vec![0x01]),
        ] {
            let code = Bytecode::new_raw(Bytes::from(code));
            db.insert_account_info(
                address,
                AccountInfo {
                    nonce: 1,
                    code_hash: code.hash_slow(),
                    code: Some(code),
                    ..Default::default()
                },
            );
        }
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let mut env: EvmEnv<TaikoSpecId> = EvmEnv::default();
        env.cfg_env.spec = TaikoSpecId::UNZEN;
        env.cfg_env.chain_id = 167;
        env.block_env.number = U256::from(1);
        env.block_env.timestamp = U256::from(1);
        env.block_env.gas_limit = 30_000_000;
        let evm = TaikoEvmFactory.create_evm(&mut state, env);
        let ctx = TaikoBlockExecutionCtx {
            parent_hash: B256::ZERO,
            parent_beacon_block_root: Some(B256::ZERO),
            ommers: &[],
            withdrawals: None,
            basefee_per_gas: 0,
            extra_data: Bytes::new(),
            is_unzen_active: true,
            expected_difficulty: None,
            finalized_block_zk_gas: Default::default(),
        };
        let mut $executor = TaikoBlockExecutor::new_with_zk_gas_mode(
            evm,
            ctx,
            spec,
            RethReceiptBuilder::default(),
            $mode,
        );
        $body
    }};
}

#[test]
fn signed_envelopes_are_longer_than_calldata_and_failures_stay_included() {
    with_executor!(ZkGasMode::Shadow, executor, {
        let mut expected_bytes = 0;
        for (nonce, target, input) in
            [(0, STOP, &[][..]), (1, REVERT, &[1, 0, 2][..]), (2, HALT, &[0][..])]
        {
            let tx = signed(target, nonce, input);
            let length = tx.inner().encode_2718_len() as u64;
            assert!(length > input.len() as u64);
            expected_bytes += length;
            executor
                .execute_transaction(tx)
                .expect("valid execution result must produce an included receipt");
        }
        let snapshot = executor.evm().zk_gas_pricing_snapshot().unwrap();
        assert_eq!(snapshot.transaction_payload.canonical_transaction_bytes, expected_bytes);
        assert_eq!(snapshot.transaction_payload.nonempty_payload_transactions, 2);
        let (_, result) = executor.finish().unwrap();
        assert_eq!(result.receipts.len(), 3);
    });
}

#[test]
fn predicate_and_validation_failures_do_not_own_committed_payload() {
    with_executor!(ZkGasMode::Shadow, executor, {
        let omitted = signed(STOP, 0, &[1, 2, 3]);
        assert!(
            executor
                .execute_transaction_with_commit_condition(omitted, |_| CommitChanges::No)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            executor
                .evm()
                .zk_gas_pricing_snapshot()
                .unwrap()
                .transaction_payload
                .canonical_transaction_bytes,
            0
        );
        assert!(executor.execute_transaction(signed(STOP, 1, &[4, 5])).is_err());
        assert_eq!(
            executor
                .evm()
                .zk_gas_pricing_snapshot()
                .unwrap()
                .transaction_payload
                .canonical_transaction_bytes,
            0
        );
        let included = signed(STOP, 0, &[]);
        let expected_bytes = included.inner().encode_2718_len() as u64;
        executor.execute_transaction(included).unwrap();
        let snapshot = executor.evm().zk_gas_pricing_snapshot().unwrap();
        assert_eq!(snapshot.transaction_payload.canonical_transaction_bytes, expected_bytes);
        assert_eq!(snapshot.transaction_payload.nonempty_payload_transactions, 0);
        let attempted = executor.evm().zk_gas_attempted_pricing_snapshot().unwrap();
        assert_eq!(attempted.transaction_count, 3);
        assert!(attempted.block.transaction_payload.canonical_transaction_bytes > expected_bytes);
    });
}

#[test]
fn shadow_payload_does_not_change_receipts_or_unzen_difficulty() {
    fn run(mode: ZkGasMode) -> (Vec<reth_ethereum_primitives::Receipt>, u64, u64) {
        with_executor!(mode, executor, {
            for (nonce, target, input) in
                [(0, STOP, &[][..]), (1, REVERT, &[1, 2][..]), (2, HALT, &[0][..])]
            {
                executor.execute_transaction(signed(target, nonce, input)).unwrap();
            }
            let difficulty = executor.ctx.finalized_block_zk_gas();
            let (_, result) = executor.finish().unwrap();
            (result.receipts, result.gas_used, difficulty)
        })
    }
    assert_eq!(run(ZkGasMode::Disabled), run(ZkGasMode::Shadow));
}

#[test]
fn intrinsic_budget_rejection_creates_no_payload_attempt() {
    use alethia_reth_evm::zk_gas::unzen::{BLOCK_ZK_GAS_LIMIT, TX_INTRINSIC_ZK_GAS};
    with_executor!(ZkGasMode::Shadow, executor, {
        executor.reserve_block_zk_gas(BLOCK_ZK_GAS_LIMIT - TX_INTRINSIC_ZK_GAS + 1).unwrap();
        assert!(executor.execute_transaction(signed(STOP, 0, &[1, 2])).is_err());
        assert!(executor.execute_transaction(signed(STOP, 0, &[3])).is_err());
        let snapshot = executor.evm().zk_gas_pricing_snapshot().unwrap();
        assert_eq!(snapshot.transaction_payload.canonical_transaction_bytes, 0);
        assert_eq!(snapshot.transaction_payload.nonempty_payload_transactions, 0);
        assert_eq!(
            executor.evm().zk_gas_attempted_pricing_snapshot().unwrap().transaction_count,
            0
        );
    });
}

#[test]
fn anchor_envelope_is_owned_even_when_non_anchor_started_count_is_zero() {
    with_executor!(ZkGasMode::Shadow, executor, {
        executor.apply_pre_execution_changes().unwrap();
        let treasury = alethia_reth_evm::handler::get_treasury_address(167);
        let tx = TxLegacy {
            chain_id: Some(167),
            nonce: 7,
            gas_price: 1,
            gas_limit: 100_000,
            to: TxKind::Call(treasury),
            value: U256::ZERO,
            input: Bytes::from(vec![0, 1]),
        };
        let sig = Signature::new(U256::from(1), U256::from(2), false);
        let signed: TransactionSigned = Signed::new_unchecked(tx, sig, B256::ZERO).into();
        let expected_bytes = signed.encode_2718_len() as u64;
        let anchor = Recovered::new_unchecked(
            signed,
            Address::from(alethia_reth_evm::alloy::TAIKO_GOLDEN_TOUCH_ADDRESS),
        );
        executor.execute_transaction(anchor).unwrap();
        let snapshot = executor.evm().zk_gas_pricing_snapshot().unwrap();
        assert_eq!(snapshot.started_non_anchor_transactions, 0);
        assert_eq!(snapshot.transaction_payload.canonical_transaction_bytes, expected_bytes);
        assert_eq!(snapshot.transaction_payload.nonempty_payload_transactions, 1);
    });
}

#[test]
fn execution_budget_rejection_retains_attempted_payload_without_committing_it() {
    use alethia_reth_evm::zk_gas::unzen::{BLOCK_ZK_GAS_LIMIT, TX_INTRINSIC_ZK_GAS};
    with_executor!(ZkGasMode::Shadow, executor, {
        executor.reserve_block_zk_gas(BLOCK_ZK_GAS_LIMIT - TX_INTRINSIC_ZK_GAS - 1).unwrap();
        let tx = signed(CHARGED_HALT, 0, &[9]);
        let bytes = tx.inner().encode_2718_len() as u64;
        assert!(executor.execute_transaction(tx).is_err());
        assert_eq!(
            executor
                .evm()
                .zk_gas_pricing_snapshot()
                .unwrap()
                .transaction_payload
                .canonical_transaction_bytes,
            0
        );
        let attempted = executor.evm().zk_gas_attempted_pricing_snapshot().unwrap();
        assert_eq!(attempted.transaction_count, 1);
        assert_eq!(attempted.block.transaction_payload.canonical_transaction_bytes, bytes);
        assert_eq!(attempted.block.transaction_payload.nonempty_payload_transactions, 1);
        assert!(executor.execute_transaction(signed(STOP, 0, &[1])).is_err());
        assert_eq!(executor.evm().zk_gas_attempted_pricing_snapshot().unwrap(), attempted);
    });
}

#[test]
fn typed_envelope_type_byte_is_included_in_the_committed_payload_size() {
    with_executor!(ZkGasMode::Shadow, executor, {
        let tx = alloy_consensus::TxEip1559 {
            chain_id: 167,
            nonce: 0,
            gas_limit: 100_000,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(STOP),
            value: U256::ZERO,
            input: Bytes::from(vec![0, 1, 2]),
            access_list: Default::default(),
        };
        let sig = Signature::new(U256::from(1), U256::from(2), false);
        let tx: TransactionSigned = Signed::new_unchecked(tx, sig, B256::ZERO).into();
        let encoded = tx.encoded_2718();
        assert_eq!(encoded[0], 2);
        let bytes = encoded.len() as u64;
        assert_eq!(bytes, tx.encode_2718_len() as u64);
        executor.execute_transaction(Recovered::new_unchecked(tx, CALLER)).unwrap();
        let payload = executor.evm().zk_gas_pricing_snapshot().unwrap().transaction_payload;
        assert_eq!(payload.canonical_transaction_bytes, bytes);
        assert_eq!(payload.nonempty_payload_transactions, 1);
    });
}
