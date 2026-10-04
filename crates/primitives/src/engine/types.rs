//! Taiko execution payload and sidecar representations.
use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
use alloy_rpc_types_engine::{
    ExecutionPayload, ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3,
};
use alloy_rpc_types_eth::Withdrawal;
use reth_payload_primitives::ExecutionPayload as ExecutionPayloadTr;

/// Represents the execution data for the Taiko network, which includes the execution payload and a
/// sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TaikoExecutionData {
    /// Base execution payload fields returned to the engine API.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub execution_payload: TaikoExecutionPayloadV1,
    /// Taiko-specific sidecar metadata paired with the execution payload.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub taiko_sidecar: TaikoExecutionDataSidecar,
}

impl TaikoExecutionData {
    /// Creates a new instance of `ExecutionPayload`.
    pub fn into_payload(self) -> ExecutionPayload {
        let Self { execution_payload, taiko_sidecar } = self;
        let payload_inner = ExecutionPayloadV1::from(execution_payload);
        match taiko_sidecar.osaka {
            Some(osaka) => ExecutionPayload::V3(ExecutionPayloadV3 {
                payload_inner: ExecutionPayloadV2 { payload_inner, withdrawals: osaka.withdrawals },
                blob_gas_used: osaka.blob_gas_used,
                excess_blob_gas: osaka.excess_blob_gas,
            }),
            None => ExecutionPayload::V1(payload_inner),
        }
    }
}

impl From<TaikoExecutionData> for ExecutionPayload {
    /// Converts Taiko execution data into the engine `ExecutionPayload` enum.
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
    /// Transactions root hash for the payload.
    pub tx_hash: B256,
    /// Optional withdrawals root hash for the payload.
    pub withdrawals_hash: Option<B256>,
    /// Optional hash-relevant header difficulty restored from the original built block.
    pub header_difficulty: Option<U256>,
    /// Marker flag indicating whether this payload is a Taiko block.
    pub taiko_block: Option<bool>,
    /// Inbound Amsterdam block access list retained only so engine validation can reject it.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing))]
    pub block_access_list: Option<Bytes>,
    /// Inbound Amsterdam slot number retained only so engine validation can reject it.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing, with = "alloy_serde::quantity::opt")
    )]
    pub slot_number: Option<u64>,
    /// Hash-relevant Osaka payload fields retained for lossless block reconstruction.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub osaka: Option<TaikoOsakaPayloadFields>,
}

/// Hash-relevant V3 payload and `newPayloadV4` side parameters retained internally.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TaikoOsakaPayloadFields {
    /// Withdrawals whose trie root is committed by the reconstructed block header.
    pub withdrawals: Vec<Withdrawal>,
    /// Blob gas consumed by the block.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub blob_gas_used: u64,
    /// Excess blob gas carried into the block.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub excess_blob_gas: u64,
    /// Parent beacon block root committed by the block header.
    pub parent_beacon_block_root: B256,
    /// Blob hashes supplied alongside `newPayloadV4` for transaction consistency checks.
    pub expected_blob_versioned_hashes: Vec<B256>,
    /// Opaque EIP-7685 request values supplied alongside `newPayloadV4`.
    pub execution_requests: Vec<Bytes>,
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
        self.taiko_sidecar.osaka.as_ref().map(|osaka| &osaka.withdrawals)
    }

    /// Returns the access list associated with the block, if any.
    fn block_access_list(&self) -> Option<&Bytes> {
        self.taiko_sidecar.block_access_list.as_ref()
    }

    /// Returns the parent beacon block root, if applicable.
    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.taiko_sidecar.osaka.as_ref().map(|osaka| osaka.parent_beacon_block_root)
    }

    /// Returns the timestamp of the block.
    fn timestamp(&self) -> u64 {
        self.execution_payload.timestamp
    }

    /// Returns the gas used in the block.
    fn gas_used(&self) -> u64 {
        self.execution_payload.gas_used
    }

    /// Returns the gas limit of the block.
    fn gas_limit(&self) -> u64 {
        self.execution_payload.gas_limit
    }

    /// Returns the number of transactions in the payload.
    fn transaction_count(&self) -> usize {
        self.execution_payload.transactions.as_ref().map_or(0, Vec::len)
    }

    /// Returns the slot number for the payload. Taiko payloads do not carry a beacon slot.
    fn slot_number(&self) -> Option<u64> {
        self.taiko_sidecar.slot_number
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
    /// Keccak256 hash of the parent header, used to link this payload to the canonical chain.
    pub parent_hash: B256,
    /// Coinbase account that receives execution-layer priority fees.
    pub fee_recipient: Address,
    /// Post-state trie root after executing all transactions in this payload.
    pub state_root: B256,
    /// Trie root over all transaction receipts produced by this payload.
    pub receipts_root: B256,
    /// Bloom filter aggregating receipt logs for fast topic/address matching.
    pub logs_bloom: Bloom,
    /// Beacon RANDAO mix committed into the payload header.
    pub prev_randao: B256,
    /// L2 block height represented by this payload.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub block_number: u64,
    /// Maximum total gas that can be consumed by transactions in this payload.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub gas_limit: u64,
    /// Actual gas consumed by transaction execution.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub gas_used: u64,
    /// Block timestamp (seconds since Unix epoch) chosen for this payload.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub timestamp: u64,
    /// Opaque protocol-specific bytes committed in the header.
    pub extra_data: Bytes,
    /// Base fee per gas applied to transactions in this payload.
    pub base_fee_per_gas: U256,
    /// Header hash asserted by the builder/engine for this payload.
    pub block_hash: B256,
    /// RLP-encoded signed transactions; `None` preserves legacy optional-transaction behavior.
    #[serde(default)]
    pub transactions: Option<Vec<Bytes>>,
}

impl From<ExecutionPayloadV1> for TaikoExecutionPayloadV1 {
    /// Converts an `ExecutionPayloadV1` into a `TaikoExecutionPayloadV1`.
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
    /// Converts a `TaikoExecutionPayloadV1` into an `ExecutionPayloadV1`.
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

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::{TaikoExecutionData, TaikoExecutionDataSidecar, TaikoExecutionPayloadV1};
    use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
    use serde_json::json;

    #[test]
    fn legacy_execution_data_json_has_no_osaka_fields() {
        let data = TaikoExecutionData {
            execution_payload: TaikoExecutionPayloadV1 {
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number: 1,
                gas_limit: 2,
                gas_used: 3,
                timestamp: 4,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::from(5),
                block_hash: B256::ZERO,
                transactions: Some(Vec::new()),
            },
            taiko_sidecar: TaikoExecutionDataSidecar {
                tx_hash: B256::ZERO,
                withdrawals_hash: None,
                header_difficulty: Some(U256::from(6)),
                taiko_block: Some(true),
                block_access_list: None,
                slot_number: None,
                osaka: None,
            },
        };

        let value = serde_json::to_value(data).unwrap();
        assert!(value.get("osaka").is_none(), "{value}");
        assert_eq!(value["headerDifficulty"], json!("0x6"));
        assert_eq!(value.get("withdrawalsHash"), Some(&json!(null)));
        assert_eq!(value["taikoBlock"], json!(true));
    }
}
