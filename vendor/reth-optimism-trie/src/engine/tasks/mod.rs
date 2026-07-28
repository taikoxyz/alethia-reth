//! Task structs — one per engine action variant.
//!
//! Each task owns its input data and reply channel. Its `execute` method
//! takes `&mut EngineState`, calls the appropriate state method, and sends
//! the reply. The engine dispatcher is a thin match with no business logic.

/// Executes canonical blocks and derives their trie updates.
mod execute_block;
#[cfg(any(test, feature = "test-utils"))]
/// Drains outstanding persistence work for deterministic synchronization.
mod flush;
/// Buffers already-computed trie and hashed-state updates.
mod index_block;
/// Rewinds the old chain and buffers a replacement branch.
mod reorg;
/// Advances the engine's desired synchronization height.
mod sync_to;
/// Rewinds buffered and persisted state to a requested boundary.
mod unwind;

pub(super) use execute_block::{ExecuteBlockTask, run as execute_block};
#[cfg(any(test, feature = "test-utils"))]
pub(super) use flush::FlushTask;
pub(super) use index_block::IndexBlockTask;
pub(super) use reorg::ReorgTask;
pub(super) use sync_to::SyncToTask;
pub(super) use unwind::UnwindTask;
