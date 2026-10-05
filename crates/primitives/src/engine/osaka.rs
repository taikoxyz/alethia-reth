//! Osaka execution-payload wire types and lossless internal normalization.

use crate::engine::types::{TaikoExecutionData, TaikoExecutionDataSidecar};
use alloy_consensus::proofs::{calculate_withdrawals_root, ordered_trie_root_encoded};
use alloy_primitives::{B256, Bytes, U256};
use alloy_rpc_types_engine::ExecutionPayloadV3;
use core::fmt;
#[cfg(feature = "serde")]
use std::collections::BTreeMap;

pub use crate::engine::types::TaikoOsakaPayloadFields;

/// Osaka engine payload with Taiko's finalized zk-gas value in decimal JSON form.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TaikoExecutionPayloadV3 {
    /// Standard Cancun payload fields, including a mandatory transaction array.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub execution_payload: ExecutionPayloadV3,
    /// Finalized block zk-gas, encoded as a required decimal JSON integer.
    pub header_difficulty: u64,
    /// Unrecognized flattened properties retained until normalization rejects them.
    #[cfg(feature = "serde")]
    #[serde(flatten)]
    pub extra_fields: BTreeMap<String, serde_json::Value>,
}

/// Error returned when an Osaka payload contains fields outside its wire contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaikoOsakaInputError {
    /// Sorted names of extension properties that cannot be normalized safely.
    pub unexpected_fields: Vec<String>,
}

impl fmt::Display for TaikoOsakaInputError {
    /// Lists the extension properties that made normalization fail closed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unsupported Osaka payload fields: {}", self.unexpected_fields.join(", "))
    }
}

impl core::error::Error for TaikoOsakaInputError {}

impl TaikoExecutionPayloadV3 {
    /// Normalizes the Osaka wire payload and its `newPayloadV4` side parameters without dropping
    /// hash-relevant inputs.
    pub fn into_execution_data(
        self,
        expected_blob_versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Vec<Bytes>,
    ) -> Result<TaikoExecutionData, TaikoOsakaInputError> {
        #[cfg(feature = "serde")]
        if !self.extra_fields.is_empty() {
            return Err(TaikoOsakaInputError {
                unexpected_fields: self.extra_fields.into_keys().collect(),
            });
        }

        let ExecutionPayloadV3 { payload_inner, blob_gas_used, excess_blob_gas } =
            self.execution_payload;
        let withdrawals = payload_inner.withdrawals;
        let payload = payload_inner.payload_inner;
        let tx_hash = ordered_trie_root_encoded(&payload.transactions);
        let withdrawals_hash = Some(calculate_withdrawals_root(&withdrawals));

        Ok(TaikoExecutionData {
            execution_payload: payload.into(),
            taiko_sidecar: TaikoExecutionDataSidecar {
                tx_hash,
                withdrawals_hash,
                header_difficulty: Some(U256::from(self.header_difficulty)),
                taiko_block: Some(true),
                block_access_list: None,
                slot_number: None,
                osaka: Some(TaikoOsakaPayloadFields {
                    withdrawals,
                    blob_gas_used,
                    excess_blob_gas,
                    parent_beacon_block_root,
                    expected_blob_versioned_hashes,
                    execution_requests,
                }),
            },
        })
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::TaikoExecutionPayloadV3;
    use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
    use serde_json::{Value, json};

    fn execution_payload_v3() -> ExecutionPayloadV3 {
        ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: B256::ZERO,
                    fee_recipient: Address::ZERO,
                    state_root: B256::ZERO,
                    receipts_root: B256::ZERO,
                    logs_bloom: Bloom::ZERO,
                    prev_randao: B256::ZERO,
                    block_number: 0,
                    gas_limit: 0,
                    gas_used: 0,
                    timestamp: 0,
                    extra_data: Bytes::new(),
                    base_fee_per_gas: U256::ZERO,
                    block_hash: B256::ZERO,
                    transactions: Vec::new(),
                },
                withdrawals: Vec::new(),
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        }
    }

    fn wire_value(header_difficulty: Value) -> Value {
        let mut value = serde_json::to_value(execution_payload_v3()).unwrap();
        value["headerDifficulty"] = header_difficulty;
        value
    }

    #[test]
    fn header_difficulty_rejects_missing_null_and_overflow() {
        let mut missing = wire_value(json!(0));
        missing.as_object_mut().unwrap().remove("headerDifficulty");
        assert!(serde_json::from_value::<TaikoExecutionPayloadV3>(missing).is_err());
        assert!(
            serde_json::from_value::<TaikoExecutionPayloadV3>(wire_value(Value::Null)).is_err()
        );
        let overflow = serde_json::to_string(&wire_value(json!(0)))
            .unwrap()
            .replace("\"headerDifficulty\":0", "\"headerDifficulty\":18446744073709551616");
        assert!(serde_json::from_str::<TaikoExecutionPayloadV3>(&overflow).is_err());
    }

    #[test]
    fn transaction_array_is_required_but_may_be_empty() {
        let mut missing = wire_value(json!(0));
        missing.as_object_mut().unwrap().remove("transactions");
        assert!(serde_json::from_value::<TaikoExecutionPayloadV3>(missing).is_err());

        let mut null = wire_value(json!(0));
        null["transactions"] = Value::Null;
        assert!(serde_json::from_value::<TaikoExecutionPayloadV3>(null).is_err());

        let mut empty = wire_value(json!(0));
        empty["transactions"] = json!([]);
        let payload: TaikoExecutionPayloadV3 = serde_json::from_value(empty).unwrap();
        assert!(payload.execution_payload.payload_inner.payload_inner.transactions.is_empty());
    }

    #[test]
    fn normalization_rejects_legacy_override_and_amsterdam_fields() {
        for field in
            ["txHash", "withdrawalsHash", "blockAccessList", "slotNumber", "targetGasLimit"]
        {
            let mut value = wire_value(json!(0));
            value[field] = match field {
                "slotNumber" | "targetGasLimit" => json!("0x1"),
                "blockAccessList" => json!("0x"),
                _ => json!(B256::with_last_byte(1)),
            };
            let payload: TaikoExecutionPayloadV3 = serde_json::from_value(value).unwrap();
            assert!(
                payload
                    .into_execution_data(Vec::new(), B256::with_last_byte(2), Vec::new())
                    .is_err(),
                "field {field} must not be discarded"
            );
        }
    }

    #[test]
    fn normalization_preserves_all_osaka_inputs_and_derives_body_roots() {
        let mut value = wire_value(json!(73));
        value["blobGasUsed"] = json!("0x5");
        value["excessBlobGas"] = json!("0x7");
        let wire: TaikoExecutionPayloadV3 = serde_json::from_value(value).unwrap();
        let root = B256::with_last_byte(0x22);
        let hashes = vec![B256::with_last_byte(0x33)];
        let requests = vec![Bytes::from_static(&[1, 2])];

        let data = wire.into_execution_data(hashes.clone(), root, requests.clone()).unwrap();
        let osaka = data.taiko_sidecar.osaka.unwrap();

        assert_eq!(data.taiko_sidecar.header_difficulty, Some(U256::from(73)));
        assert_eq!(osaka.parent_beacon_block_root, root);
        assert_eq!(osaka.blob_gas_used, 5);
        assert_eq!(osaka.excess_blob_gas, 7);
        assert_eq!(osaka.expected_blob_versioned_hashes, hashes);
        assert_eq!(osaka.execution_requests, requests);
        assert!(osaka.withdrawals.is_empty());
        assert_eq!(data.execution_payload.transactions, Some(Vec::new()));
        assert_eq!(data.taiko_sidecar.tx_hash, alloy_consensus::EMPTY_ROOT_HASH);
        assert_eq!(data.taiko_sidecar.withdrawals_hash, Some(alloy_consensus::EMPTY_ROOT_HASH));
    }
}
