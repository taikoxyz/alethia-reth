pub mod builder;

pub use alethia_reth_consensus_core::{eip4396, validation};

use alethia_reth_consensus_core::validation::TaikoBlockReader;
use alloy_primitives::B256;
use reth_primitives_traits::{AlloyBlockHeader, Block};
use reth_provider::BlockReader;
use std::fmt::Debug;

/// Adapter that exposes a `reth_provider::BlockReader` as a Taiko block reader.
#[derive(Debug)]
pub struct ProviderTaikoBlockReader<T>(pub T);

impl<T> TaikoBlockReader for ProviderTaikoBlockReader<T>
where
    T: BlockReader + Debug,
    T::Block: Block,
{
    fn block_timestamp_by_hash(&self, hash: B256) -> Option<u64> {
        self.0
            .block_by_hash(hash)
            .ok()
            .flatten()
            .map(|block| block.header().timestamp())
    }
}
