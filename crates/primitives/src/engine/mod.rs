//! Engine API type adapters for Taiko execution payloads.
use alloy_primitives::Bytes;
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadEnvelopeV3, ExecutionPayloadEnvelopeV4,
    ExecutionPayloadEnvelopeV5, ExecutionPayloadEnvelopeV6, ExecutionPayloadV1,
};
use reth_engine_primitives::EngineTypes;
use reth_ethereum_engine_primitives::EthBuiltPayload;
use reth_payload_primitives::{BuiltPayload, PayloadTypes};
use reth_primitives_traits::{BlockBody as _, NodePrimitives, SealedBlock};
use std::sync::Arc;

use self::types::{TaikoExecutionData, TaikoExecutionDataSidecar, TaikoOsakaPayloadFields};
use crate::payload::attributes::TaikoPayloadAttributes;

/// Osaka engine wire types and normalization helpers.
pub mod osaka;
/// Taiko execution payload and sidecar structures.
pub mod types;

/// The types used in the Taiko consensus engine.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub struct TaikoEngineTypes;

impl PayloadTypes for TaikoEngineTypes {
    /// The execution payload type provided as input.
    type ExecutionData = TaikoExecutionData;
    /// The built payload type.
    type BuiltPayload = EthBuiltPayload;
    /// The RPC payload attributes type the CL node emits via the engine API.
    type PayloadAttributes = TaikoPayloadAttributes;

    /// Converts a block into an execution payload.
    ///
    /// Taiko networks schedule no Amsterdam fork, so a caller-supplied EIP-7928 block access
    /// list cannot be honored. It is carried on the sidecar's inbound-only sentinel instead of
    /// being dropped, so engine validation fails closed with `BlockAccessListNotSupported`
    /// rather than silently executing the block without it — reth's `reth_newPayload` BlockRlp
    /// arm and the debug consensus clients route caller-supplied data through this method.
    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
        bal: Option<Bytes>,
    ) -> Self::ExecutionData {
        let tx_hash = block.transactions_root;
        let withdrawals_hash = block.withdrawals_root;
        let header_difficulty = block.header().difficulty;
        let withdrawals = block.body().withdrawals().map(|value| value.to_vec());
        let expected_blob_versioned_hashes =
            block.body().blob_versioned_hashes_iter().copied().collect::<Vec<_>>();
        let has_osaka_fields = withdrawals_hash.is_some() ||
            withdrawals.is_some() ||
            !expected_blob_versioned_hashes.is_empty() ||
            block.header().parent_beacon_block_root.is_some() ||
            block.header().blob_gas_used.is_some() ||
            block.header().excess_blob_gas.is_some() ||
            block.header().requests_hash.is_some();
        let osaka = has_osaka_fields.then(|| TaikoOsakaPayloadFields {
            withdrawals: withdrawals.unwrap_or_default(),
            blob_gas_used: block.header().blob_gas_used.unwrap_or_default(),
            excess_blob_gas: block.header().excess_blob_gas.unwrap_or_default(),
            parent_beacon_block_root: block.header().parent_beacon_block_root.unwrap_or_default(),
            expected_blob_versioned_hashes,
            execution_requests: Vec::new(),
        });

        let payload = ExecutionPayloadV1::from_block_unchecked(block.hash(), &block.into_block());

        TaikoExecutionData {
            execution_payload: payload.into(),
            taiko_sidecar: TaikoExecutionDataSidecar {
                tx_hash,
                withdrawals_hash,
                header_difficulty: Some(header_difficulty),
                taiko_block: Some(true),
                block_access_list: bal,
                slot_number: None,
                osaka,
            },
        }
    }
}

impl From<EthBuiltPayload> for TaikoExecutionData {
    /// Converts a built payload into Taiko execution data. Locally built payloads never carry
    /// an Amsterdam block access list (the payload builder fails closed before Amsterdam
    /// activation), so the sidecar's inbound-only sentinels stay empty.
    fn from(value: EthBuiltPayload) -> Self {
        let block = Arc::unwrap_or_clone(value.into_block_arc()).into_sealed_block();
        TaikoEngineTypes::block_to_payload(block, None)
    }
}

impl EngineTypes for TaikoEngineTypes {
    /// Execution Payload V1 envelope type.
    type ExecutionPayloadEnvelopeV1 = ExecutionPayloadV1;
    /// Execution Payload V2 envelope type.
    type ExecutionPayloadEnvelopeV2 = ExecutionPayloadEnvelopeV2;
    /// Execution Payload V3 envelope type.
    type ExecutionPayloadEnvelopeV3 = ExecutionPayloadEnvelopeV3;
    /// Execution Payload V4 envelope type.
    type ExecutionPayloadEnvelopeV4 = ExecutionPayloadEnvelopeV4;
    /// Execution Payload V5 envelope type.
    type ExecutionPayloadEnvelopeV5 = ExecutionPayloadEnvelopeV5;
    /// Execution Payload V6 envelope type.
    type ExecutionPayloadEnvelopeV6 = ExecutionPayloadEnvelopeV6;
}

#[cfg(test)]
mod tests {
    use super::TaikoEngineTypes;
    use alloy_consensus::{Block, BlockBody, Header};
    use alloy_primitives::{B256, U256};
    use alloy_rpc_types_engine::ExecutionPayload;
    use alloy_rpc_types_eth::Withdrawal;
    use reth_payload_primitives::PayloadTypes;
    use reth_primitives_traits::SealedBlock;

    #[test]
    fn block_conversion_preserves_osaka_header_and_body_inputs() {
        let root = B256::with_last_byte(0x11);
        let requests_hash = B256::with_last_byte(0x22);
        let block = Block {
            header: Header {
                transactions_root: alloy_consensus::EMPTY_ROOT_HASH,
                withdrawals_root: Some(alloy_consensus::EMPTY_ROOT_HASH),
                difficulty: U256::from(91),
                parent_beacon_block_root: Some(root),
                blob_gas_used: Some(5),
                excess_blob_gas: Some(7),
                requests_hash: Some(requests_hash),
                ..Default::default()
            },
            body: BlockBody::default(),
        };
        let hash = block.header.hash_slow();

        let data =
            TaikoEngineTypes::block_to_payload(SealedBlock::new_unchecked(block, hash), None);
        let osaka = data.taiko_sidecar.osaka.as_ref().unwrap();

        assert_eq!(data.execution_payload.block_hash, hash);
        assert_eq!(data.taiko_sidecar.header_difficulty, Some(U256::from(91)));
        assert_eq!(osaka.parent_beacon_block_root, root);
        assert_eq!(osaka.blob_gas_used, 5);
        assert_eq!(osaka.excess_blob_gas, 7);
        assert!(osaka.withdrawals.is_empty());
        assert!(osaka.expected_blob_versioned_hashes.is_empty());
        assert!(osaka.execution_requests.is_empty());

        let ExecutionPayload::V3(payload) = data.into_payload() else {
            panic!("Osaka block must produce a V3 execution payload")
        };
        assert!(payload.payload_inner.payload_inner.transactions.is_empty());
        assert_eq!(payload.payload_inner.withdrawals, Vec::new());
        assert_eq!(payload.blob_gas_used, 5);
        assert_eq!(payload.excess_blob_gas, 7);
    }

    #[test]
    fn empty_legacy_block_remains_v1() {
        let block = Block { header: Header::default(), body: BlockBody::default() };
        let hash = block.header.hash_slow();
        let data =
            TaikoEngineTypes::block_to_payload(SealedBlock::new_unchecked(block, hash), None);

        assert!(data.taiko_sidecar.osaka.is_none());
        assert!(matches!(data.into_payload(), ExecutionPayload::V1(_)));
    }

    #[test]
    fn partial_osaka_inputs_keep_their_original_hash_and_body() {
        let withdrawal = Withdrawal {
            index: 1,
            validator_index: 2,
            address: alloy_primitives::Address::with_last_byte(3),
            amount: 4,
        };
        let header =
            Header { transactions_root: alloy_consensus::EMPTY_ROOT_HASH, ..Default::default() };
        let cases = [
            // Body withdrawals without a withdrawals root.
            Block {
                header: header.clone(),
                body: BlockBody {
                    withdrawals: Some(vec![withdrawal].into()),
                    ..Default::default()
                },
            },
            // A withdrawals root without body withdrawals.
            Block {
                header: Header {
                    withdrawals_root: Some(B256::with_last_byte(9)),
                    ..header.clone()
                },
                body: BlockBody::default(),
            },
            // A beacon root and requests hash without the blob-gas fields.
            Block {
                header: Header {
                    parent_beacon_block_root: Some(B256::with_last_byte(1)),
                    requests_hash: Some(B256::with_last_byte(2)),
                    ..header
                },
                body: BlockBody::default(),
            },
        ];
        for (index, block) in cases.into_iter().enumerate() {
            let hash = block.header.hash_slow();
            let data =
                TaikoEngineTypes::block_to_payload(SealedBlock::new_unchecked(block, hash), None);
            assert!(data.taiko_sidecar.osaka.is_some(), "case {index}");
            assert_eq!(data.execution_payload.block_hash, hash, "case {index}");
            if index == 0 {
                let ExecutionPayload::V3(payload) = data.into_payload() else {
                    panic!("a withdrawal-bearing block must produce a V3 execution payload")
                };
                assert_eq!(payload.payload_inner.withdrawals, vec![withdrawal]);
            }
        }
    }
}
