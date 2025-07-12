use reth::revm::{
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm, InspectCommitEvm, InspectEvm, Inspector,
    context::{
        ContextSetters, ContextTr, Database, JournalOutput, JournalTr,
        result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction, ResultAndState},
    },
    handler::{EthFrame, Handler},
    inspector::{InspectorHandler, JournalExt},
    interpreter::interpreter::EthInterpreter,
};

use crate::evm::{evm::TaikoEvm, handler::TaikoEvmHandler};

// Trait that allows to replay and transact the transaction, we
// use [`TaikoEvmHandler`] to handle the transactions execution, besides
// that, we use the same implementation as `RevmEvm`.
impl<CTX, INSP> ExecuteEvm for TaikoEvm<CTX, INSP>
where
    CTX: ContextSetters<Journal: JournalTr<FinalOutput = JournalOutput>>,
{
    /// Output of transaction execution.
    type Output = Result<
        ResultAndState<HaltReason>,
        EVMError<<CTX::Db as Database>::Error, InvalidTransaction>,
    >;
    /// Transaction type.
    type Tx = <CTX as ContextTr>::Tx;
    /// Block type.
    type Block = <CTX as ContextTr>::Block;

    /// Set the transaction.
    fn set_tx(&mut self, tx: Self::Tx) {
        self.inner.set_tx(tx);
    }

    /// Set the block.
    fn set_block(&mut self, block: Self::Block) {
        self.inner.set_block(block);
    }

    /// Transact the transaction that is set in the context, handle
    /// the execution with [TaikoEvmHandler] and return the result.
    fn replay(&mut self) -> Self::Output {
        TaikoEvmHandler::<_, _, EthFrame<_, _, _>>::new(self.extra_execution_ctx.clone()).run(self)
    }
}

// Trait allows replay_commit and transact_commit functionality, we use the same
// implementation as `RevmEvm`.
impl<CTX, INSP> ExecuteCommitEvm for TaikoEvm<CTX, INSP>
where
    CTX: ContextSetters<Db: DatabaseCommit, Journal: JournalTr<FinalOutput = JournalOutput>>,
{
    /// Commit output of transaction execution.
    type CommitOutput = Result<
        ExecutionResult<HaltReason>,
        EVMError<<CTX::Db as Database>::Error, InvalidTransaction>,
    >;

    /// Transact the transaction and commit to the state.
    fn replay_commit(&mut self) -> Self::CommitOutput {
        self.inner.replay_commit()
    }
}

// Inspection trait, here we use the same implementation as `RevmEvm`.
impl<CTX, INSP> InspectEvm for TaikoEvm<CTX, INSP>
where
    CTX: ContextSetters<Journal: JournalTr<FinalOutput = JournalOutput> + JournalExt>,
    INSP: Inspector<CTX, EthInterpreter>,
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

    /// Inspect the EVM with the current inspector and previous transaction.
    fn inspect_replay(&mut self) -> Self::Output {
        TaikoEvmHandler::<_, _, EthFrame<_, _, _>>::new(self.extra_execution_ctx.clone())
            .inspect_run(&mut self.inner)
    }
}

// Inspect the commit EVM, here we use the same implementation as `RevmEvm`.
impl<CTX, INSP> InspectCommitEvm for TaikoEvm<CTX, INSP>
where
    CTX: ContextSetters<
            Db: DatabaseCommit,
            Journal: JournalTr<FinalOutput = JournalOutput> + JournalExt,
        >,
    INSP: Inspector<CTX, EthInterpreter>,
{
    /// Inspect the EVM with the current inspector and previous transaction, similar to [`InspectEvm::inspect_replay`]
    /// and commit the state diff to the database.
    fn inspect_replay_commit(&mut self) -> Self::CommitOutput {
        self.inner.inspect_replay_commit()
    }
}
