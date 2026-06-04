//! End-to-end integration test for the L1Staticcall precompile path (T5).
//!
//! Mirrors `e2e_l1sload.rs` for L1Sload — verifies the wire-up from
//! `install_l1staticcall_fetcher` through `debug_traceCall` against a mock RPC server, with
//! both happy-path and reverted-call sanitization variants.

use std::sync::Arc;

use alethia_reth_evm::precompiles::{
    context::{clear_l1_origin_context, set_l1_origin_block_id},
    l1staticcall::{
        clear_l1_staticcall_cache, clear_l1_staticcall_rpc_fetcher,
        clear_l1_staticcall_rpc_served_calls, l1staticcall_run, take_l1_staticcall_rpc_served_calls,
    },
};
use alethia_reth_l1_client::{L1RpcClient, install_l1staticcall_fetcher};
use alloy_primitives::{Address, U256};
use jsonrpsee::{RpcModule, server::Server};
use serial_test::serial;
use std::net::SocketAddr;

async fn start_mock_l1(
    gas: u64,
    return_hex: &'static str,
    failed: bool,
) -> (jsonrpsee::server::ServerHandle, String) {
    let mut module = RpcModule::new(());
    module
        .register_method("debug_traceCall", move |_params, _, _| {
            serde_json::json!({
                "gas": gas,
                "returnValue": return_hex,
                "failed": failed,
            })
        })
        .unwrap();
    let server =
        Server::builder().build("127.0.0.1:0".parse::<SocketAddr>().unwrap()).await.unwrap();
    let addr = server.local_addr().unwrap();
    let handle = server.start(module);
    (handle, format!("http://{addr}"))
}

fn reset_precompile_state() {
    clear_l1_staticcall_cache();
    clear_l1_staticcall_rpc_fetcher();
    clear_l1_staticcall_rpc_served_calls();
    clear_l1_origin_context();
}

/// Build the L1Staticcall precompile input: 20-byte target + 32-byte block + calldata.
fn build_input(target: Address, block: u64, calldata: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(52 + calldata.len());
    buf.extend_from_slice(target.as_slice());
    buf.extend_from_slice(&U256::from(block).to_be_bytes::<32>());
    buf.extend_from_slice(calldata);
    buf
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn e2e_l1staticcall_fetches_from_mock_l1_via_installed_fetcher() {
    reset_precompile_state();
    let (_server_handle, url) = start_mock_l1(21_000, "0xdeadbeef", false).await;

    let client = Arc::new(L1RpcClient::new(&url).unwrap());
    install_l1staticcall_fetcher(client, tokio::runtime::Handle::current())
        .expect("multi-thread runtime");

    set_l1_origin_block_id(200);
    let target = Address::from([0xAAu8; 20]);
    let calldata = vec![0x01, 0x02];
    let input = build_input(target, 200, &calldata);

    let result = l1staticcall_run(&input, 1_000_000, 0).expect("precompile call must succeed");
    assert_eq!(
        result.bytes.as_ref(),
        &[0xDE, 0xAD, 0xBE, 0xEF],
        "precompile must return the mock's canned return data"
    );

    let served = take_l1_staticcall_rpc_served_calls();
    assert_eq!(served.len(), 1, "served call must be recorded for preflight");
    assert_eq!(served[0].target, target);
    assert_eq!(served[0].block_number, 200);
    assert_eq!(served[0].calldata, calldata);
    assert_eq!(served[0].return_data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    assert_eq!(served[0].gas_used, 21_000);
    assert!(!served[0].is_reverted);

    reset_precompile_state();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn e2e_l1staticcall_reverted_response_sanitized_in_record() {
    // T7-symmetric: a reverted response (failed=true) must produce a served-call record
    // with gas=0 and empty data, regardless of what the upstream L1 EL reports.
    reset_precompile_state();
    let (_h, url) = start_mock_l1(99_999, "0xcafebabe", true).await;

    let client = Arc::new(L1RpcClient::new(&url).unwrap());
    install_l1staticcall_fetcher(client, tokio::runtime::Handle::current())
        .expect("multi-thread runtime");

    set_l1_origin_block_id(200);
    let target = Address::from([0xBBu8; 20]);
    let input = build_input(target, 200, &[0x99]);

    let result = l1staticcall_run(&input, 1_000_000, 0).expect("precompile returns halt for revert");
    assert!(result.is_halt(), "reverted L1 staticcall must halt");

    let served = take_l1_staticcall_rpc_served_calls();
    assert_eq!(served.len(), 1);
    assert!(served[0].is_reverted);
    // Note: the client itself sanitizes `gas` to `min(reported, requested)` and returns
    // `(gas, vec![], true)` on failure. The precompile fetcher path then zeros gas + data
    // for the cache + served-call record (S/T7).
    assert_eq!(served[0].gas_used, 0, "reverted served-call must have gas=0");
    assert!(
        served[0].return_data.is_empty(),
        "reverted served-call must have empty data"
    );

    reset_precompile_state();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn e2e_l1staticcall_second_call_hits_cache_no_extra_rpc() {
    // Cache behavior: once a (target, block, calldata) triple is served, subsequent precompile
    // calls for the same triple must NOT round-trip to L1.
    reset_precompile_state();
    let (_h, url) = start_mock_l1(10_000, "0xab", false).await;
    let client = Arc::new(L1RpcClient::new(&url).unwrap());
    install_l1staticcall_fetcher(client, tokio::runtime::Handle::current())
        .expect("multi-thread runtime");
    set_l1_origin_block_id(200);

    let target = Address::from([0xCCu8; 20]);
    let calldata = vec![0xAA, 0xBB];
    let input = build_input(target, 200, &calldata);

    let first = l1staticcall_run(&input, 1_000_000, 0).unwrap();
    let second = l1staticcall_run(&input, 1_000_000, 0).unwrap();
    assert_eq!(first.bytes, second.bytes, "cache must return identical value");

    // Only ONE served-call record (first call). Cache hit on the second.
    let served = take_l1_staticcall_rpc_served_calls();
    assert_eq!(served.len(), 1, "second call must hit cache, not the RPC");

    reset_precompile_state();
}
