#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Taiko consensus validation rules and base-fee helpers.

/// Anchor selector constants and gas limits (always available, no heavy deps).
pub mod anchor_constants;

/// EIP-4396 base-fee helpers used by Taiko Shasta validation.
pub mod eip4396;

#[cfg(feature = "full")]
/// Block and anchor transaction validation for Taiko consensus.
pub mod validation;

#[cfg(not(feature = "full"))]
/// Fallback `validation` module that only re-exports anchor-related constants.
pub mod validation {
    pub use super::anchor_constants::*;
}
