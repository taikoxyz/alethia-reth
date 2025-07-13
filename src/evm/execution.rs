use reth::revm::{
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm, InspectCommitEvm, InspectEvm, Inspector,
    context::{
        ContextSetters, ContextTr, JournalTr,
        result::{ExecutionResult, HaltReason},
    },
    handler::{EthFrame, Handler},
    inspector::{InspectorHandler, JournalExt},
    interpreter::interpreter::EthInterpreter,
};
use reth_revm::{
    context::result::{EVMError, ExecResultAndState, InvalidTransaction, ResultAndState},
    handler::PrecompileProvider,
    interpreter::InterpreterResult,
    state::EvmState,
};
use revm_database_interface::Database;

use crate::evm::{evm::TaikoEvm, handler::TaikoEvmHandler};

// Trait that allows to replay and transact the transaction, we
// use [`TaikoEvmHandler`] to handle the transactions execution, besides
// that, we use the same implementation as `RevmEvm`.
impl<CTX, INSP, P> ExecuteEvm for TaikoEvm<CTX, INSP, P>
where
    CTX: ContextSetters<Journal: JournalTr<State = EvmState>>,
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
    type Error = EVMError<<CTX::Db as Database>::Error, InvalidTransaction>;

    /// Set the block.
    fn set_block(&mut self, block: Self::Block) {
        self.inner.set_block(block);
    }

    /// Execute transaction and store state inside journal. Returns output of transaction execution.
    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        TaikoEvmHandler::<_, _, EthFrame>::new(self.extra_execution_ctx.clone()).run(self)
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
        TaikoEvmHandler::<_, _, EthFrame>::new(self.extra_execution_ctx.clone())
            .run(self)
            .map(|result| ResultAndState::new(result, self.finalize()))
    }
}

// Trait allows replay_commit and transact_commit functionality, we use the same
// implementation as `RevmEvm`.
impl<CTX, INSP, P> ExecuteCommitEvm for TaikoEvm<CTX, INSP, P>
where
    CTX: ContextSetters<Db: DatabaseCommit, Journal: JournalTr<State = EvmState>>,
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
    CTX: ContextSetters<Journal: JournalTr<State = EvmState> + JournalExt>,
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
        self.inner.set_tx(tx);
        TaikoEvmHandler::<_, _, EthFrame>::new(self.extra_execution_ctx.clone())
            .inspect_run(&mut self.inner)
    }
}

// Inspect the commit EVM, here we use the same implementation as `RevmEvm`.
impl<CTX, INSP, P> InspectCommitEvm for TaikoEvm<CTX, INSP, P>
where
    CTX: ContextSetters<Db: DatabaseCommit, Journal: JournalTr<State = EvmState> + JournalExt>,
    INSP: Inspector<CTX, EthInterpreter>,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
}
