//! HTTP RPC client used by `alethia-reth` to feed the L1Sload + L1Staticcall precompiles.
//!
//! The precompiles inside `alethia-reth-evm` expose two callback hooks that, when set, let
//! revm fetch from an upstream L1 EL when the in-process cache misses:
//!  * `set_l1_rpc_fetcher` — closure `(Address, B256, u64) -> Result<B256, _>` driving
//!    `eth_getStorageAt` for L1Sload.
//!  * `set_l1_staticcall_rpc_fetcher` — closure `(Address, u64, u64, &[u8]) -> Result<(u64,
//!    Vec<u8>, bool), _>` driving `debug_traceCall` for L1Staticcall.
//!
//! This crate provides the HTTP plumbing behind those closures so the node binary can wire
//! them once at startup against any L1 EL that speaks the standard JSON-RPC methods.
//! Sync↔async bridging uses `tokio::task::block_in_place + Handle::block_on` so the
//! synchronous fetcher signatures expected by revm can dispatch async HTTP requests.

use std::sync::Arc;

use alethia_reth_evm::precompiles::{
    l1sload::set_l1_rpc_fetcher, l1staticcall::set_l1_staticcall_rpc_fetcher,
};
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_client::{ClientBuilder, RpcClient};
use serde::Deserialize;
use tokio::runtime::Handle;
use tracing::{debug, warn};

/// Maximum L1 gas budget a single L1Staticcall fetch may consume. Mirrors `L1STATICCALL_GAS_CAP`
/// shared between the L2 precompile, this live `debug_traceCall` fetcher, and the prover's
/// preflight — keeping them in lockstep prevents sequencer↔prover OOM divergence.
pub const L1STATICCALL_GAS_CAP: u64 = 30_000_000;

/// HTTP-backed L1 RPC client. Wraps `alloy_rpc_client::RpcClient` with the specific RPC
/// methods used by the L1 precompile fetchers + provers.
#[derive(Clone, Debug)]
pub struct L1RpcClient {
    inner: RpcClient,
}

impl L1RpcClient {
    /// Build an HTTP RPC client targeting `url`. Returns an error if the URL is malformed
    /// (e.g. wrong scheme, no host).
    pub fn new(url: &str) -> anyhow::Result<Self> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|e| anyhow::anyhow!("invalid L1 RPC URL ({url}): {e}"))?;
        let inner = ClientBuilder::default().http(parsed);
        Ok(Self { inner })
    }

    /// Fetch the storage value at `(contract, slot, block_n)` via `eth_getStorageAt`.
    pub async fn eth_get_storage_at(
        &self,
        contract: Address,
        slot: B256,
        block_n: u64,
    ) -> anyhow::Result<B256> {
        let block_hex = format!("0x{block_n:x}");
        let value: U256 = self
            .inner
            .request("eth_getStorageAt", (contract, slot, block_hex))
            .await
            .map_err(|e| anyhow::anyhow!("eth_getStorageAt failed: {e}"))?;
        Ok(B256::from(value.to_be_bytes::<32>()))
    }

    /// Fetch the result of a read-only L1 call via `debug_traceCall`. Returns
    /// `(gas_used, return_data, reverted)` so the L1Staticcall fetcher signature can be
    /// produced directly. `from` is pinned to `0x0000…0000` to match the caller account
    /// the ZK guest re-executes under — without that match, the witness produced upstream
    /// covers a different sender's trie nodes and verification fails later.
    pub async fn debug_trace_call(
        &self,
        target: Address,
        gas_limit: u64,
        calldata: &[u8],
        block_n: u64,
    ) -> anyhow::Result<(u64, Vec<u8>, bool)> {
        let call_data_hex = format!("0x{}", hex::encode(calldata));
        let block_id = format!("0x{block_n:x}");
        // L1 budget = min(requested, hard cap) so we never burn more L1 gas per fetch than
        // the L2 precompile would accept.
        let gas_hex = format!("0x{:x}", gas_limit.min(L1STATICCALL_GAS_CAP));

        let resp: TraceCallResult = self
            .inner
            .request(
                "debug_traceCall",
                (
                    serde_json::json!({
                        "from": "0x0000000000000000000000000000000000000000",
                        "to": format!("{target:?}"),
                        "data": call_data_hex,
                        "gas": gas_hex,
                    }),
                    block_id,
                    // Slim the struct-logger payload — we only read gas/returnValue/failed, not
                    // the per-opcode logs, so disable stack/memory/storage capture.
                    serde_json::json!({
                        "disableStorage": true,
                        "disableStack": true,
                        "disableMemory": true,
                    }),
                ),
            )
            .await
            .map_err(|e| anyhow::anyhow!("debug_traceCall failed: {e}"))?;

        if resp.failed {
            return Ok((resp.gas.min(gas_limit), Vec::new(), true));
        }
        let hex_str = resp.return_value.strip_prefix("0x").unwrap_or(&resp.return_value);
        let bytes = hex::decode(hex_str).map_err(|e| anyhow::anyhow!("decode returnValue: {e}"))?;
        Ok((resp.gas.min(gas_limit), bytes, false))
    }
}

/// Subset of `debug_traceCall`'s response that the L1Staticcall fetcher consumes.
/// Other tracer fields (`structLogs`, `stateDiff`, `accessList`, etc.) are ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TraceCallResult {
    gas: u64,
    return_value: String,
    failed: bool,
}

/// Install the L1Sload precompile fetcher backed by `client`. The fetcher closure dispatches
/// `eth_getStorageAt` synchronously by re-entering the tokio runtime via `block_in_place`.
///
/// `handle` must outlive the precompile fetcher — store it on a long-lived component
/// (typically the node's tokio runtime handle). On RPC failures, the closure logs at
/// `warn` and returns the error string verbatim so revm surfaces it as a precompile halt.
pub fn install_l1sload_fetcher(client: Arc<L1RpcClient>, handle: Handle) {
    set_l1_rpc_fetcher(move |contract, slot, block_n| {
        let client = client.clone();
        let handle = handle.clone();
        tokio::task::block_in_place(move || {
            handle.block_on(async move {
                client.eth_get_storage_at(contract, slot, block_n).await.map_err(|e| {
                    warn!(?contract, ?slot, block_n, "L1Sload fetch failed: {e}");
                    e.to_string()
                })
            })
        })
    });
    debug!("L1Sload RPC fetcher installed");
}

/// Install the L1Staticcall precompile fetcher backed by `client`. Same `block_in_place`
/// pattern as [`install_l1sload_fetcher`].
pub fn install_l1staticcall_fetcher(client: Arc<L1RpcClient>, handle: Handle) {
    set_l1_staticcall_rpc_fetcher(move |target, block_n, gas_limit, calldata| {
        let client = client.clone();
        let handle = handle.clone();
        let calldata_owned = calldata.to_vec();
        tokio::task::block_in_place(move || {
            handle.block_on(async move {
                client.debug_trace_call(target, gas_limit, &calldata_owned, block_n).await.map_err(
                    |e| {
                        warn!(?target, block_n, gas_limit, "L1Staticcall fetch failed: {e}");
                        e.to_string()
                    },
                )
            })
        })
    });
    debug!("L1Staticcall RPC fetcher installed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::{RpcModule, server::Server};
    use std::net::SocketAddr;

    /// Spin up a localhost jsonrpsee server that answers the L1 RPC methods we care about.
    /// Returns `(server_handle, base_url)` so tests can build an `L1RpcClient` targeting it.
    async fn mock_server() -> (jsonrpsee::server::ServerHandle, String) {
        let mut module = RpcModule::new(());
        module
            .register_method("eth_getStorageAt", |params, _, _| {
                // params: (address, slot, block) — we ignore them and return a canned U256.
                let _: (Address, B256, String) = params.parse().unwrap();
                serde_json::to_value(U256::from(0xDEAD_BEEFu64)).unwrap()
            })
            .unwrap();
        module
            .register_method("debug_traceCall", |_params, _, _| {
                serde_json::json!({
                    "gas": 21_000,
                    "returnValue": "0xdeadbeef",
                    "failed": false,
                })
            })
            .unwrap();
        let server =
            Server::builder().build("127.0.0.1:0".parse::<SocketAddr>().unwrap()).await.unwrap();
        let addr = server.local_addr().unwrap();
        let handle = server.start(module);
        (handle, format!("http://{addr}"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_eth_get_storage_at_returns_b256() {
        let (_h, url) = mock_server().await;
        let client = L1RpcClient::new(&url).unwrap();
        let value = client
            .eth_get_storage_at(Address::from([0x42u8; 20]), B256::from([0x01u8; 32]), 100)
            .await
            .unwrap();
        // U256(0xDEAD_BEEF) → B256 (big-endian, zero-padded)
        assert_eq!(value, B256::from(U256::from(0xDEAD_BEEFu64).to_be_bytes::<32>()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_debug_trace_call_decodes_success() {
        let (_h, url) = mock_server().await;
        let client = L1RpcClient::new(&url).unwrap();
        let (gas_used, output, reverted) = client
            .debug_trace_call(Address::from([0xABu8; 20]), 1_000_000, &[0x01, 0x02], 100)
            .await
            .unwrap();
        assert_eq!(gas_used, 21_000);
        assert_eq!(output, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(!reverted);
    }

    #[test]
    fn test_new_rejects_malformed_url() {
        let err = L1RpcClient::new("not-a-url").unwrap_err();
        assert!(err.to_string().contains("invalid L1 RPC URL"));
    }
}
