//! Request/response types for the `taikoAuth` RPC namespace.
//!
//! Re-exported from [`alethia_reth_rpc_types`] so that lightweight consumers
//! can depend on the types crate alone.

pub use alethia_reth_rpc_types::{
    PreBuiltTxList, TxPoolContentParams, TxPoolContentWithMinTipParams,
};
