use alloy_rpc_types_engine::{ExecutionPayload, ExecutionPayloadV1};
use alloy_rpc_types_eth::Withdrawal;
use reth::revm::primitives::B256;
use reth_payload_primitives::ExecutionPayload as ExecutionPayloadTr;

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TaikoExecutionData {
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub execution_payload: ExecutionPayloadV1,
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub taiko_sidecar: TaikoExecutionDataSidecar,
}

impl TaikoExecutionData {
    pub fn into_payload(self) -> ExecutionPayload {
        ExecutionPayload::V1(self.execution_payload)
    }
}

impl From<TaikoExecutionData> for ExecutionPayload {
    fn from(input: TaikoExecutionData) -> Self {
        input.into_payload()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TaikoExecutionDataSidecar {
    pub tx_hash: B256,
    pub withdrawals_hash: Option<B256>,
    pub taiko_block: Option<bool>,
}

impl ExecutionPayloadTr for TaikoExecutionData {
    fn parent_hash(&self) -> B256 {
        self.execution_payload.parent_hash
    }

    fn block_hash(&self) -> B256 {
        self.execution_payload.block_hash
    }

    fn block_number(&self) -> u64 {
        self.execution_payload.block_number
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        None
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        None
    }

    fn timestamp(&self) -> u64 {
        self.execution_payload.timestamp
    }

    fn gas_used(&self) -> u64 {
        self.execution_payload.gas_used
    }
}
