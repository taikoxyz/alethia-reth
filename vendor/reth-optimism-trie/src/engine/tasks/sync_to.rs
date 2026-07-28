use super::super::state::EngineState;
use crate::OpProofsStore;
use reth_evm::ConfigureEvm;
use reth_primitives_traits::BlockTy;
use reth_provider::{
    BlockHashReader, BlockReader, DatabaseProviderFactory, StateProviderFactory, StateReader,
};
use tracing::debug;

/// Request to raise the engine's desired synchronization height.
pub(crate) struct SyncToTask {
    /// Block number the engine should eventually cover.
    pub(crate) target: u64,
}

impl SyncToTask {
    /// Updates the synchronization target without performing work synchronously.
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
        state.update_sync_target(self.target);
        debug!(target: "trie::engine::task", sync_target = self.target, "Sync target updated");
    }
}
