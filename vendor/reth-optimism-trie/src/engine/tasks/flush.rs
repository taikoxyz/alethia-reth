use super::super::state::EngineState;
use crate::OpProofsStore;
use crossbeam_channel::Sender;
use reth_evm::ConfigureEvm;
use reth_primitives_traits::BlockTy;
use reth_provider::{
    BlockHashReader, BlockReader, DatabaseProviderFactory, StateProviderFactory, StateReader,
};

/// Test-support request that waits until queued persistence work has completed.
pub(crate) struct FlushTask {
    /// Completion channel signalled after the persistence queue is drained.
    pub(crate) reply: Sender<()>,
}

impl FlushTask {
    /// Drains persistence work and notifies the requester.
    pub(crate) fn execute<Evm, Provider, Store>(self, state: &mut EngineState<Evm, Provider, Store>)
    where
        Evm: ConfigureEvm,
        Provider: BlockHashReader
            + StateReader
            + DatabaseProviderFactory
            + StateProviderFactory
            + BlockReader<Block = BlockTy<Evm::Primitives>>
            + Clone
            + 'static,
        Store: OpProofsStore + Clone + 'static,
    {
        state.drain_persistence();
        let _ = self.reply.send(());
    }
}
