#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Lightweight request/response types for the `taikoAuth` RPC namespace.
//!
//! This crate contains only serializable types so that downstream consumers
//! (e.g. taiko-client-rs) can avoid depending on the full RPC server
//! infrastructure provided by `alethia-reth-rpc`.

use alloy_primitives::Address;
use serde::{Deserialize, Serialize};

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
        } = params;
        Self {
            beneficiary,
            base_fee,
            block_max_gas_limit,
            max_bytes_per_tx_list,
            locals,
            max_transactions_lists,
            min_tip: 0,
        }
    }
}
