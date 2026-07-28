//! Engine API type adapters for Taiko execution payloads.
use alloy_primitives::Bytes;
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadEnvelopeV3, ExecutionPayloadEnvelopeV4,
    ExecutionPayloadEnvelopeV5, ExecutionPayloadEnvelopeV6, ExecutionPayloadV1,
};
use reth_engine_primitives::EngineTypes;
use reth_ethereum_engine_primitives::EthBuiltPayload;
use reth_payload_primitives::{BuiltPayload, PayloadTypes};
use reth_primitives_traits::{NodePrimitives, SealedBlock};
use std::sync::Arc;

use self::types::{TaikoExecutionData, TaikoExecutionDataSidecar};
use crate::payload::attributes::TaikoPayloadAttributes;

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
