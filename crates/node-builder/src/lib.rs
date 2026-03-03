#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Node component builders for Taiko consensus and executors.
/// Taiko consensus builder integration for node composition.
mod consensus;
/// Taiko executor builder integration for node composition.
mod executor;

/// Provider-backed block reader and consensus builder exports.
pub use consensus::{ProviderTaikoBlockReader, TaikoConsensusBuilder};
/// Taiko executor builder export.
pub use executor::TaikoExecutorBuilder;
