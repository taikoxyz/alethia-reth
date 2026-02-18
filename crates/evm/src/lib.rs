#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Taiko EVM extensions, handlers, and fork-spec adapters.
/// Alloy-facing EVM wrapper and helpers.
pub mod alloy;
/// Taiko EVM context types and builder traits.
pub mod context;
#[allow(clippy::module_inception)]
/// Core Taiko EVM type extensions over `reth-revm`.
pub mod evm;
/// Taiko transaction execution integration points.
pub mod execution;
/// Factory wiring for Taiko EVM builders.
pub mod factory;
/// Taiko-specific handler behavior for fee sharing and anchor processing.
pub mod handler;
/// Taiko hardfork spec identifiers and conversions.
pub mod spec;
