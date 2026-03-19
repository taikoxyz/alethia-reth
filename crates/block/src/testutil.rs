//! Shared test utilities for Uzen zk gas integration tests.
//!
//! These helpers are used by `crate::executor`, `crate::tx_selection`, and
//! `alethia_reth_payload::builder::execution` tests to avoid duplicating EVM
//! setup, chain spec creation, and bytecode generation.

use alloy_consensus::{Signed, TxLegacy};
use alloy_evm::EvmEnv;
use alloy_hardforks::ForkCondition;
use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U256};
use reth_ethereum::EthPrimitives;
use reth_evm::{
    block::{BlockExecutionError, BlockExecutor},
    execute::{BlockBuilder, BlockBuilderOutcome, ExecutorTx},
};
use reth_primitives::Recovered;
use reth_revm::{
    context::result::ExecutionResult,
    db::InMemoryDB,
    state::{AccountInfo, Bytecode, bytecode::opcode},
};
use reth_storage_api::StateProvider;

use crate::factory::TaikoBlockExecutionCtx;
use alethia_reth_chainspec::{TAIKO_DEVNET, hardfork::TaikoHardfork, spec::TaikoChainSpec};
use alethia_reth_evm::spec::TaikoSpecId;

/// Target address for contracts whose execution succeeds within the zk gas budget.
pub const BENCH_SUCCESS_TARGET: Address = Address::with_last_byte(0x21);

/// Target address for contracts whose execution exceeds the zk gas limit.
pub const BENCH_LIMIT_TARGET: Address = Address::with_last_byte(0x22);

/// Returns a [`TaikoChainSpec`] with Uzen activated at timestamp 0.
pub fn uzen_chain_spec() -> TaikoChainSpec {
    let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
    chain_spec.inner.hardforks.insert(TaikoHardfork::Uzen, ForkCondition::Timestamp(0));
    chain_spec
}

/// Returns an [`EvmEnv`] configured for Uzen execution.
pub fn uzen_evm_env() -> EvmEnv<TaikoSpecId> {
    let mut env: EvmEnv<TaikoSpecId> = EvmEnv::default();
    env.cfg_env.spec = TaikoSpecId::UZEN;
    env.cfg_env.chain_id = 167;
    env.block_env.number = U256::from(1_u64);
    env.block_env.timestamp = U256::from(1_u64);
    env.block_env.gas_limit = 30_000_000;
    env
}

/// Returns a [`TaikoBlockExecutionCtx`] for Uzen test blocks.
pub fn uzen_execution_ctx<'a>() -> TaikoBlockExecutionCtx<'a> {
    TaikoBlockExecutionCtx {
        parent_hash: B256::ZERO,
        parent_beacon_block_root: None,
        ommers: &[],
        withdrawals: None,
        basefee_per_gas: 0,
        extra_data: Bytes::default(),
        is_uzen: true,
        expected_difficulty: None,
        finalized_block_zk_gas: Default::default(),
    }
}

/// Builds a recovered legacy transaction targeting `to` from `caller`.
pub fn recovered_tx(
    caller: Address,
    to: Address,
    nonce: u64,
    gas_price: u64,
) -> Recovered<reth_ethereum::TransactionSigned> {
    let tx = TxLegacy {
        chain_id: Some(ChainId::from(167_u64)),
        nonce,
        gas_price: gas_price.into(),
        gas_limit: 5_000_000,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input: Bytes::default(),
    };
    let signature = Signature::new(U256::from(1_u64), U256::from(2_u64), false);
    Recovered::new_unchecked(
        Signed::new_unchecked(tx, signature, B256::with_last_byte(caller.as_slice()[19])).into(),
        caller,
    )
}

/// Creates an in-memory database preloaded with the standard test contracts and caller accounts.
pub fn db_with_contracts(accounts: &[(Address, u64)]) -> InMemoryDB {
    let mut db = InMemoryDB::default();
    insert_contract(&mut db, BENCH_SUCCESS_TARGET, arithmetic_bytecode());
    insert_contract(&mut db, BENCH_LIMIT_TARGET, limit_exceeding_keccak_bytecode());
    for &(address, nonce) in accounts {
        db.insert_account_info(
            address,
            AccountInfo { nonce, balance: U256::from(10_000_000_000_u64), ..Default::default() },
        );
    }
    db
}

/// Inserts a contract bytecode at the given address.
pub fn insert_contract(db: &mut InMemoryDB, address: Address, bytecode: Bytecode) {
    let code_hash = bytecode.hash_slow();
    db.insert_account_info(
        address,
        AccountInfo {
            nonce: 1,
            balance: U256::ZERO,
            code_hash,
            code: Some(bytecode),
            ..Default::default()
        },
    );
}

/// Simple arithmetic bytecode: `PUSH1 1, PUSH1 2, ADD, STOP`.
pub fn arithmetic_bytecode() -> Bytecode {
    Bytecode::new_raw(Bytes::from(vec![
        opcode::PUSH1,
        0x01,
        opcode::PUSH1,
        0x02,
        opcode::ADD,
        opcode::STOP,
    ]))
}

/// KECCAK256 bytecode that exceeds the Uzen zk gas block limit.
pub fn limit_exceeding_keccak_bytecode() -> Bytecode {
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

/// Minimal [`BlockBuilder`] adapter backed by a [`BlockExecutor`] for unit tests.
pub struct ExecutorBackedBuilder<E> {
    /// The underlying executor.
    pub executor: E,
}

impl<E> BlockBuilder for ExecutorBackedBuilder<E>
where
    E: BlockExecutor<
            Transaction = reth_ethereum::TransactionSigned,
            Receipt = reth_ethereum::Receipt,
        >,
{
    type Primitives = EthPrimitives;
    type Executor = E;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.executor.apply_pre_execution_changes()
    }

    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutorTx<Self::Executor>,
        f: impl FnOnce(
            &ExecutionResult<<<Self::Executor as BlockExecutor>::Evm as reth_evm::Evm>::HaltReason>,
        ) -> reth_evm::block::CommitChanges,
    ) -> Result<Option<u64>, BlockExecutionError> {
        let tx = tx.into_parts();
        self.executor.execute_transaction_with_commit_condition(tx, f)
    }

    fn finish(
        self,
        _state_provider: impl StateProvider,
    ) -> Result<BlockBuilderOutcome<Self::Primitives>, BlockExecutionError> {
        unreachable!("finish is not used in these unit tests")
    }

    fn executor_mut(&mut self) -> &mut Self::Executor {
        &mut self.executor
    }

    fn executor(&self) -> &Self::Executor {
        &self.executor
    }

    fn into_executor(self) -> Self::Executor {
        self.executor
    }
}
