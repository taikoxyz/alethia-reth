#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
//! Bounded-history proof sidecar storage for alethia-reth.
//!
//! This crate provides a versioned, append-only MDBX store of account and storage
//! trie data, enabling sub-second historical `eth_getProof` / `debug_executionWitness`
//! within a configurable retention window.

// The following crates are declared as dependencies in `Cargo.toml` because they will be
// consumed by forthcoming tasks (provider, cursors, live store, metrics). They are pulled
// in here with `as _` to silence `unused_crate_dependencies` until those modules land.
use auto_impl as _;
use eyre as _;
use metrics as _;
use parking_lot as _;
use reth_db_api as _;
use reth_evm as _;
use reth_metrics as _;
use reth_revm as _;
use reth_storage_errors as _;
use reth_tasks as _;
use reth_trie as _;
use reth_trie_db as _;
use strum as _;
use tokio as _;
use tracing as _;

pub mod db;
pub mod error;

pub use error::{ProofsStorageError, ProofsStorageResult};
