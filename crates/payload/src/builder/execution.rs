//! Transaction execution helpers used by Taiko payload building.

use alloy_consensus::Transaction;
use alloy_eips::eip4844::BYTES_PER_BLOB;
use reth::{
    api::PayloadBuilderError,
    providers::{ChainSpecProvider, StateProviderFactory},
    revm::{cancelled::CancelOnDrop, primitives::U256},
};
use reth_errors::RethError;
use reth_ethereum::{EthPrimitives, TransactionSigned as EthTransactionSigned};
use reth_evm::{
    block::{BlockExecutionError, BlockValidationError},
    execute::BlockBuilder,
};
use reth_primitives::{Header as RethHeader, Recovered};
use tracing::{debug, trace, warn};

use alethia_reth_block::{
    executor::is_uzen_zk_gas_limit_exceeded,
    tx_selection::{
        DEFAULT_DA_ZLIB_GUARD_BYTES, SelectionOutcome, TxSelectionConfig,
        select_and_execute_pool_transactions,
    },
};
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_consensus::validation::{AnchorValidationContext, validate_anchor_transaction};
use alethia_reth_primitives::transaction::is_allowed_tx_type;

/// Creates an error for when a transaction's effective tip cannot be calculated.
fn missing_tip_error(base_fee: u64) -> PayloadBuilderError {
    PayloadBuilderError::Internal(RethError::msg(format!(
        "effective tip missing for executed transaction (base_fee={base_fee})"
    )))
}

/// Outcome of executing the transaction phase for a payload build.
pub(super) enum ExecutionOutcome {
    /// Execution was cancelled before completion.
    Cancelled,
    /// Execution completed successfully with accumulated fees.
    Completed(U256),
}

/// Context for executing transactions in new mode (anchor + pool transactions).
pub(super) struct PoolExecutionContext<'a> {
    /// Prebuilt anchor transaction for new mode.
    pub(super) anchor_tx: &'a Recovered<EthTransactionSigned>,
    /// The parent block header.
    pub(super) parent_header: &'a RethHeader,
    /// Timestamp for the new block.
    pub(super) block_timestamp: u64,
    /// Payload identifier for logging.
    pub(super) payload_id: String,
    /// Base fee per gas for transaction selection.
    pub(super) base_fee: u64,
    /// Block gas limit.
    pub(super) gas_limit: u64,
}

/// Executes the provided transaction list in legacy mode.
///
/// Preserves legacy mode: validation errors are skipped, fatal errors abort
/// the build, and cancellation short-circuits the loop.
pub(super) fn execute_provided_transactions(
    builder: &mut impl BlockBuilder<Primitives = EthPrimitives>,
    transactions: &[Recovered<EthTransactionSigned>],
    base_fee: u64,
    cancel: &CancelOnDrop,
) -> Result<ExecutionOutcome, PayloadBuilderError> {
    let mut total_fees = U256::ZERO;

    for tx in transactions {
        if cancel.is_cancelled() {
            return Ok(ExecutionOutcome::Cancelled);
        }

        if !is_allowed_tx_type(tx.inner()) {
            trace!(target: "payload_builder", ?tx, "skipping unsupported transaction type in legacy mode");
            continue;
        }

        let gas_used = match builder.execute_transaction(tx.clone()) {
            Ok(gas_used) => gas_used,
            Err(err) if is_uzen_zk_gas_limit_exceeded(&err) => {
                debug!(
                    target: "payload_builder",
                    ?tx,
                    "stopping legacy-mode payload after Uzen zk gas exhaustion"
                );
                break;
            }
            Err(BlockExecutionError::Validation(
                BlockValidationError::InvalidTx { .. } |
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. },
            )) => {
                trace!(target: "payload_builder", ?tx, "skipping invalid transaction in legacy mode");
                continue;
            }
            // Fatal errors should still fail the build
            Err(err) => {
                warn!(target: "payload_builder", ?tx, %err, "fatal error executing transaction");
                return Err(PayloadBuilderError::evm(err));
            }
        };

        // Add transaction fees to total
        let miner_fee =
            tx.effective_tip_per_gas(base_fee).ok_or_else(|| missing_tip_error(base_fee))?;
        total_fees += U256::from(miner_fee) * U256::from(gas_used);
    }

    Ok(ExecutionOutcome::Completed(total_fees))
}

/// Executes new-mode transactions: injects the anchor transaction, then pulls
/// from the mempool until exhaustion or cancellation.
pub(super) fn execute_anchor_and_pool_transactions<Client, Pool>(
    builder: &mut impl BlockBuilder<Primitives = EthPrimitives>,
    pool: &Pool,
    client: &Client,
    ctx: &PoolExecutionContext<'_>,
    cancel: &CancelOnDrop,
) -> Result<ExecutionOutcome, PayloadBuilderError>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec = TaikoChainSpec>
        + reth_provider::BlockReader,
    Pool: reth_transaction_pool::TransactionPool<
            Transaction: reth_transaction_pool::PoolTransaction<
                Consensus = reth_ethereum::TransactionSigned,
            >,
        >,
{
    debug!(target: "payload_builder", id=%ctx.payload_id, "injecting anchor transaction");

    let chain_spec = client.chain_spec();
    validate_anchor_transaction(
        ctx.anchor_tx.inner(),
        chain_spec.as_ref(),
        AnchorValidationContext {
            timestamp: ctx.block_timestamp,
            block_number: ctx.parent_header.number + 1,
            base_fee_per_gas: ctx.base_fee,
        },
    )
    .map_err(PayloadBuilderError::other)?;

    // Execute the anchor transaction as the first transaction in the block
    // NOTE: anchor transaction does not contribute to the total DA size limit calculation.
    match builder.execute_transaction(ctx.anchor_tx.clone()) {
        Ok(gas_used) => {
            // Note: Anchor transaction has zero priority fee (tip), so no fees to add
            debug!(target: "payload_builder", id=%ctx.payload_id, gas_used, "anchor transaction executed successfully");
        }
        Err(err) if is_uzen_zk_gas_limit_exceeded(&err) => {
            debug!(
                target: "payload_builder",
                id=%ctx.payload_id,
                "stopping new-mode payload after anchor hit the Uzen zk gas limit"
            );
            return Ok(ExecutionOutcome::Completed(U256::ZERO));
        }
        Err(err) => {
            warn!(target: "payload_builder", id=%ctx.payload_id, %err, "failed to execute anchor transaction");
            return Err(PayloadBuilderError::evm(err));
        }
    }

    // Use the shared transaction selection logic for pool transactions.
    let config = TxSelectionConfig {
        base_fee: ctx.base_fee,
        gas_limit_per_list: ctx.gas_limit,
        max_da_bytes_per_list: BYTES_PER_BLOB as u64,
        da_size_zlib_guard_bytes: DEFAULT_DA_ZLIB_GUARD_BYTES,
        max_lists: 1,
        min_tip: 0,
        locals: vec![],
    };

    match select_and_execute_pool_transactions(builder, pool, &config, || cancel.is_cancelled()) {
        Ok(SelectionOutcome::Cancelled) => Ok(ExecutionOutcome::Cancelled),
        Ok(SelectionOutcome::Completed(lists)) => {
            // Calculate total fees from the executed transactions.
            let total_fees = match lists.first() {
                Some(list) => list.transactions.iter().try_fold(U256::ZERO, |acc, etx| {
                    let tip = etx
                        .tx
                        .effective_tip_per_gas(ctx.base_fee)
                        .ok_or_else(|| missing_tip_error(ctx.base_fee))?;
                    Ok::<_, PayloadBuilderError>(acc + U256::from(tip) * U256::from(etx.gas_used))
                })?,
                None => U256::ZERO,
            };
            Ok(ExecutionOutcome::Completed(total_fees))
        }
        Err(err) => Err(PayloadBuilderError::evm(err)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind};
    use reth::revm::{
        State,
        context::result::ExecutionResult,
        db::InMemoryDB,
        state::{AccountInfo, Bytecode, bytecode::opcode},
    };
    use reth_evm::{
        EvmEnv, EvmFactory,
        block::{BlockExecutionError, BlockExecutor},
        execute::{BlockBuilder, BlockBuilderOutcome, ExecutorTx},
    };
    use reth_evm_ethereum::RethReceiptBuilder;
    use reth_primitives::Recovered;
    use reth_storage_api::StateProvider;

    use alethia_reth_block::{executor::TaikoBlockExecutor, factory::TaikoBlockExecutionCtx};
    use alethia_reth_chainspec::{TAIKO_DEVNET, hardfork::TaikoHardfork, spec::TaikoChainSpec};
    use alethia_reth_evm::{factory::TaikoEvmFactory, spec::TaikoSpecId};

    #[test]
    fn missing_tip_error_includes_base_fee_context() {
        let err = missing_tip_error(1234);
        let message = err.to_string();
        assert!(
            message.contains("base_fee=1234"),
            "expected error message to include base fee context, got: {message}"
        );
    }

    #[test]
    fn missing_tip_error_mentions_effective_tip() {
        let err = missing_tip_error(1);
        assert!(err.to_string().contains("effective tip missing"));
    }

    #[test]
    fn execute_provided_transactions_stops_on_uzen_zk_gas_error() {
        let chain_spec = Arc::new(uzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[
                (BENCH_SUCCESS_CALLER, 0),
                (BENCH_LIMIT_CALLER, 0),
                (BENCH_LATE_CALLER, 0),
            ]))
            .with_bundle_update()
            .without_state_clear()
            .build();
        let evm = TaikoEvmFactory.create_evm(&mut state, uzen_evm_env());
        let executor = TaikoBlockExecutor::new(
            evm,
            uzen_execution_ctx(),
            chain_spec,
            RethReceiptBuilder::default(),
        );
        let mut builder = ExecutorBackedBuilder { executor };
        let cancel = CancelOnDrop::default();
        let transactions = vec![
            recovered_tx(BENCH_SUCCESS_CALLER, BENCH_SUCCESS_TARGET, 0, 1),
            recovered_tx(BENCH_LIMIT_CALLER, BENCH_LIMIT_TARGET, 0, 1),
            recovered_tx(BENCH_LATE_CALLER, BENCH_SUCCESS_TARGET, 0, 1),
        ];

        let outcome = execute_provided_transactions(&mut builder, &transactions, 0, &cancel)
            .expect("Uzen zk gas exhaustion should stop cleanly");

        match outcome {
            ExecutionOutcome::Completed(total_fees) => {
                assert!(total_fees > U256::ZERO, "fees from the committed prefix should remain");
            }
            ExecutionOutcome::Cancelled => panic!("selection should not cancel"),
        }
        assert_eq!(builder.executor().receipts().len(), 1);
    }

    struct ExecutorBackedBuilder<E> {
        executor: E,
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
                &ExecutionResult<
                    <<Self::Executor as BlockExecutor>::Evm as reth_evm::Evm>::HaltReason,
                >,
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

    fn uzen_chain_spec() -> TaikoChainSpec {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Uzen, ForkCondition::Timestamp(0));
        chain_spec
    }

    fn uzen_evm_env() -> EvmEnv<TaikoSpecId> {
        let mut env: EvmEnv<TaikoSpecId> = EvmEnv::default();
        env.cfg_env.spec = TaikoSpecId::UZEN;
        env.cfg_env.chain_id = 167;
        env.block_env.number = U256::from(1_u64);
        env.block_env.timestamp = U256::from(1_u64);
        env.block_env.gas_limit = 30_000_000;
        env
    }

    fn uzen_execution_ctx<'a>() -> TaikoBlockExecutionCtx<'a> {
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

    fn recovered_tx(
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
            Signed::new_unchecked(tx, signature, B256::with_last_byte(caller.as_slice()[19]))
                .into(),
            caller,
        )
    }

    fn db_with_contracts(accounts: &[(Address, u64)]) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        insert_contract(&mut db, BENCH_SUCCESS_TARGET, arithmetic_bytecode());
        insert_contract(&mut db, BENCH_LIMIT_TARGET, limit_exceeding_keccak_bytecode());
        for &(address, nonce) in accounts {
            db.insert_account_info(
                address,
                AccountInfo {
                    nonce,
                    balance: U256::from(10_000_000_000_u64),
                    ..Default::default()
                },
            );
        }
        db
    }

    fn insert_contract(db: &mut InMemoryDB, address: Address, bytecode: Bytecode) {
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

    fn arithmetic_bytecode() -> Bytecode {
        Bytecode::new_raw(Bytes::from(vec![
            opcode::PUSH1,
            0x01,
            opcode::PUSH1,
            0x02,
            opcode::ADD,
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

    const BENCH_SUCCESS_CALLER: Address = Address::with_last_byte(0x30);
    const BENCH_LIMIT_CALLER: Address = Address::with_last_byte(0x31);
    const BENCH_LATE_CALLER: Address = Address::with_last_byte(0x32);
    const BENCH_SUCCESS_TARGET: Address = Address::with_last_byte(0x21);
    const BENCH_LIMIT_TARGET: Address = Address::with_last_byte(0x22);
}
