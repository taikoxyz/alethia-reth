use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadEnvelopeV3, ExecutionPayloadEnvelopeV4,
    ExecutionPayloadEnvelopeV5, ExecutionPayloadV1,
};
use reth::primitives::SealedBlock;
use reth_ethereum_engine_primitives::EthBuiltPayload;
use reth_node_api::{BuiltPayload, EngineTypes, NodePrimitives, PayloadTypes};

use self::types::{TaikoExecutionData, TaikoExecutionDataSidecar};
use crate::payload::{attributes::TaikoPayloadAttributes, payload::TaikoPayloadBuilderAttributes};

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
    type ExecutionPayloadEnvelopeV3 = ExecutionPayloadEnvelopeV3;
    /// Execution Payload V4 envelope type.
    type ExecutionPayloadEnvelopeV4 = ExecutionPayloadEnvelopeV4;
    /// Execution Payload V5 envelope type.
    type ExecutionPayloadEnvelopeV5 = ExecutionPayloadEnvelopeV5;
}
