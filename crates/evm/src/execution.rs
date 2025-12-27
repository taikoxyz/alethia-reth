use reth_revm::{
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm, InspectCommitEvm, InspectEvm, Inspector,
    context::{
        ContextSetters, ContextTr, JournalTr,
        result::{EVMError, ExecResultAndState, ExecutionResult, HaltReason, ResultAndState},
    },
    handler::{EthFrame, Handler, PrecompileProvider},
    inspector::{InspectorHandler, JournalExt},
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
    state::EvmState,
};
use revm_database_interface::Database;

use crate::{
    error::TaikoInvalidTxError,
    evm::TaikoEvm,
    handler::TaikoEvmHandler,
    zk_gas::{TaikoChainContext, ZkGasViolation},
};

// Trait that allows to replay and transact the transaction, we
// use [`TaikoEvmHandler`] to handle the transactions execution, besides
// that, we use the same implementation as `RevmEvm`.
impl<CTX, INSP, P> ExecuteEvm for TaikoEvm<CTX, INSP, P>
where
    CTX:
        ContextSetters<Journal: JournalTr<State = EvmState>> + ContextTr<Chain = TaikoChainContext>,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    /// Output state type representing changes after execution.
    type State = EvmState;
    /// Transaction type.
    type Tx = <CTX as ContextTr>::Tx;
    /// Block type.
    type Block = <CTX as ContextTr>::Block;
    /// Output of transaction execution.
    type ExecutionResult = ExecutionResult<HaltReason>;
    type Error = EVMError<<CTX::Db as Database>::Error, TaikoInvalidTxError>;

    /// Set the block.
    fn set_block(&mut self, block: Self::Block) {
        self.inner.set_block(block);
    }

    /// Execute transaction and store state inside journal. Returns output of transaction execution.
    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.reset_zk_gas_for_tx();
        self.inner.ctx.set_tx(tx);
        let result =
            TaikoEvmHandler::<_, _, EthFrame>::new(self.extra_execution_ctx.clone()).run(self);

        match result {
            Ok(exec_result) => {
                if let Some(violation) = self.zk_gas_violation() {
                    return Err(EVMError::Transaction(map_zk_violation(violation)));
                }
                Ok(exec_result)
            }
            Err(err) => Err(err),
        }
    }

    /// Finalize execution, clearing the journal and returning the accumulated state changes.
    ///
    /// # State Management
    /// Journal is cleared and can be used for next transaction.
    fn finalize(&mut self) -> Self::State {
        self.inner.journal_mut().finalize()
    }

    /// Transact the transaction that is set in the context, handle
    /// the execution with [TaikoEvmHandler] and return the result.
    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        self.reset_zk_gas_for_tx();
        let result =
            TaikoEvmHandler::<_, _, EthFrame>::new(self.extra_execution_ctx.clone()).run(self);

        match result {
            Ok(exec_result) => {
                if let Some(violation) = self.zk_gas_violation() {
                    return Err(EVMError::Transaction(map_zk_violation(violation)));
                }
                Ok(ResultAndState::new(exec_result, self.finalize()))
            }
            Err(err) => Err(err),
        }
    }
}

// Trait allows replay_commit and transact_commit functionality, we use the same
// implementation as `RevmEvm`.
impl<CTX, INSP, P> ExecuteCommitEvm for TaikoEvm<CTX, INSP, P>
where
    CTX: ContextSetters<Db: DatabaseCommit, Journal: JournalTr<State = EvmState>>
        + ContextTr<Chain = TaikoChainContext>,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    /// Commit the state.
    fn commit(&mut self, state: Self::State) {
        self.inner.db_mut().commit(state);
    }
}

// Inspection trait, here we use the same implementation as `RevmEvm`.
impl<CTX, INSP, P> InspectEvm for TaikoEvm<CTX, INSP, P>
where
    CTX: ContextSetters<Journal: JournalTr<State = EvmState> + JournalExt>
        + ContextTr<Chain = TaikoChainContext>,
    INSP: Inspector<CTX, EthInterpreter>,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Inspector = INSP;

    /// Set the inspector for the EVM.
    ///
    /// this function is used to change inspector during execution.
    /// This function can't change Inspector type, changing inspector type can be done in
    /// `Evm` with `with_inspector` function.
    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.inspector = inspector;
    }

    /// Inspect the EVM with the given inspector and transaction.
    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.reset_zk_gas_for_tx();
        self.inner.set_tx(tx);
        let result = TaikoEvmHandler::<_, _, EthFrame>::new(self.extra_execution_ctx.clone())
            .inspect_run(&mut self.inner);

        match result {
            Ok(exec_result) => {
                if let Some(violation) = self.zk_gas_violation() {
                    return Err(EVMError::Transaction(map_zk_violation(violation)));
                }
                Ok(exec_result)
            }
            Err(err) => Err(err),
        }
    }
}

/// Maps a zk gas violation to a structured invalid-transaction error.
fn map_zk_violation(violation: ZkGasViolation) -> TaikoInvalidTxError {
    match violation {
        ZkGasViolation::TxLimitExceeded { limit, used } => {
            TaikoInvalidTxError::ZkTxGasLimitExceeded { limit, used }
        }
        ZkGasViolation::TxOverflow { used } => TaikoInvalidTxError::ZkTxGasOverflow { used },
    }
}

// Inspect the commit EVM, here we use the same implementation as `RevmEvm`.
impl<CTX, INSP, P> InspectCommitEvm for TaikoEvm<CTX, INSP, P>
where
    CTX: ContextSetters<Db: DatabaseCommit, Journal: JournalTr<State = EvmState> + JournalExt>
        + ContextTr<Chain = TaikoChainContext>,
    INSP: Inspector<CTX, EthInterpreter>,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
}
