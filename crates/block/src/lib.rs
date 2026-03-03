#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Taiko block assembly, execution, and transaction-selection components.
/// Block assembler implementation for Taiko payloads.
pub mod assembler;
/// EVM and block-construction configuration for Taiko execution.
pub mod config;
/// Block execution strategy and anchor pre-execution logic.
pub mod executor;
/// Executor factory wiring for Taiko block execution.
pub mod factory;
/// Shared transaction-selection primitives used by block and RPC flows.
pub mod tx_selection;
