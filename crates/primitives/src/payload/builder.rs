use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_rlp::{Decodable, Encodable};
use alloy_rpc_types_engine::PayloadId;
use alloy_rpc_types_eth::Withdrawals;
use reth_ethereum::TransactionSigned;
use reth_ethereum_engine_primitives::EthPayloadBuilderAttributes;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives::Recovered;
use reth_primitives_traits::SignerRecoverable;
use sha2::{Digest, Sha256};
use std::fmt::Debug;
use tracing::debug;

use crate::payload::attributes::TaikoPayloadAttributes;

/// Taiko Payload Builder Attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaikoPayloadBuilderAttributes {
    /// Inner ethereum payload builder attributes
    pub payload_attributes: EthPayloadBuilderAttributes,
    /// Taiko related attributes.
    /// The hash of the RLP-encoded transactions in the L2 block.
    pub tx_list_hash: B256,
    /// The coinbase for the L2 block.
    pub beneficiary: Address,
    /// The gas limit for the L2 block.
    pub gas_limit: u64,
    /// The timestamp for the L2 block.
    pub timestamp: u64,
    /// The mix hash for the L2 block.
    pub mix_hash: B256,
    /// The basefee for the L2 block.
    pub base_fee_per_gas: u64,
    /// The transactions inside the L2 block.
    ///
    /// - `None`: Transactions should be selected from the mempool (new mode).
    /// - `Some(vec)`: Use the provided transaction list (legacy mode).
    pub transactions: Option<Vec<Recovered<TransactionSigned>>>,
    /// The extra data for the L2 block.
    pub extra_data: Bytes,
    /// Prebuilt anchor transaction for new mode, decoded and recovered.
    pub anchor_transaction: Option<Recovered<TransactionSigned>>,
}

impl PayloadBuilderAttributes for TaikoPayloadBuilderAttributes {
    /// The payload attributes that can be used to construct this type. Used as the argument in
    /// [`PayloadBuilderAttributes::try_new`].
    type RpcPayloadAttributes = TaikoPayloadAttributes;
    /// The error type used in [`PayloadBuilderAttributes::try_new`].
    type Error = alloy_rlp::Error;

    /// Creates a new payload builder for the given parent block and the attributes.
    ///
    /// Derives the unique [`PayloadId`] for the given parent and attributes.
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
            withdrawals: attributes.payload_attributes.withdrawals.unwrap_or_default().into(),
            parent_beacon_block_root: attributes.payload_attributes.parent_beacon_block_root,
        };

        // Determine transaction source based on whether tx_list is provided.
        let transactions = match &attributes.block_metadata.tx_list {
            None => {
                // New mode: transactions will be selected from mempool during payload building
                None
            }
            Some(tx_list_bytes) => {
                // Legacy mode: decode and use provided transactions
                let txs = decode_transactions(tx_list_bytes)
                    .unwrap_or_else(|e| {
                        // If we can't decode the given transactions bytes, we will mine an empty
                        // block instead.
                        debug!(
                            target: "payload_builder",
                            "Failed to decode transactions: {e}, bytes: {:?}, mining empty block",
                            tx_list_bytes
                        );
                        Vec::new()
                    })
                    .into_iter()
                    .filter_map(|tx| match tx.try_into_recovered() {
                        Ok(recovered) => Some(recovered),
                        Err(e) => {
                            debug!(
                                "Failed to recover transaction: {e}, skipping invalid transaction"
                            );
                            None
                        }
                    })
                    .collect();
                Some(txs)
            }
        };

        // Compute tx_list_hash based on whether tx_list is provided
        let tx_list_hash =
            attributes.block_metadata.tx_list.as_deref().map(keccak256).unwrap_or_default();

        let anchor_transaction = attributes
            .anchor_transaction
            .as_ref()
            .map(|bytes| {
                TransactionSigned::decode(&mut &bytes[..])
                    .map_err(|_| alloy_rlp::Error::Custom("invalid anchor_transaction"))?
                    .try_into_recovered()
                    .map_err(|_| alloy_rlp::Error::Custom("anchor tx not recoverable"))
            })
            .transpose()?;

        let res = Self {
            payload_attributes,
            tx_list_hash,
            beneficiary: attributes.block_metadata.beneficiary,
            gas_limit: attributes.block_metadata.gas_limit,
            timestamp: attributes.block_metadata.timestamp.to(),
            mix_hash: attributes.payload_attributes.prev_randao,
            base_fee_per_gas: attributes
                .base_fee_per_gas
                .try_into()
                .map_err(|_| alloy_rlp::Error::Custom("invalid attributes.base_fee_per_gas"))?,
            extra_data: attributes.block_metadata.extra_data,
            transactions,
            anchor_transaction,
        };

        Ok(res)
    }

    /// Returns the id for the running payload job.
    fn payload_id(&self) -> PayloadId {
        self.payload_attributes.id
    }

    /// Returns the parent for the running payload job.
    fn parent(&self) -> B256 {
        self.payload_attributes.parent
    }

    /// Returns the timestamp for the running payload job.
    fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Returns the parent beacon block root for the running payload job.
    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.payload_attributes.parent_beacon_block_root
    }

    /// Returns the suggested fee recipient for the running payload job.
    fn suggested_fee_recipient(&self) -> Address {
        self.beneficiary
    }

    /// Returns the random beacon value for the running payload job.
    fn prev_randao(&self) -> B256 {
        self.mix_hash
    }

    /// Returns the withdrawals for the running payload job.
    fn withdrawals(&self) -> &Withdrawals {
        &self.payload_attributes.withdrawals
    }
}

/// Generates the payload id for the configured payload from the [`TaikoPayloadAttributes`].
///
/// Returns an 8-byte identifier by hashing the payload components with sha256 hash.
pub fn payload_id_taiko(
    parent: &B256,
    attributes: &TaikoPayloadAttributes,
    payload_version: u8,
) -> PayloadId {
    let mut hasher = Sha256::new();
    hasher.update(parent.as_slice());
    hasher.update(&attributes.payload_attributes.timestamp.to_be_bytes()[..]);
    hasher.update(attributes.payload_attributes.prev_randao.as_slice());
    hasher.update(attributes.payload_attributes.suggested_fee_recipient.as_slice());
    if let Some(withdrawals) = &attributes.payload_attributes.withdrawals {
        let mut buf = Vec::with_capacity(withdrawals.length());
        withdrawals.encode(&mut buf);
        hasher.update(buf);
    }

    if let Some(parent_beacon_block) = attributes.payload_attributes.parent_beacon_block_root {
        hasher.update(parent_beacon_block);
    }

    // Include tx_list hash if provided (legacy mode), otherwise use zero hash (new mode)
    let tx_hash = attributes.block_metadata.tx_list.as_deref().map(keccak256).unwrap_or_default();
    hasher.update(tx_hash);
    hasher.update(attributes.block_metadata.extra_data.as_ref());

    let mut out = hasher.finalize();
    out[0] = payload_version;
    let mut id_bytes = [0u8; 8];
    id_bytes.copy_from_slice(&out[..8]);
    PayloadId::new(id_bytes)
}

// Decodes the given RLP-encoded bytes into transactions.
fn decode_transactions(bytes: &[u8]) -> Result<Vec<TransactionSigned>, alloy_rlp::Error> {
    Vec::<TransactionSigned>::decode(&mut &bytes[..])
}

#[cfg(all(test, feature = "net"))]
mod test {
    use super::*;
    use crate::payload::attributes::{RpcL1Origin, TaikoBlockMetadata, TaikoPayloadAttributes};
    use alloy_consensus::Header;
    use alloy_primitives::{Address, Bytes, U256, hex};
    use reth_chainspec::ChainSpec;
    use reth_engine_local::LocalPayloadAttributesBuilder;
    use reth_ethereum_engine_primitives::EthPayloadAttributes;
    use reth_node_api::PayloadAttributesBuilder;
    use reth_primitives_traits::SealedHeader;
    use std::sync::Arc;

    fn default_l1_origin() -> RpcL1Origin {
        RpcL1Origin {
            block_id: U256::ZERO,
            l2_block_hash: B256::ZERO,
            l1_block_hash: None,
            l1_block_height: None,
            build_payload_args_id: [0; 8],
            is_forced_inclusion: false,
            signature: [0; 65],
        }
    }

    fn default_eth_payload_attributes(timestamp: u64) -> EthPayloadAttributes {
        EthPayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
        }
    }

    fn create_block_metadata(timestamp: u64, tx_list: Option<Bytes>) -> TaikoBlockMetadata {
        TaikoBlockMetadata {
            beneficiary: Address::ZERO,
            gas_limit: 30_000_000,
            timestamp: U256::from(timestamp),
            mix_hash: B256::ZERO,
            extra_data: Bytes::default(),
            tx_list,
        }
    }

    fn create_payload_attrs(
        timestamp: u64,
        tx_list: Option<Bytes>,
        base_fee: u64,
    ) -> TaikoPayloadAttributes {
        TaikoPayloadAttributes {
            payload_attributes: default_eth_payload_attributes(timestamp),
            base_fee_per_gas: U256::from(base_fee),
            block_metadata: create_block_metadata(timestamp, tx_list),
            l1_origin: default_l1_origin(),
            anchor_transaction: None,
        }
    }

    #[test]
    fn test_decode_transactions() {
        let empty_decoded = decode_transactions(&Bytes::from_static(&hex!("0xc0")));
        assert_eq!(empty_decoded.unwrap().len(), 0);

        let with_anchor_decoded = decode_transactions(&Bytes::from_static(&hex!(
            "0xf90220b901b302f901af83028c59808083989680830f424094167001000000000000000000000000000001000180b9014448080a450000000000000000000000000000000000000000000000000000000000000028d2c559ea42da728e0d0154b95699eeac543c768755611756ab0d1ce2b0abe95600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000003200000000000000000000000000000000000000000000000000000000004c4b4000000000000000000000000000000000000000000000000000000000502989660000000000000000000000000000000000000000000000000000000023c3460000000000000000000000000000000000000000000000000000000000000001200000000000000000000000000000000000000000000000000000000000000000c080a079be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798a060ad1bd4369cd9156712860a4aaf49c474fa9290bbbc600069f666de1fd28cbdf868808502540be400830186a0943edb876b8928dd168f3785576a79afa7d07dc7978080830518d5a072ae800154047cf587c08937484082b436a4a0d236bfdf731603dfe5c7580a64a054161df1ea94ec7933b643fd6fbfefbb453350a47dfe4a4ec3cd840c0c5f915c"
        )));
        assert!(!with_anchor_decoded.unwrap().is_empty());
    }

    #[test]
    fn test_taiko_payload_builder_attributes_legacy_mode() {
        let tx_list_bytes = Bytes::from_static(&hex!("c0"));
        let payload_attrs = create_payload_attrs(1000, Some(tx_list_bytes.clone()), 100_000_000);

        let attrs = TaikoPayloadBuilderAttributes::try_new(B256::ZERO, payload_attrs, 1)
            .expect("Should create builder attributes in legacy mode");

        assert!(attrs.transactions.is_some(), "Legacy mode should have transactions");
        assert_eq!(
            attrs.transactions.unwrap().len(),
            0,
            "Empty tx_list should decode to empty vec"
        );
        assert_eq!(attrs.tx_list_hash, keccak256(tx_list_bytes));
    }

    #[test]
    fn test_taiko_payload_builder_attributes_new_mode() {
        let payload_attrs = create_payload_attrs(1000, None, 100_000_000);

        let attrs = TaikoPayloadBuilderAttributes::try_new(B256::ZERO, payload_attrs, 1)
            .expect("Should create builder attributes in new mode");

        assert!(attrs.transactions.is_none(), "New mode should use mempool selection");
        assert_eq!(attrs.tx_list_hash, B256::ZERO, "tx_list_hash should be zero without tx_list");
    }

    #[test]
    fn payload_id_changes_with_extra_data() {
        let builder = LocalPayloadAttributesBuilder::new(Arc::new(ChainSpec::<Header>::default()));
        let parent_hash = B256::from([1u8; 32]);
        // Create a parent header to pass to the builder
        let parent_header = Header { timestamp: 1_700_000_000, ..Default::default() };
        let parent = SealedHeader::seal_slow(parent_header);
        let mut base_attributes: TaikoPayloadAttributes = builder.build(&parent);
        base_attributes.block_metadata.extra_data = Bytes::from_static(b"extra-a");

        let mut other_attributes = base_attributes.clone();
        other_attributes.block_metadata.extra_data = Bytes::from_static(b"extra-b");

        let first = payload_id_taiko(&parent_hash, &base_attributes, 1);
        let second = payload_id_taiko(&parent_hash, &other_attributes, 1);

        assert_ne!(first, second);
    }
}
