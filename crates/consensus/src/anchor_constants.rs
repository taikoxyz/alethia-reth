//! Lightweight anchor constants for consumers that don't need full validation.
//!
//! This module provides only the anchor selectors and gas-limit constants,
//! avoiding the heavy consensus and chain-spec dependencies required by the
//! full validation logic.

use alloy_sol_types::{SolCall, sol};

sol! {
    function anchor(bytes32, bytes32, uint64, uint32) external;
    function anchorV2(uint64, bytes32, uint32, (uint8, uint8, uint32, uint64, uint32)) external;
    function anchorV3(uint64, bytes32, uint32, (uint8, uint8, uint32, uint64, uint32), bytes32[]) external;
    function anchorV4((uint48, bytes32, bytes32)) external;
}

/// Selector for the genesis-era `anchor` transaction.
pub const ANCHOR_V1_SELECTOR: &[u8; 4] = &anchorCall::SELECTOR;
/// Selector for the Ontake-era `anchorV2` transaction.
pub const ANCHOR_V2_SELECTOR: &[u8; 4] = &anchorV2Call::SELECTOR;
/// Selector for the Pacaya-era `anchorV3` transaction.
pub const ANCHOR_V3_SELECTOR: &[u8; 4] = &anchorV3Call::SELECTOR;
/// Selector for the Shasta-era `anchorV4` transaction.
pub const ANCHOR_V4_SELECTOR: &[u8; 4] = &anchorV4Call::SELECTOR;

/// The gas limit for the anchor transactions before Pacaya hardfork.
pub const ANCHOR_V1_V2_GAS_LIMIT: u64 = 250_000;
/// The gas limit for the anchor transactions in Pacaya and Shasta hardfork blocks.
pub const ANCHOR_V3_V4_GAS_LIMIT: u64 = 1_000_000;
