//! Bounded-history proof sidecar storage for alethia-reth.
//!
//! This crate provides a versioned, append-only MDBX store of account and storage
//! trie data, enabling sub-second historical `eth_getProof` / `debug_executionWitness`
//! within a configurable retention window.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
