//! Taiko `eth` and `taikoAuth` namespace RPC extensions.
/// Authenticated Taiko RPC methods and tx-pool helpers.
pub mod auth;
pub mod error;
#[allow(clippy::module_inception)]
/// Public Taiko `eth` namespace methods.
pub mod eth;
/// Aliases to `reth` Eth API types used by Taiko node wiring.
pub mod types;
