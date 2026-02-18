#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Taiko RPC namespace extensions for engine and `eth` APIs.
/// Engine API extensions and validator/builder wiring.
pub mod engine;
/// Taiko `eth` namespace extensions and custom RPC methods.
pub mod eth;

/// Taiko RPC type conversions
pub mod converter;
