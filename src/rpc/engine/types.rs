use alloy_rpc_types_engine::{ExecutionPayload, ExecutionPayloadV1};
use alloy_rpc_types_eth::Withdrawal;
use reth::revm::primitives::B256;
use reth_payload_primitives::ExecutionPayload as ExecutionPayloadTr;

/// Represents the execution data for the Taiko network, which includes the execution payload and a sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TaikoExecutionData {
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub execution_payload: ExecutionPayloadV1,
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub taiko_sidecar: TaikoExecutionDataSidecar,
}

impl TaikoExecutionData {
    /// Creates a new instance of `ExecutionPayload`.
    pub fn into_payload(self) -> ExecutionPayload {
        ExecutionPayload::V1(self.execution_payload)
    }
}

impl From<TaikoExecutionData> for ExecutionPayload {
    fn from(input: TaikoExecutionData) -> Self {
        input.into_payload()
    }
}

/// Represents the sidecar data for the Taiko execution payload, which includes the transaction hash,
/// optional withdrawals hash, and a boolean indicating if the block is a Taiko block.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TaikoExecutionDataSidecar {
    pub tx_hash: B256,
    pub withdrawals_hash: Option<B256>,
    pub taiko_block: Option<bool>,
}

impl ExecutionPayloadTr for TaikoExecutionData {
    /// Returns the parent hash of the block.
    fn parent_hash(&self) -> B256 {
        self.execution_payload.parent_hash
    }

    /// Returns the hash of the block.
    fn block_hash(&self) -> B256 {
        self.execution_payload.block_hash
    }

    /// Returns the block number.
    fn block_number(&self) -> u64 {
        self.execution_payload.block_number
    }

    /// Returns the withdrawals associated with the block, if any.
    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        None
    }

    /// Returns the parent beacon block root, if applicable.
    fn parent_beacon_block_root(&self) -> Option<B256> {
        None
    }

    /// Returns the timestamp of the block.
    fn timestamp(&self) -> u64 {
        self.execution_payload.timestamp
    }

    /// Returns the gas used in the block.
    fn gas_used(&self) -> u64 {
        self.execution_payload.gas_used
    }
}
