use alloy_rlp::Bytes;
use alloy_rpc_types_engine::PayloadAttributes as EthPayloadAttributes;
use alloy_rpc_types_eth::Withdrawal;
use reth::revm::primitives::{Address, B256, U256};
use reth_node_api::PayloadAttributes;

/// Taiko Payload Attributes
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TaikoPayloadAttributes {
    /// The payload attributes
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub payload_attributes: EthPayloadAttributes,
    pub base_fee_per_gas: U256,
    pub block_metadata: TaikoBlockMetadata,
    pub l1_origin: L1Origin,
}

impl PayloadAttributes for TaikoPayloadAttributes {
    fn timestamp(&self) -> u64 {
        self.payload_attributes.timestamp()
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        self.payload_attributes.withdrawals()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.payload_attributes.parent_beacon_block_root()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TaikoBlockMetadata {
    pub beneficiary: Address,
    pub gas_limit: u64,
    pub timestamp: U256,
    pub mix_hash: B256,
    pub tx_list: Bytes,
    pub extra_data: Bytes,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct L1Origin {
    #[serde(rename = "blockID")]
    pub block_id: U256,
    pub l2_block_hash: B256,
    pub l1_block_height: Option<U256>,
    pub l1_block_hash: Option<B256>,
    pub build_payload_args_id: Option<[u8; 8]>,
}

impl L1Origin {
    pub fn is_preconf_block(&self) -> bool {
        self.l1_block_height.is_none() || self.l1_block_height == Some(U256::ZERO)
    }
}
