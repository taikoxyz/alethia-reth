use alloy_rpc_types_engine::{ExecutionPayloadEnvelopeV2, ExecutionPayloadV1};
use reth_engine_primitives::EngineTypes;
use reth_payload_primitives::{BuiltPayload, PayloadTypes};
use reth_primitives::SealedBlock;
use reth_primitives_traits::NodePrimitives;

use self::types::{TaikoExecutionData, TaikoExecutionDataSidecar};
use crate::payload::{
    attributes::TaikoPayloadAttributes, builder::TaikoPayloadBuilderAttributes,
    built_payload::TaikoBuiltPayload,
};

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
    type BuiltPayload = TaikoBuiltPayload;
    /// The RPC payload attributes type the CL node emits via the engine API.
    type PayloadAttributes = TaikoPayloadAttributes;
    /// The payload attributes type that contains information about a running payload job.
    type PayloadBuilderAttributes = TaikoPayloadBuilderAttributes;

    /// Converts a block into an execution payload.
    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
    ) -> Self::ExecutionData {
        let tx_hash = block.transactions_root;
        let withdrawals_hash = block.withdrawals_root;

        let payload = ExecutionPayloadV1::from_block_unchecked(block.hash(), &block.into_block());

        TaikoExecutionData {
            execution_payload: payload.into(),
            taiko_sidecar: TaikoExecutionDataSidecar {
                tx_hash,
                withdrawals_hash,
                taiko_block: Some(true),
            },
        }
    }
}

impl EngineTypes for TaikoEngineTypes {
    /// Execution Payload V1 envelope type.
    type ExecutionPayloadEnvelopeV1 = ExecutionPayloadV1;
    /// Execution Payload V2 envelope type.
    type ExecutionPayloadEnvelopeV2 = ExecutionPayloadEnvelopeV2;
    /// Execution Payload V3 envelope type.
    type ExecutionPayloadEnvelopeV3 = ExecutionPayloadEnvelopeV2;
    /// Execution Payload V4 envelope type.
    type ExecutionPayloadEnvelopeV4 = ExecutionPayloadEnvelopeV2;
    /// Execution Payload V5 envelope type.
    type ExecutionPayloadEnvelopeV5 = ExecutionPayloadEnvelopeV2;
    /// Execution Payload V6 envelope type.
    type ExecutionPayloadEnvelopeV6 = ExecutionPayloadEnvelopeV2;
}
