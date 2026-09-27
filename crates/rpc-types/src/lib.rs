#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Lightweight request/response types for the `taikoAuth` RPC namespace.
//!
//! This crate contains only serializable types so that downstream consumers
//! (e.g. taiko-client-rs) can avoid depending on the full RPC server
//! infrastructure provided by `alethia-reth-rpc`.

use alloy_primitives::{Address, B256, Bytes};
use serde::{Deserialize, Serialize};

/// Target block values required to simulate tx-pool candidates under the intended fork rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxPoolBlockContext {
    /// Target block timestamp encoded as an Ethereum JSON-RPC quantity.
    #[serde(with = "alloy_serde::quantity")]
    pub timestamp: u64,
    /// Parent beacon block root supplied to the EIP-4788 system contract call.
    pub parent_beacon_block_root: B256,
    /// Exact target block extra data used for fork-specific fee context.
    pub extra_data: Bytes,
}

/// A pre-built transaction list that contains the mempool content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreBuiltTxList<T> {
    /// Selected transactions encoded for RPC response delivery.
    pub tx_list: Vec<T>,
    /// Estimated gas used by all transactions in `tx_list`.
    pub estimated_gas_used: u64,
    /// Total transaction-list byte length used for DA constraints.
    pub bytes_length: u64,
}

impl<T> Default for PreBuiltTxList<T> {
    /// Creates an empty pre-built transaction list.
    fn default() -> Self {
        Self { tx_list: vec![], estimated_gas_used: 0, bytes_length: 0 }
    }
}

/// Request payload for `taikoAuth_txPoolContent`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxPoolContentParams {
    /// Fee-recipient address used while simulating candidate transaction lists.
    pub beneficiary: Address,
    /// Base fee applied to candidate transaction-list construction.
    pub base_fee: u64,
    /// Maximum gas limit allocated per candidate transaction list.
    pub block_max_gas_limit: u64,
    /// Maximum DA bytes allowed per candidate transaction list.
    pub max_bytes_per_tx_list: u64,
    /// Optional local addresses to prioritize during tx-pool selection.
    pub locals: Option<Vec<Address>>,
    /// Maximum number of candidate transaction lists to return.
    pub max_transactions_lists: u64,
    /// Optional target block values for fork-aware candidate simulation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_context: Option<TxPoolBlockContext>,
}

/// Request payload for `taikoAuth_txPoolContentWithMinTip`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxPoolContentWithMinTipParams {
    /// Fee-recipient address used while simulating candidate transaction lists.
    pub beneficiary: Address,
    /// Base fee applied to candidate transaction-list construction.
    pub base_fee: u64,
    /// Maximum gas limit allocated per candidate transaction list.
    pub block_max_gas_limit: u64,
    /// Maximum DA bytes allowed per candidate transaction list.
    pub max_bytes_per_tx_list: u64,
    /// Optional local addresses to prioritize during tx-pool selection.
    pub locals: Option<Vec<Address>>,
    /// Maximum number of candidate transaction lists to return.
    pub max_transactions_lists: u64,
    /// Minimum transaction tip required for inclusion.
    pub min_tip: u64,
    /// Optional target block values for fork-aware candidate simulation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_context: Option<TxPoolBlockContext>,
}

impl From<TxPoolContentParams> for TxPoolContentWithMinTipParams {
    /// Converts base tx-pool query parameters into the min-tip variant with `min_tip = 0`.
    fn from(params: TxPoolContentParams) -> Self {
        let TxPoolContentParams {
            beneficiary,
            base_fee,
            block_max_gas_limit,
            max_bytes_per_tx_list,
            locals,
            max_transactions_lists,
            block_context,
        } = params;
        Self {
            beneficiary,
            base_fee,
            block_max_gas_limit,
            max_bytes_per_tx_list,
            locals,
            max_transactions_lists,
            min_tip: 0,
            block_context,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, Bytes};
    use serde_json::json;

    #[test]
    fn tx_pool_block_context_uses_quantity_and_alloy_hex_encoding() {
        let context = TxPoolBlockContext {
            timestamp: 100,
            parent_beacon_block_root: B256::with_last_byte(1),
            extra_data: Bytes::from(vec![0; 7]),
        };

        let value = serde_json::to_value(&context).unwrap();

        assert_eq!(value["timestamp"], "0x64");
        assert_eq!(value["parentBeaconBlockRoot"], format!("{:#x}", B256::with_last_byte(1)));
        assert_eq!(value["extraData"], "0x00000000000000");
        assert_eq!(serde_json::from_value::<TxPoolBlockContext>(value).unwrap(), context);
    }

    #[test]
    fn legacy_tx_pool_params_round_trip_without_block_context() {
        let legacy = json!({
            "beneficiary": Address::from([0x11; 20]),
            "baseFee": 10,
            "blockMaxGasLimit": 15_000_000,
            "maxBytesPerTxList": 120_000,
            "locals": [Address::from([0x22; 20])],
            "maxTransactionsLists": 4
        });

        let params: TxPoolContentParams = serde_json::from_value(legacy.clone()).unwrap();

        assert_eq!(params.block_context, None);
        assert_eq!(serde_json::to_value(params).unwrap(), legacy);
    }

    #[test]
    fn legacy_min_tip_params_round_trip_without_block_context() {
        let legacy = json!({
            "beneficiary": Address::from([0x33; 20]),
            "baseFee": 20,
            "blockMaxGasLimit": 20_000_000,
            "maxBytesPerTxList": 240_000,
            "locals": null,
            "maxTransactionsLists": 8,
            "minTip": 2
        });

        let params: TxPoolContentWithMinTipParams = serde_json::from_value(legacy.clone()).unwrap();

        assert_eq!(params.block_context, None);
        assert_eq!(serde_json::to_value(params).unwrap(), legacy);
    }

    #[test]
    fn tx_pool_params_conversion_preserves_block_context() {
        let context = TxPoolBlockContext {
            timestamp: 100,
            parent_beacon_block_root: B256::with_last_byte(1),
            extra_data: Bytes::from(vec![0; 7]),
        };
        let params = TxPoolContentParams {
            beneficiary: Address::ZERO,
            base_fee: 0,
            block_max_gas_limit: 30_000_000,
            max_bytes_per_tx_list: 120_000,
            locals: None,
            max_transactions_lists: 1,
            block_context: Some(context.clone()),
        };

        let converted = TxPoolContentWithMinTipParams::from(params);

        assert_eq!(converted.block_context, Some(context));
    }

    #[test]
    fn tx_pool_block_context_requires_parent_beacon_block_root() {
        let value = json!({
            "timestamp": "0x64",
            "extraData": "0x00000000000000"
        });

        assert!(serde_json::from_value::<TxPoolBlockContext>(value).is_err());
    }
}
