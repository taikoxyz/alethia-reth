#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Node component builders for Taiko consensus and executors.

/// Taiko consensus builder integration for node composition.
mod consensus;

/// Taiko `eth` API builder and method implementations.
mod eth_api;

/// Taiko block execution and validation builder implementations.
mod executor;

/// Network builder for Taiko P2P integration with `reth` node services.
mod network;

/// Taiko payload builder integration with `reth` node services.
mod payload_builder;

/// Provider-backed block reader and consensus builder exports.
pub use consensus::{ProviderTaikoBlockReader, TaikoConsensusBuilder};
pub use eth_api::TaikoEthApiBuilder;
pub use executor::TaikoExecutorBuilder;
pub use network::TaikoNetworkBuilder;
pub use payload_builder::TaikoPayloadBuilderBuilder;
