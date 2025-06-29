use std::fmt::Debug;

use alloy_rlp::{Bytes, Decodable, Encodable};
use alloy_rpc_types_eth::Withdrawals;
use reth::{
    api::PayloadBuilderAttributes,
    payload::PayloadId,
    primitives::Recovered,
    revm::primitives::{Address, B256, keccak256},
};
use reth_ethereum::TransactionSigned;
use reth_ethereum_engine_primitives::EthPayloadBuilderAttributes;

use crate::payload::attributes::TaikoPayloadAttributes;

/// Optimism Payload Builder Attributes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaikoPayloadBuilderAttributes {
    /// Inner ethereum payload builder attributes
    pub payload_attributes: EthPayloadBuilderAttributes,
    pub tx_list_hash: B256,
    pub beneficiary: Address,
    pub gas_limit: u64,
    pub timestamp: u64,
    pub mix_hash: B256,
    pub base_fee_per_gas: u64,
    pub transactions: Vec<Recovered<TransactionSigned>>,
    pub extra_data: Bytes,
}

impl PayloadBuilderAttributes for TaikoPayloadBuilderAttributes {
    type RpcPayloadAttributes = TaikoPayloadAttributes;
    type Error = alloy_rlp::Error;

    /// Creates a new payload builder for the given parent block and the attributes.
    ///
    /// Derives the unique [`PayloadId`] for the given parent and attributes
    fn try_new(
        parent: B256,
        attributes: TaikoPayloadAttributes,
        version: u8,
    ) -> Result<Self, Self::Error> {
        let id = payload_id_taiko(&parent, &attributes, version);

        let payload_attributes = EthPayloadBuilderAttributes {
            id,
            parent,
            timestamp: attributes.payload_attributes.timestamp,
            suggested_fee_recipient: attributes.payload_attributes.suggested_fee_recipient,
            prev_randao: attributes.payload_attributes.prev_randao,
            withdrawals: attributes
                .payload_attributes
                .withdrawals
                .unwrap_or_default()
                .into(),
            parent_beacon_block_root: attributes.payload_attributes.parent_beacon_block_root,
        };

        Ok(Self {
            payload_attributes: payload_attributes,
            tx_list_hash: keccak256(attributes.block_metadata.tx_list.clone()),
            beneficiary: attributes.block_metadata.beneficiary,
            gas_limit: attributes.block_metadata.gas_limit,
            timestamp: attributes.block_metadata.timestamp,
            mix_hash: attributes.payload_attributes.prev_randao,
            base_fee_per_gas: attributes.base_fee_per_gas.try_into().unwrap(),
            extra_data: attributes.block_metadata.extra_data,
            transactions: decode_transactions(&attributes.block_metadata.tx_list)?,
        })
    }

    fn payload_id(&self) -> PayloadId {
        self.payload_attributes.id
    }

    fn parent(&self) -> B256 {
        self.payload_attributes.parent
    }

    fn timestamp(&self) -> u64 {
        self.timestamp
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.payload_attributes.parent_beacon_block_root
    }

    fn suggested_fee_recipient(&self) -> Address {
        self.beneficiary
    }

    fn prev_randao(&self) -> B256 {
        self.mix_hash
    }

    fn withdrawals(&self) -> &Withdrawals {
        &self.payload_attributes.withdrawals
    }
}

/// Generates the payload id for the configured payload from the [`TaikoPayloadAttributes`].
///
/// Returns an 8-byte identifier by hashing the payload components with sha256 hash.
pub(crate) fn payload_id_taiko(
    parent: &B256,
    attributes: &TaikoPayloadAttributes,
    payload_version: u8,
) -> PayloadId {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(parent.as_slice());
    hasher.update(&attributes.payload_attributes.timestamp.to_be_bytes()[..]);
    hasher.update(attributes.payload_attributes.prev_randao.as_slice());
    hasher.update(
        attributes
            .payload_attributes
            .suggested_fee_recipient
            .as_slice(),
    );
    if let Some(withdrawals) = &attributes.payload_attributes.withdrawals {
        let mut buf = Vec::new();
        withdrawals.encode(&mut buf);
        hasher.update(buf);
    }

    if let Some(parent_beacon_block) = attributes.payload_attributes.parent_beacon_block_root {
        hasher.update(parent_beacon_block);
    }
    let tx_hash = keccak256(attributes.block_metadata.tx_list.clone());
    hasher.update(tx_hash);

    let mut out = hasher.finalize();
    out[0] = payload_version;
    PayloadId::new(out.as_slice()[..8].try_into().expect("sufficient length"))
}

fn decode_transactions(
    bytes: &Bytes,
) -> Result<Vec<Recovered<TransactionSigned>>, alloy_rlp::Error> {
    Vec::<Recovered<TransactionSigned>>::decode(&mut &bytes[..])
}
