use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
use alloy_rpc_types_engine::{ExecutionPayload, ExecutionPayloadV1};
use alloy_rpc_types_eth::Withdrawal;
use reth_payload_primitives::ExecutionPayload as ExecutionPayloadTr;

/// Represents the execution data for the Taiko network, which includes the execution payload and a
/// sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TaikoExecutionData {
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub execution_payload: TaikoExecutionPayloadV1,
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub taiko_sidecar: TaikoExecutionDataSidecar,
}

impl TaikoExecutionData {
    /// Creates a new instance of `ExecutionPayload`.
    pub fn into_payload(self) -> ExecutionPayload {
        ExecutionPayload::V1(self.execution_payload.into())
    }
}

impl From<TaikoExecutionData> for ExecutionPayload {
    fn from(input: TaikoExecutionData) -> Self {
        input.into_payload()
    }
}

/// Represents the sidecar data for the Taiko execution payload, which includes the transaction
/// hash, optional withdrawals hash, and a boolean indicating if the block is a Taiko block.
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

    /// Returns the access list associated with the block, if any.
    fn block_access_list(&self) -> Option<&Bytes> {
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

    /// Returns the number of transactions in the payload.
    fn transaction_count(&self) -> usize {
        self.execution_payload.transactions.as_ref().map_or(0, Vec::len)
    }
}

/// This structure maps on the ExecutionPayload structure of the beacon chain spec.
///
/// See also: <https://github.com/ethereum/execution-apis/blob/6709c2a795b707202e93c4f2867fa0bf2640a84f/src/engine/paris.md#executionpayloadv1>
/// NOTE: we change `transactions` to `Option<Vec<Bytes>>` to ensure backward compatibility with the
/// taiko-client driver behavior.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TaikoExecutionPayloadV1 {
    /// The parent hash of the block.
    pub parent_hash: B256,
    /// The fee recipient of the block.
    pub fee_recipient: Address,
    /// The state root of the block.
    pub state_root: B256,
    /// The receipts root of the block.
    pub receipts_root: B256,
    /// The logs bloom of the block.
    pub logs_bloom: Bloom,
    /// The previous randao of the block.
    pub prev_randao: B256,
    /// The block number.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub block_number: u64,
    /// The gas limit of the block.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub gas_limit: u64,
    /// The gas used of the block.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub gas_used: u64,
    /// The timestamp of the block.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub timestamp: u64,
    /// The extra data of the block.
    pub extra_data: Bytes,
    /// The base fee per gas of the block.
    pub base_fee_per_gas: U256,
    /// The block hash of the block.
    pub block_hash: B256,
    /// The transactions of the block.
    #[serde(default)]
    pub transactions: Option<Vec<Bytes>>,
}

impl From<ExecutionPayloadV1> for TaikoExecutionPayloadV1 {
    // Converts an `ExecutionPayloadV1` into a `TaikoExecutionPayloadV1`.
    fn from(payload: ExecutionPayloadV1) -> Self {
        Self {
            parent_hash: payload.parent_hash,
            fee_recipient: payload.fee_recipient,
            state_root: payload.state_root,
            receipts_root: payload.receipts_root,
            logs_bloom: payload.logs_bloom,
            prev_randao: payload.prev_randao,
            block_number: payload.block_number,
            gas_limit: payload.gas_limit,
            gas_used: payload.gas_used,
            timestamp: payload.timestamp,
            extra_data: payload.extra_data,
            base_fee_per_gas: payload.base_fee_per_gas,
            block_hash: payload.block_hash,
            transactions: Some(payload.transactions),
        }
    }
}

impl From<TaikoExecutionPayloadV1> for ExecutionPayloadV1 {
    // Converts a `TaikoExecutionPayloadV1` into an `ExecutionPayloadV1`.
    fn from(val: TaikoExecutionPayloadV1) -> Self {
        ExecutionPayloadV1 {
            parent_hash: val.parent_hash,
            fee_recipient: val.fee_recipient,
            state_root: val.state_root,
            receipts_root: val.receipts_root,
            logs_bloom: val.logs_bloom,
            prev_randao: val.prev_randao,
            block_number: val.block_number,
            gas_limit: val.gas_limit,
            gas_used: val.gas_used,
            timestamp: val.timestamp,
            extra_data: val.extra_data,
            base_fee_per_gas: val.base_fee_per_gas,
            block_hash: val.block_hash,
            transactions: val.transactions.unwrap_or_default(),
        }
    }
}
