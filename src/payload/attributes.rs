use alloy_rlp::Bytes;
use alloy_rpc_types_engine::PayloadAttributes;
use reth::revm::primitives::{Address, B256, U256};

/// Optimism Payload Attributes
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TaikoPayloadAttributes {
    /// The payload attributes
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub payload_attributes: PayloadAttributes,
    pub base_fee_per_gas: U256,
    pub block_metadata: TaikoBlockMetadata,
    pub l1_origin: L1Origin,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TaikoBlockMetadata {
    pub beneficiary: Address,
    pub gas_limit: u64,
    pub timestamp: u64,
    pub mix_hash: B256,
    pub tx_list: Bytes,
    pub extra_data: Bytes,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct L1Origin {
    pub block_id: U256,
    pub l2_block_hash: B256,
    pub l1_block_height: Option<U256>,
    pub l1_block_hash: Option<B256>,
    pub build_payload_args_id: [u8; 8],
}
