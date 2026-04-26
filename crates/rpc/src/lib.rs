#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Taiko RPC namespace extensions for engine and `eth` APIs.
/// Proof-history backed `debug_` namespace witness overrides.
pub mod debug;
/// Engine API extensions and validator/builder wiring.
pub mod engine;
/// Taiko `eth` namespace extensions and custom RPC methods.
pub mod eth;
/// Proof-history backed state provider factory for RPC witness generation.
pub mod proof_state;
