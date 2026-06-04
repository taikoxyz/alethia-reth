//! End-to-end integration test for the L1Sload precompile path.
//!
//! Verifies the full wire-up:
//!
//! ```text
//!   alethia-reth-l1-client    →  install_l1sload_fetcher
//!         ↓                              ↓
//!   reqwest → mock RPC server     alethia-reth-evm::l1sload_run
//!         ↑                              ↑
//!   eth_getStorageAt returns      precompile sees cache-miss,
//!   canned value                   calls fetcher, gets value,
//!                                  caches it, returns to caller
//! ```
//!
//! What this proves about the alethia-reth ↔ raiko2 integration:
//!
//! * The `--l1-rpc-url` CLI flag wiring runs the same code path this test does (`L1RpcClient::new`
//!   + `install_l1sload_fetcher`).
//! * The precompile fetcher callback dispatches HTTP through the L1 RPC client.
//! * The L1 RPC client correctly decodes `eth_getStorageAt`'s `U256` response into `B256` for the
//!   precompile cache.
//! * The precompile caches the served value AND tracks it in `take_l1_rpc_served_calls()`, which is
//!   what raiko2's preflight reads after discovery to know which `(contract, slot, block)` triples
//!   need EIP-1186 proofs.
//!
//! The raiko2 verification half (`verify_and_populate_l1sload_proofs`) is exercised
//! by the unit tests in `raiko2-primitives-shasta`; this test verifies the integration
//! point between the two halves — namely that `take_l1_rpc_served_calls()` after an
//! `l1sload_run` invocation produces the input raiko2 expects.

use std::sync::Arc;

use alethia_reth_evm::precompiles::{
    context::{clear_l1_origin_context, set_l1_origin_block_id},
    l1sload::{
        clear_l1_rpc_fetcher, clear_l1_rpc_served_calls, clear_l1_storage, l1sload_run,
        take_l1_rpc_served_calls,
    },
};
use alethia_reth_l1_client::{L1RpcClient, install_l1sload_fetcher};
use alloy_primitives::{Address, B256, U256};
use jsonrpsee::{RpcModule, server::Server};
use serial_test::serial;
use std::{net::SocketAddr, sync::Mutex};

/// Mock L1 server that returns a canned storage value for any `eth_getStorageAt` request,
/// and records the (contract, slot, block) it was asked about so the test can assert the
/// fetcher passed parameters through correctly.
struct MockL1State {
    /// `(contract, slot, block_hex)` of every request seen.
    seen_requests: Mutex<Vec<(Address, B256, String)>>,
    /// What every `eth_getStorageAt` request returns.
    canned_value: U256,
}

async fn start_mock_l1(
    canned_value: U256,
) -> (jsonrpsee::server::ServerHandle, String, Arc<MockL1State>) {
    let state = Arc::new(MockL1State { seen_requests: Mutex::new(Vec::new()), canned_value });

    let mut module = RpcModule::new(state.clone());
    module
        .register_method("eth_getStorageAt", |params, ctx, _| {
            let (addr, slot, block_hex): (Address, B256, String) = params.parse().unwrap();
            ctx.seen_requests.lock().unwrap().push((addr, slot, block_hex));
            serde_json::to_value(ctx.canned_value).unwrap()
        })
        .unwrap();

    let server =
        Server::builder().build("127.0.0.1:0".parse::<SocketAddr>().unwrap()).await.unwrap();
    let addr = server.local_addr().unwrap();
    let handle = server.start(module);
    (handle, format!("http://{addr}"), state)
}

/// Reset all global precompile state so tests don't interfere with each other.
/// (Precompile caches live in static muteces.)
fn reset_precompile_state() {
    clear_l1_storage();
    clear_l1_rpc_fetcher();
    clear_l1_rpc_served_calls();
    clear_l1_origin_context();
}

/// Build the 84-byte precompile input: 20-byte address + 32-byte slot + 32-byte block (B256).
fn build_l1sload_input(contract: Address, slot: B256, block: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(84);
    buf.extend_from_slice(contract.as_slice());
    buf.extend_from_slice(slot.as_slice());
    buf.extend_from_slice(&U256::from(block).to_be_bytes::<32>());
    buf
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn e2e_l1sload_fetches_from_mock_l1_via_installed_fetcher() {
    reset_precompile_state();

    let canned = U256::from(0xCAFE_BABE_DEAD_BEEFu64);
    let (_server_handle, url, mock_state) = start_mock_l1(canned).await;

    let client = Arc::new(L1RpcClient::new(&url).unwrap());
    let runtime_handle = tokio::runtime::Handle::current();

    // This is the same call sequence `bin/alethia-reth/src/main.rs` runs when the operator
    // supplies `--l1-rpc-url` — the test is exercising the exact wiring.
    install_l1sload_fetcher(client, runtime_handle).expect("multi-thread runtime");

    // The precompile needs an active L1 trust window. Set origin = 200, covering any test
    // block ≤ 200 within the 256-block lookback.
    set_l1_origin_block_id(200);

    let contract = Address::from([0xAAu8; 20]);
    let slot = B256::from([0x01u8; 32]);
    let block = 200u64;
    let input = build_l1sload_input(contract, slot, block);

    // Call the precompile directly on the runtime worker thread. The fetcher closure
    // uses `tokio::task::block_in_place` to bridge sync→async, which requires being on a
    // multi-thread-runtime worker (which `#[tokio::test(flavor = "multi_thread")]` is).
    let result = l1sload_run(&input, 1_000_000, 0).expect("precompile call must succeed");

    // The precompile returns 32 bytes of value. Our mock canned `canned`, so the precompile
    // output must equal `canned` big-endian-encoded.
    let expected_bytes = B256::from(canned.to_be_bytes::<32>());
    assert_eq!(
        result.bytes.as_ref(),
        expected_bytes.as_slice(),
        "precompile must return the mock's canned storage value"
    );

    // The fetcher must have hit the mock exactly once with the parameters we asked for.
    let seen = mock_state.seen_requests.lock().unwrap();
    assert_eq!(seen.len(), 1, "fetcher must dispatch exactly one RPC call");
    assert_eq!(seen[0].0, contract, "fetcher must pass the contract address through");
    assert_eq!(seen[0].1, slot, "fetcher must pass the storage slot through");
    assert_eq!(seen[0].2, format!("0x{block:x}"), "fetcher must pass the block number through");

    // Raiko2's preflight calls `take_l1_rpc_served_calls()` after EVM execution to discover
    // which (contract, slot, block) triples need EIP-1186 proofs. Verify our call appears.
    drop(seen);
    let served = take_l1_rpc_served_calls();
    assert_eq!(served.len(), 1, "precompile must record the served call for preflight");
    let block_b256 = B256::from(U256::from(block).to_be_bytes::<32>());
    assert!(
        served.contains(&(contract, slot, block_b256)),
        "served calls must include our (contract, slot, block) triple, got: {served:?}"
    );

    reset_precompile_state();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn e2e_l1sload_second_call_hits_cache_no_extra_rpc() {
    // Cache behavior: once a (contract, slot, block) triple is served, subsequent precompile
    // calls for the same triple must NOT round-trip to L1. This matters for hot-path L2 gas
    // accounting — the fixed precompile gas is calibrated assuming cache-hit cost, not RPC.
    reset_precompile_state();

    let (_h, url, mock_state) = start_mock_l1(U256::from(0x42u64)).await;
    let client = Arc::new(L1RpcClient::new(&url).unwrap());
    install_l1sload_fetcher(client, tokio::runtime::Handle::current()).expect("multi-thread");
    set_l1_origin_block_id(200);

    let contract = Address::from([0xBBu8; 20]);
    let slot = B256::from([0x02u8; 32]);
    let input = build_l1sload_input(contract, slot, 200);

    let first = l1sload_run(&input, 1_000_000, 0).unwrap();
    let second = l1sload_run(&input, 1_000_000, 0).unwrap();

    assert_eq!(first.bytes, second.bytes, "cache must return identical value");
    assert_eq!(
        mock_state.seen_requests.lock().unwrap().len(),
        1,
        "second call must not dispatch a second RPC — cache must serve it",
    );

    reset_precompile_state();
}
