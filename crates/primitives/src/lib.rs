#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Shared primitive types used across Alethia Reth crates.
use reth_ethereum::Receipt;

#[cfg(feature = "net")]
/// Engine API payload and type definitions.
pub mod engine;
/// Helpers for decoding Taiko extra-data fields.
pub mod extra_data;
/// Payload-attribute and builder primitive types.
pub mod payload;

/// Taiko-specific block type.
pub type TaikoBlock = alloy_consensus::Block<TaikoTxEnvelope>;

/// Taiko-specific block body type.
pub type TaikoBlockBody = <TaikoBlock as reth_primitives_traits::Block>::Body;

pub use alethia_reth_consensus::transaction::TaikoTxEnvelope;

/// Primitive types for Taiko Node.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TaikoPrimitives;

impl reth_primitives_traits::NodePrimitives for TaikoPrimitives {
    type Block = TaikoBlock;
    type BlockHeader = alloy_consensus::Header;
    type BlockBody = TaikoBlockBody;
    type SignedTx = TaikoTxEnvelope;
    type Receipt = Receipt;
}
/// Bincode-compatible serde implementations.
#[cfg(feature = "serde-bincode-compat")]
pub mod serde_bincode_compat {
    pub use alethia_reth_consensus::transaction::serde_bincode_compat::*;
}
pub use extra_data::{
    SHASTA_EXTRA_DATA_LEN, decode_shasta_basefee_sharing_pctg, decode_shasta_proposal_id,
};
