use std::borrow::Cow;

use alloy_consensus::{Transaction, TxReceipt};
use alloy_eips::{Encodable2718, eip7685::Requests};
use alloy_evm::{
    Database, FromRecoveredTx, FromTxWithEncoded,
    eth::{EthBlockExecutionCtx, receipt_builder::ReceiptBuilder, spec::EthExecutorSpec},
};
use reth::{
    primitives::Log,
    revm::{
        State,
        context::result::{ExecutionResult, ResultAndState},
    },
};
use reth_evm::{
    Evm, OnStateHook,
    block::{
        BlockExecutionError, BlockExecutor, BlockValidationError, CommitChanges, ExecutableTx,
        StateChangeSource, SystemCaller,
    },
    eth::receipt_builder::ReceiptBuilderCtx,
    state_change::{balance_increment_state, post_block_balance_increments},
};
use reth_provider::BlockExecutionResult;
use revm_database_interface::DatabaseCommit;

/// Block executor for Ethereum.
pub struct TaikoBlockExecutor<'a, Evm, Spec, R: ReceiptBuilder> {
    /// Reference to the specification object.
    spec: Spec,

    /// Context for block execution.
    pub ctx: EthBlockExecutionCtx<'a>,
    /// Inner EVM.
    evm: Evm,
    /// Utility to call system smart contracts.
    system_caller: SystemCaller<Spec>,
    /// Receipt builder.
    receipt_builder: R,

    /// Receipts of executed transactions.
    receipts: Vec<R::Receipt>,
    /// Total gas used by transactions in this block.
    gas_used: u64,
}

impl<'a, Evm, Spec, R> TaikoBlockExecutor<'a, Evm, Spec, R>
where
    Spec: Clone,
    R: ReceiptBuilder,
{
    /// Creates a new [`EthBlockExecutor`]
    pub fn new(evm: Evm, ctx: EthBlockExecutionCtx<'a>, spec: Spec, receipt_builder: R) -> Self {
        Self {
            evm,
            ctx,
            receipts: Vec::new(),
            gas_used: 0,
            system_caller: SystemCaller::new(spec.clone()),
            spec,
            receipt_builder,
        }
    }
}

impl<'db, DB, E, Spec, R> BlockExecutor for TaikoBlockExecutor<'_, E, Spec, R>
where
    DB: Database + 'db,
    E: Evm<
            DB = &'db mut State<DB>,
            Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
        >,
    Spec: EthExecutorSpec,
    R: ReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt<Log = Log>>,
{
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;
    type Evm = E;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        // Set state clear flag if the block is after the Spurious Dragon hardfork.
        let state_clear_flag = self
            .spec
            .is_spurious_dragon_active_at_block(self.evm.block().number);
        self.evm.db_mut().set_state_clear_flag(state_clear_flag);

        self.system_caller
            .apply_blockhashes_contract_call(self.ctx.parent_hash, &mut self.evm)?;
        self.system_caller
            .apply_beacon_root_contract_call(self.ctx.parent_beacon_block_root, &mut self.evm)?;

        Ok(())
    }

    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx<Self>,
        f: impl FnOnce(&ExecutionResult<<Self::Evm as Evm>::HaltReason>) -> CommitChanges,
    ) -> Result<Option<u64>, BlockExecutionError> {
        // The sum of the transaction's gas limit, Tg, and the gas utilized in this block prior,
        // must be no greater than the block's gasLimit.
        let block_available_gas = self.evm.block().gas_limit - self.gas_used;

        if tx.tx().gas_limit() > block_available_gas {
            return Err(
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: tx.tx().gas_limit(),
                    block_available_gas,
                }
                .into(),
            );
        }

        // Execute transaction.
        let ResultAndState { result, state } = self
            .evm
            .transact(tx)
            .map_err(|err| BlockExecutionError::evm(err, tx.tx().trie_hash()))?;

        if !f(&result).should_commit() {
            return Ok(None);
        }

        self.system_caller
            .on_state(StateChangeSource::Transaction(self.receipts.len()), &state);

        let gas_used = result.gas_used();

        // append gas used
        self.gas_used += gas_used;

        // Push transaction changeset and calculate header bloom filter for receipt.
        self.receipts
            .push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
                tx: tx.tx(),
                evm: &self.evm,
                result,
                state: &state,
                cumulative_gas_used: self.gas_used,
            }));

        // Commit the state changes.
        self.evm.db_mut().commit(state);

        Ok(Some(gas_used))
    }

    fn finish(
        mut self,
    ) -> Result<(Self::Evm, BlockExecutionResult<R::Receipt>), BlockExecutionError> {
        let balance_increments = post_block_balance_increments(
            &self.spec,
            self.evm.block(),
            self.ctx.ommers,
            self.ctx.withdrawals.as_deref(),
        );

        // increment balances
        self.evm
            .db_mut()
            .increment_balances(balance_increments.clone())
            .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

        // call state hook with changes due to balance increments.
        self.system_caller.try_on_state_with(|| {
            balance_increment_state(&balance_increments, self.evm.db_mut()).map(|state| {
                (
                    StateChangeSource::PostBlock(
                        reth_evm::block::StateChangePostBlockSource::BalanceIncrements,
                    ),
                    Cow::Owned(state),
                )
            })
        })?;

        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests: Requests::default(),
                gas_used: self.gas_used,
            },
        ))
    }

    fn set_state_hook(&mut self, hook: Option<Box<dyn OnStateHook>>) {
        self.system_caller.with_state_hook(hook);
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    fn evm(&self) -> &Self::Evm {
        &self.evm
    }
}
