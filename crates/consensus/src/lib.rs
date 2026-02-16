#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Taiko consensus validation rules and base-fee helpers.
/// EIP-4396 base-fee helpers used by Taiko Shasta validation.
pub mod eip4396;
/// Block and anchor transaction validation for Taiko consensus.
pub mod validation;
