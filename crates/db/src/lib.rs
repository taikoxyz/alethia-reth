#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Database compression helpers and Taiko table models.
/// Compression helpers for persisted payload bytes.
pub mod compress;
/// Typed database table definitions and row models.
pub mod model;
