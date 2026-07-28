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
    /// The Amsterdam block access list and slot number are retained in the transport sidecar so
    /// engine validation can explicitly reject unsupported fields.
    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
        bal: Option<Bytes>,
    ) -> Self::ExecutionData {
        let tx_hash = block.transactions_root;
        let withdrawals_hash = block.withdrawals_root;
        let header_difficulty = block.header().difficulty;
        let slot_number = block.header().slot_number;

        let payload = ExecutionPayloadV1::from_block_unchecked(block.hash(), &block.into_block());

        TaikoExecutionData {
            execution_payload: payload.into(),
            taiko_sidecar: TaikoExecutionDataSidecar {
                tx_hash,
                withdrawals_hash,
                header_difficulty: Some(header_difficulty),
                taiko_block: Some(true),
                block_access_list: bal,
                slot_number,
            },
        }
    }
}

impl From<EthBuiltPayload> for TaikoExecutionData {
    /// Converts a built payload into Taiko execution data while preserving unsupported Amsterdam
    /// fields for explicit engine validation.
    fn from(value: EthBuiltPayload) -> Self {
        let block_access_list = value.block_access_list().cloned();
        let block = Arc::unwrap_or_clone(value.into_block_arc()).into_sealed_block();
        TaikoEngineTypes::block_to_payload(block, block_access_list)
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
    use super::*;
    use alloy_primitives::U256;
    use reth_primitives_traits::{Block as _, RecoveredBlock};

    #[test]
    fn block_to_payload_preserves_bal_and_slot_number() {
        let block_access_list = Bytes::from_static(&[0xc0]);
        let mut block = reth_ethereum_primitives::Block::default();
        block.header.slot_number = Some(42);

        let execution_data =
            TaikoEngineTypes::block_to_payload(block.seal_slow(), Some(block_access_list.clone()));

        assert_eq!(execution_data.taiko_sidecar.block_access_list, Some(block_access_list));
        assert_eq!(execution_data.taiko_sidecar.slot_number, Some(42));
    }

    #[test]
    fn built_payload_conversion_preserves_bal_and_slot_number() {
        let block_access_list = Bytes::from_static(&[0xc1]);
        let mut block = reth_ethereum_primitives::Block::default();
        block.header.slot_number = Some(77);
        let recovered = RecoveredBlock::new_sealed(block.seal_slow(), Vec::new());
        let built_payload = EthBuiltPayload::new(
            Arc::new(recovered),
            U256::ZERO,
            None,
            Some(block_access_list.clone()),
        );

        let execution_data = TaikoExecutionData::from(built_payload);

        assert_eq!(execution_data.taiko_sidecar.block_access_list, Some(block_access_list));
        assert_eq!(execution_data.taiko_sidecar.slot_number, Some(77));
    }
}
