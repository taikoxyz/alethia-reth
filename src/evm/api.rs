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

// Trait that allows to replay and transact the transaction.
impl<CTX, INSP> ExecuteEvm for TaikoEvm<CTX, INSP>
where
    CTX: ContextSetters<Journal: JournalTr<FinalOutput = JournalOutput>>,
{
    type Output = Result<
        ResultAndState<HaltReason>,
        EVMError<<CTX::Db as Database>::Error, InvalidTransaction>,
    >;

    type Tx = <CTX as ContextTr>::Tx;

    type Block = <CTX as ContextTr>::Block;

    fn set_tx(&mut self, tx: Self::Tx) {
        self.inner.set_tx(tx);
    }

    fn set_block(&mut self, block: Self::Block) {
        self.inner.set_block(block);
    }

    fn replay(&mut self) -> Self::Output {
        TaikoEvmHandler::<_, _, EthFrame<_, _, _>>::new(self.extra_context()).run(self)
    }
}

// Trait allows replay_commit and transact_commit functionality.
impl<CTX, INSP> ExecuteCommitEvm for TaikoEvm<CTX, INSP>
where
    CTX: ContextSetters<Db: DatabaseCommit, Journal: JournalTr<FinalOutput = JournalOutput>>,
{
    type CommitOutput = Result<
        ExecutionResult<HaltReason>,
        EVMError<<CTX::Db as Database>::Error, InvalidTransaction>,
    >;

    fn replay_commit(&mut self) -> Self::CommitOutput {
        self.inner.replay_commit()
    }
}

// Inspection trait.
impl<CTX, INSP> InspectEvm for TaikoEvm<CTX, INSP>
where
    CTX: ContextSetters<Journal: JournalTr<FinalOutput = JournalOutput> + JournalExt>,
    INSP: Inspector<CTX, EthInterpreter>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.inspector = inspector;
    }

    fn inspect_replay(&mut self) -> Self::Output {
        TaikoEvmHandler::<_, _, EthFrame<_, _, _>>::new(self.extra_context())
            .inspect_run(&mut self.inner)
    }
}

// Inspect
impl<CTX, INSP> InspectCommitEvm for TaikoEvm<CTX, INSP>
where
    CTX: ContextSetters<
            Db: DatabaseCommit,
            Journal: JournalTr<FinalOutput = JournalOutput> + JournalExt,
        >,
    INSP: Inspector<CTX, EthInterpreter>,
{
    fn inspect_replay_commit(&mut self) -> Self::CommitOutput {
        self.inner.inspect_replay_commit()
    }
}
