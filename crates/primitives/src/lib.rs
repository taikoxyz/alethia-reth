#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Shared primitive types used across Alethia Reth crates.
#[cfg(feature = "net")]
/// Engine API payload and type definitions.
pub mod engine;
/// Helpers for decoding Taiko extra-data fields.
pub mod extra_data;
/// Payload-attribute and builder primitive types.
pub mod payload;

pub use extra_data::{
    SHASTA_EXTRA_DATA_LEN, decode_shasta_basefee_sharing_pctg, decode_shasta_proposal_id,
};
