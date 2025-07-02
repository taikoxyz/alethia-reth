use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadEnvelopeV3, ExecutionPayloadEnvelopeV4,
    ExecutionPayloadEnvelopeV5, ExecutionPayloadV1,
};
use reth::primitives::SealedBlock;
use reth_ethereum_engine_primitives::EthBuiltPayload;
use reth_node_api::{BuiltPayload, EngineTypes, NodePrimitives, PayloadTypes};

use crate::{
    payload::{attributes::TaikoPayloadAttributes, payload::TaikoPayloadBuilderAttributes},
    rpc::types::{TaikoExecutionData, TaikoExecutionDataSidecar},
};

/// The types used in the default Taiko consensus engine.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub struct TaikoEngineTypes;

impl PayloadTypes for TaikoEngineTypes {
    type ExecutionData = TaikoExecutionData;
    type BuiltPayload = EthBuiltPayload;
    type PayloadAttributes = TaikoPayloadAttributes;
    type PayloadBuilderAttributes = TaikoPayloadBuilderAttributes;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
    ) -> Self::ExecutionData {
        let tx_hash = block.transactions_root;
        let withdrawals_hash = block.withdrawals_root;

        let payload = ExecutionPayloadV1::from_block_unchecked(block.hash(), &block.into_block());

        TaikoExecutionData {
            execution_payload: ExecutionPayloadV1 {
                parent_hash: payload.parent_hash,
                prev_randao: payload.prev_randao,
                block_number: payload.block_number,
                gas_limit: payload.gas_limit,
                gas_used: payload.gas_used,
                timestamp: payload.timestamp,
                block_hash: payload.block_hash,
                fee_recipient: payload.fee_recipient,
                state_root: payload.state_root,
                receipts_root: payload.receipts_root,
                logs_bloom: payload.logs_bloom,
                extra_data: payload.extra_data,
                base_fee_per_gas: payload.base_fee_per_gas,
                transactions: payload.transactions,
            },
            taiko_sidecar: TaikoExecutionDataSidecar {
                tx_hash: tx_hash,
                withdrawals_hash: withdrawals_hash,
                taiko_block: Some(true),
            },
        }
    }
}

impl EngineTypes for TaikoEngineTypes {
    type ExecutionPayloadEnvelopeV1 = ExecutionPayloadV1;
    type ExecutionPayloadEnvelopeV2 = ExecutionPayloadEnvelopeV2;
    type ExecutionPayloadEnvelopeV3 = ExecutionPayloadEnvelopeV3;
    type ExecutionPayloadEnvelopeV4 = ExecutionPayloadEnvelopeV4;
    type ExecutionPayloadEnvelopeV5 = ExecutionPayloadEnvelopeV5;
}
