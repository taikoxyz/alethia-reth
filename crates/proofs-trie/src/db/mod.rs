//! MDBX-backed storage implementation for the proofs sidecar.

pub mod models;
pub mod store;

pub use models::*;
pub use store::MdbxProofsStorage;
