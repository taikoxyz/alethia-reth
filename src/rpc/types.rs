use alloy_rpc_types_engine::{ExecutionPayload, ExecutionPayloadSidecar};
use alloy_rpc_types_eth::Withdrawal;
use reth::revm::primitives::B256;
use reth_payload_primitives::ExecutionPayload as ExecutionPayloadTr;

/// Struct aggregating [`ExecutionPayload`] and [`ExecutionPayloadSidecar`] and encapsulating
/// complete payload supplied for execution.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TaikoExecutionData {
    /// Execution payload.
    pub payload: ExecutionPayload,
    /// Additional fork-specific fields.
    pub sidecar: ExecutionPayloadSidecar,
    /// Additional Taiko consensus specific fields.
    pub taiko_sidecar: TaikoExecutionDataSidecar,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TaikoExecutionDataSidecar {
    pub tx_hash: B256,
    pub withdrawals_hash: Option<B256>,
    pub taiko_block: bool,
}

impl ExecutionPayloadTr for TaikoExecutionData {
    fn parent_hash(&self) -> B256 {
        self.payload.parent_hash()
    }

    fn block_hash(&self) -> B256 {
        self.payload.block_hash()
    }

    fn block_number(&self) -> u64 {
        self.payload.block_number()
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        self.payload.withdrawals()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.sidecar.parent_beacon_block_root()
    }

    fn timestamp(&self) -> u64 {
        self.payload.timestamp()
    }

    fn gas_used(&self) -> u64 {
        self.payload.as_v1().gas_used
    }
}
