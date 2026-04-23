#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
//! Bounded-history proof sidecar storage for alethia-reth.
//!
//! This crate provides a versioned, append-only MDBX store of account and storage
//! trie data, enabling sub-second historical `eth_getProof` / `debug_executionWitness`
//! within a configurable retention window.

// The following crates are declared as dependencies in `Cargo.toml` because they will be
// consumed by forthcoming tasks (live store, metrics). They are pulled in here with `as _`
// to silence `unused_crate_dependencies` until those modules land.
use eyre as _;
use metrics as _;
use parking_lot as _;
use reth_db_api as _;
use reth_evm as _;
use reth_metrics as _;
use reth_storage_errors as _;
use reth_tasks as _;
use reth_trie_db as _;
use strum as _;
use tokio as _;
use tracing as _;

pub mod api;
pub mod cursor_factory;
pub mod db;
pub mod error;
#[cfg(any(test, feature = "test-utils"))]
pub mod in_memory;
pub mod proof;
pub mod provider;

pub use api::*;
pub use cursor_factory::{ProofsHashedAccountCursorFactory, ProofsTrieCursorFactory};
pub use db::MdbxProofsStorage;
pub use error::{ProofsStorageError, ProofsStorageResult};
#[cfg(any(test, feature = "test-utils"))]
pub use in_memory::InMemoryProofsStorage;
pub use provider::ProofsStateProviderRef;
