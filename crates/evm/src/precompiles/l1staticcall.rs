use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

use alloy_primitives::{keccak256, Address, B256, Bytes, U256};
use reth_revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};
use tracing::{debug, trace, warn};

use super::l1sload::{get_anchor_block_id, get_l1_origin_block_id};

/// Fixed gas cost for an L1STATICCALL precompile call.
const L1STATICCALL_FIXED_GAS: u64 = 2000;
/// Per-call overhead gas cost.
const L1STATICCALL_PER_CALL_OVERHEAD: u64 = 10000;
/// Per-byte gas cost for calldata beyond the minimum 52-byte header.
const L1STATICCALL_PER_BYTE_CALLDATA_GAS: u64 = 16;

/// Minimum input length: 20 bytes (address) + 32 bytes (block number) = 52 bytes.
/// Calldata beyond this is optional (variable-length).
const MIN_INPUT_LENGTH: usize = 52;

/// Maximum size of return data from an L1STATICCALL (24 KB).
const MAX_RETURN_DATA_SIZE: usize = 24576;

/// Maximum number of L1 blocks to look back from L1 origin.
const L1STATICCALL_MAX_BLOCK_LOOKBACK: u64 = 256;

/// Type alias for the L1 staticcall cache map.
/// Key: (target_address, block_number, keccak256(calldata)) -> Value: return_data
type L1StaticCallCache = HashMap<(Address, u64, B256), Vec<u8>>;

/// In-memory cache for L1 staticcall results.
static L1_STATICCALL_CACHE: LazyLock<Mutex<L1StaticCallCache>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Callback function type for fetching L1 staticcall results via RPC.
/// Arguments: (target_address, block_number, calldata_bytes) -> return_data
type L1StaticCallFetcher =
    Box<dyn Fn(Address, u64, &[u8]) -> Result<Vec<u8>, String> + Send + Sync>;

/// Live L1 RPC fetcher for handling cache misses on L1STATICCALL calls.
static L1_STATICCALL_RPC_FETCHER: LazyLock<Mutex<Option<L1StaticCallFetcher>>> =
    LazyLock::new(|| Mutex::new(None));

/// Record of a single L1STATICCALL served via the live RPC fetcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct L1StaticCallRecord {
    /// The L1 contract address that was called.
    pub target: Address,
    /// The L1 block number at which the call was made.
    pub block_number: u64,
    /// The calldata sent to the L1 contract.
    pub calldata: Vec<u8>,
    /// The return data from the L1 contract call.
    pub return_data: Vec<u8>,
}

/// Tracks L1STATICCALL calls served via the live RPC fetcher (not from pre-fetched cache).
/// surge-raiko reads this after execution to fetch proofs for ZK prover.
static L1_STATICCALL_RPC_SERVED_CALLS: LazyLock<Mutex<Vec<L1StaticCallRecord>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Insert a value into the L1 staticcall cache.
/// The cache key uses `keccak256(calldata)` so variable-length calldata maps to a fixed-size key.
pub fn set_l1_staticcall_value(
    target: Address,
    block_number: u64,
    calldata: &[u8],
    result: Vec<u8>,
) {
    let calldata_hash = keccak256(calldata);
    let mut cache = L1_STATICCALL_CACHE.lock().expect("L1_STATICCALL_CACHE mutex poisoned");
    cache.insert((target, block_number, calldata_hash), result);
}

/// Clear the L1STATICCALL cache only (does NOT clear the shared anchor/l1origin context).
pub fn clear_l1_staticcall_cache() {
    L1_STATICCALL_CACHE.lock().expect("L1_STATICCALL_CACHE mutex poisoned").clear();
}

/// Set the L1 staticcall RPC fetcher callback for live fetching on cache miss.
pub fn set_l1_staticcall_rpc_fetcher(
    fetcher: impl Fn(Address, u64, &[u8]) -> Result<Vec<u8>, String> + Send + Sync + 'static,
) {
    *L1_STATICCALL_RPC_FETCHER.lock().expect("L1_STATICCALL_RPC_FETCHER mutex poisoned") =
        Some(Box::new(fetcher));
}

/// Clear the L1 staticcall RPC fetcher (disables live RPC fallback).
pub fn clear_l1_staticcall_rpc_fetcher() {
    *L1_STATICCALL_RPC_FETCHER.lock().expect("L1_STATICCALL_RPC_FETCHER mutex poisoned") = None;
}

/// Get all L1STATICCALL calls that were served via live RPC (not from cache).
/// Used by surge-raiko after execution to replay and verify these calls.
pub fn take_l1_staticcall_rpc_served_calls() -> Vec<L1StaticCallRecord> {
    std::mem::take(
        &mut *L1_STATICCALL_RPC_SERVED_CALLS
            .lock()
            .expect("L1_STATICCALL_RPC_SERVED_CALLS mutex poisoned"),
    )
}

/// Clear tracked calls served via live L1 RPC.
pub fn clear_l1_staticcall_rpc_served_calls() {
    L1_STATICCALL_RPC_SERVED_CALLS
        .lock()
        .expect("L1_STATICCALL_RPC_SERVED_CALLS mutex poisoned")
        .clear();
}

/// Looks up a cached L1 staticcall result by target, block number, and calldata hash.
fn get_l1_staticcall_value(target: Address, block_number: u64, calldata: &[u8]) -> Option<Vec<u8>> {
    let calldata_hash = keccak256(calldata);
    L1_STATICCALL_CACHE
        .lock()
        .expect("L1_STATICCALL_CACHE mutex poisoned")
        .get(&(target, block_number, calldata_hash))
        .cloned()
}

/// L1STATICCALL precompile: execute a static call against an L1 contract.
///
/// Input layout:
///   [0..20)   target address (20 bytes)
///   [20..52)  block number   (32 bytes, big-endian U256)
///   [52..)    calldata       (variable length, may be empty)
///
/// Output: variable-length return data (capped at 24 KB).
pub fn l1staticcall_run(input: &[u8], gas_limit: u64) -> PrecompileResult {
    // Gas calculation: fixed + overhead + per-byte for calldata beyond header
    let extra_calldata_len = input.len().saturating_sub(MIN_INPUT_LENGTH);
    let gas_used = L1STATICCALL_FIXED_GAS
        + L1STATICCALL_PER_CALL_OVERHEAD
        + L1STATICCALL_PER_BYTE_CALLDATA_GAS * (extra_calldata_len as u64);
    if gas_used > gas_limit {
        return Err(PrecompileError::OutOfGas);
    }

    // Input validation
    if input.len() < MIN_INPUT_LENGTH {
        return Err(PrecompileError::Other("Invalid input length".into()));
    }

    // Parse input fields
    let target = Address::from_slice(&input[0..20]);
    let block_number_bytes = &input[20..52];
    let calldata = &input[52..];

    // Convert block number from big-endian U256 to u64
    let block_number_u256 = U256::from_be_slice(block_number_bytes);
    let requested_block: u64 = block_number_u256
        .try_into()
        .map_err(|_| PrecompileError::Other("Block number too large".into()))?;

    // Block range validation using shared anchor/l1origin context
    let anchor_block_id = match get_anchor_block_id() {
        Some(id) => id,
        None => {
            warn!("L1STATICCALL: anchor block ID not set");
            return Err(PrecompileError::Other("Anchor block ID not set".into()));
        }
    };
    let l1_origin_block_id = match get_l1_origin_block_id() {
        Some(id) => id,
        None => {
            warn!("L1STATICCALL: L1 origin block ID not set");
            return Err(PrecompileError::Other("L1 origin block ID not set".into()));
        }
    };

    if l1_origin_block_id < anchor_block_id {
        return Err(PrecompileError::Other(
            "Invalid L1STATICCALL context: l1origin < anchor".into(),
        ));
    }

    if requested_block > l1_origin_block_id {
        warn!(
            "L1STATICCALL: rejected block {} > l1origin {} (anchor={})",
            requested_block, l1_origin_block_id, anchor_block_id
        );
        return Err(PrecompileError::Other(
            "Requested block number is after the L1 origin block".into(),
        ));
    }

    if l1_origin_block_id - requested_block > L1STATICCALL_MAX_BLOCK_LOOKBACK {
        warn!(
            "L1STATICCALL: rejected block {} too old (l1origin={}, lookback={})",
            requested_block, l1_origin_block_id, L1STATICCALL_MAX_BLOCK_LOOKBACK
        );
        return Err(PrecompileError::Other(
            "Requested block number exceeds max lookback from L1 origin".into(),
        ));
    }

    // Cache lookup
    let result =
        if let Some(cached) = get_l1_staticcall_value(target, requested_block, calldata) {
            trace!(
                "L1STATICCALL: cache hit target={:?} block={} calldata_len={}",
                target,
                requested_block,
                calldata.len()
            );
            cached
        } else {
            // RPC fallback
            let fetcher_guard =
                L1_STATICCALL_RPC_FETCHER.lock().expect("L1_STATICCALL_RPC_FETCHER mutex poisoned");
            if let Some(ref fetcher) = *fetcher_guard {
                debug!(
                    "L1STATICCALL: RPC fallback target={:?} block={} calldata_len={}",
                    target,
                    requested_block,
                    calldata.len()
                );
                let fetched = fetcher(target, requested_block, calldata).map_err(|e| {
                    PrecompileError::Other(format!("L1 RPC error: {e}").into())
                })?;
                drop(fetcher_guard);

                // Enforce max return data size
                if fetched.len() > MAX_RETURN_DATA_SIZE {
                    return Err(PrecompileError::Other(
                        format!(
                            "L1STATICCALL return data too large: {} > {} bytes",
                            fetched.len(),
                            MAX_RETURN_DATA_SIZE
                        )
                        .into(),
                    ));
                }

                // Cache the result
                set_l1_staticcall_value(target, requested_block, calldata, fetched.clone());

                // Track the served call
                L1_STATICCALL_RPC_SERVED_CALLS
                    .lock()
                    .expect("L1_STATICCALL_RPC_SERVED_CALLS mutex poisoned")
                    .push(L1StaticCallRecord {
                        target,
                        block_number: requested_block,
                        calldata: calldata.to_vec(),
                        return_data: fetched.clone(),
                    });

                fetched
            } else {
                warn!(
                    "L1STATICCALL: cache miss + no RPC — target={:?} block={} calldata_len={}",
                    target,
                    requested_block,
                    calldata.len()
                );
                return Err(PrecompileError::Other(
                    "L1STATICCALL result not found in cache".into(),
                ));
            }
        };

    // Enforce max return data size (also for cached values)
    if result.len() > MAX_RETURN_DATA_SIZE {
        return Err(PrecompileError::Other(
            format!(
                "L1STATICCALL return data too large: {} > {} bytes",
                result.len(),
                MAX_RETURN_DATA_SIZE
            )
            .into(),
        ));
    }

    Ok(PrecompileOutput::new(gas_used, Bytes::from(result)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precompiles::l1sload::{
        clear_l1_storage, set_anchor_block_id, set_l1_origin_block_id,
    };
    use serial_test::serial;

    const TEST_ADDRESS: [u8; 20] = [1u8; 20];

    /// Helper: build input bytes with the given block number and calldata.
    fn create_test_input(block_number: u64, calldata: &[u8]) -> Vec<u8> {
        let mut input = Vec::with_capacity(52 + calldata.len());
        input.extend_from_slice(&TEST_ADDRESS);
        let bn_u256 = U256::from(block_number);
        input.extend_from_slice(&bn_u256.to_be_bytes::<32>());
        input.extend_from_slice(calldata);
        input
    }

    /// Helper: minimal 52-byte input (empty calldata).
    fn create_min_input(block_number: u64) -> Vec<u8> {
        create_test_input(block_number, &[])
    }

    /// Gas for a call with `extra_calldata_bytes` bytes of calldata beyond the 52-byte header.
    fn expected_gas(extra_calldata_bytes: usize) -> u64 {
        L1STATICCALL_FIXED_GAS
            + L1STATICCALL_PER_CALL_OVERHEAD
            + L1STATICCALL_PER_BYTE_CALLDATA_GAS * (extra_calldata_bytes as u64)
    }

    /// Reset all L1STATICCALL-specific state AND the shared anchor/l1origin context.
    fn reset_all() {
        clear_l1_storage(); // clears anchor, l1origin, l1sload cache, l1sload rpc state
        clear_l1_staticcall_cache();
        clear_l1_staticcall_rpc_fetcher();
        clear_l1_staticcall_rpc_served_calls();
    }

    // ── Input validation ──────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_rejects_short_input() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        // 51 bytes — one byte short of minimum
        let short = vec![0u8; MIN_INPUT_LENGTH - 1];
        let result = l1staticcall_run(&short, expected_gas(0));
        assert!(result.is_err(), "Should reject input shorter than 52 bytes");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_accepts_exact_min_input() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        let input = create_min_input(100);
        assert_eq!(input.len(), 52, "Minimum input should be exactly 52 bytes");

        // Populate cache so it doesn't fail on cache miss
        let target = Address::from(TEST_ADDRESS);
        set_l1_staticcall_value(target, 100, &[], vec![0xAA]);

        let result = l1staticcall_run(&input, expected_gas(0));
        assert!(result.is_ok(), "52-byte input should be accepted: {:?}", result.err());
    }

    #[test]
    #[serial]
    fn test_l1staticcall_accepts_variable_length_input() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);
        let target = Address::from(TEST_ADDRESS);

        for extra_len in [1, 4, 32, 100, 256] {
            let calldata = vec![0xBBu8; extra_len];
            set_l1_staticcall_value(target, 100, &calldata, vec![0xCC]);

            let input = create_test_input(100, &calldata);
            assert_eq!(input.len(), 52 + extra_len);

            let result = l1staticcall_run(&input, expected_gas(extra_len));
            assert!(
                result.is_ok(),
                "Input with {} extra calldata bytes should be accepted: {:?}",
                extra_len,
                result.err()
            );
        }
    }

    // ── Context validation ────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_fails_without_anchor() {
        reset_all();
        // Do NOT set anchor block ID

        let input = create_min_input(100);
        let result = l1staticcall_run(&input, expected_gas(0));
        assert!(result.is_err(), "Should fail without anchor block ID");
        let msg = format!("{:?}", result.unwrap_err());
        assert!(msg.contains("Anchor block ID not set"), "Got: {msg}");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_fails_without_l1_origin() {
        reset_all();
        set_anchor_block_id(100);
        // Do NOT set l1_origin

        let input = create_min_input(100);
        let result = l1staticcall_run(&input, expected_gas(0));
        assert!(result.is_err(), "Should fail without L1 origin block ID");
        let msg = format!("{:?}", result.unwrap_err());
        assert!(msg.contains("L1 origin block ID not set"), "Got: {msg}");
    }

    // ── Cache miss without RPC ────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_fails_without_cached_value() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        let input = create_min_input(100);
        let result = l1staticcall_run(&input, expected_gas(0));
        assert!(result.is_err(), "Should fail when result is not cached and no RPC set");
    }

    // ── Cache hit ─────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_succeeds_with_cached_value() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        let target = Address::from(TEST_ADDRESS);
        let calldata = vec![0x01, 0x02, 0x03, 0x04];
        let return_data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        set_l1_staticcall_value(target, 100, &calldata, return_data.clone());

        let input = create_test_input(100, &calldata);
        let result = l1staticcall_run(&input, expected_gas(calldata.len()));
        assert!(result.is_ok(), "Should succeed with cached value: {:?}", result.err());

        let output = result.unwrap();
        assert_eq!(output.bytes.as_ref(), &return_data);
        assert_eq!(output.gas_used, expected_gas(calldata.len()));
    }

    // ── Gas calculation ───────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_gas_calculation_varies_by_calldata_length() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);
        let target = Address::from(TEST_ADDRESS);

        // 0 extra bytes
        let gas_0 = expected_gas(0);
        assert_eq!(gas_0, 2000 + 10000);

        // 10 extra bytes
        let gas_10 = expected_gas(10);
        assert_eq!(gas_10, 2000 + 10000 + 16 * 10);

        // 100 extra bytes
        let gas_100 = expected_gas(100);
        assert_eq!(gas_100, 2000 + 10000 + 16 * 100);

        // Verify precompile reports correct gas on success
        let calldata = vec![0xAA; 100];
        set_l1_staticcall_value(target, 100, &calldata, vec![0x01]);
        let input = create_test_input(100, &calldata);
        let output = l1staticcall_run(&input, gas_100).unwrap();
        assert_eq!(output.gas_used, gas_100);
    }

    #[test]
    #[serial]
    fn test_l1staticcall_fails_with_insufficient_gas() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        let input = create_min_input(100);
        let min_gas = expected_gas(0);

        // One gas below required
        let result = l1staticcall_run(&input, min_gas - 1);
        assert!(result.is_err(), "Should fail with insufficient gas");

        // Zero gas
        let result = l1staticcall_run(&input, 0);
        assert!(result.is_err(), "Should fail with zero gas");
    }

    // ── Block range checks ────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_rejects_block_after_l1_origin() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(110);

        // Request block 111 which is after l1origin 110
        let input = create_min_input(111);
        let result = l1staticcall_run(&input, expected_gas(0));
        assert!(result.is_err(), "Should reject block after l1origin");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_accepts_block_between_anchor_and_l1origin() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(120);

        let target = Address::from(TEST_ADDRESS);
        let calldata = vec![0x11];
        let return_data = vec![0x22, 0x33];
        set_l1_staticcall_value(target, 115, &calldata, return_data.clone());

        let input = create_test_input(115, &calldata);
        let result = l1staticcall_run(&input, expected_gas(calldata.len()));
        assert!(result.is_ok(), "Should accept block between anchor and l1origin");
        assert_eq!(result.unwrap().bytes.as_ref(), &return_data);
    }

    #[test]
    #[serial]
    fn test_l1staticcall_rejects_block_beyond_lookback() {
        reset_all();
        set_anchor_block_id(700);
        set_l1_origin_block_id(1000);

        // Block 1000 - 257 = 743. Distance from l1origin: 257 > 256
        let input = create_min_input(1000 - L1STATICCALL_MAX_BLOCK_LOOKBACK - 1);
        let result = l1staticcall_run(&input, expected_gas(0));
        assert!(result.is_err(), "Should reject block beyond max lookback from l1origin");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_exact_lookback_boundary() {
        reset_all();
        set_anchor_block_id(700);
        set_l1_origin_block_id(1000);

        let block = 1000 - L1STATICCALL_MAX_BLOCK_LOOKBACK; // 744
        let target = Address::from(TEST_ADDRESS);
        set_l1_staticcall_value(target, block, &[], vec![0xFF]);

        let input = create_min_input(block);
        let result = l1staticcall_run(&input, expected_gas(0));
        assert!(result.is_ok(), "Block at exact lookback boundary should succeed");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_exact_l1_origin() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(200);

        let target = Address::from(TEST_ADDRESS);
        let return_data = vec![0x42];
        set_l1_staticcall_value(target, 200, &[], return_data.clone());

        let input = create_min_input(200);
        let result = l1staticcall_run(&input, expected_gas(0));
        assert!(result.is_ok(), "Block at exact l1origin should succeed");
        assert_eq!(result.unwrap().bytes.as_ref(), &return_data);
    }

    // ── Cache key uses calldata hash ──────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_cache_key_includes_calldata_hash() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        let target = Address::from(TEST_ADDRESS);
        let calldata_a = vec![0xAA; 4];
        let calldata_b = vec![0xBB; 4];
        let return_a = vec![0x11];
        let return_b = vec![0x22];

        set_l1_staticcall_value(target, 100, &calldata_a, return_a.clone());
        set_l1_staticcall_value(target, 100, &calldata_b, return_b.clone());

        // Verify each calldata retrieves its own result
        let input_a = create_test_input(100, &calldata_a);
        let result_a = l1staticcall_run(&input_a, expected_gas(4)).unwrap();
        assert_eq!(result_a.bytes.as_ref(), &return_a);

        let input_b = create_test_input(100, &calldata_b);
        let result_b = l1staticcall_run(&input_b, expected_gas(4)).unwrap();
        assert_eq!(result_b.bytes.as_ref(), &return_b);
    }

    // ── Variable-length return data ───────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_variable_length_return_data() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);
        let target = Address::from(TEST_ADDRESS);

        // Empty return data
        set_l1_staticcall_value(target, 100, &[0x01], vec![]);
        let input = create_test_input(100, &[0x01]);
        let output = l1staticcall_run(&input, expected_gas(1)).unwrap();
        assert!(output.bytes.is_empty(), "Empty return data should be allowed");

        // Single byte
        set_l1_staticcall_value(target, 100, &[0x02], vec![0xFF]);
        let input = create_test_input(100, &[0x02]);
        let output = l1staticcall_run(&input, expected_gas(1)).unwrap();
        assert_eq!(output.bytes.as_ref(), &[0xFF]);

        // 1024 bytes
        let big_return = vec![0xAB; 1024];
        set_l1_staticcall_value(target, 100, &[0x03], big_return.clone());
        let input = create_test_input(100, &[0x03]);
        let output = l1staticcall_run(&input, expected_gas(1)).unwrap();
        assert_eq!(output.bytes.len(), 1024);
        assert_eq!(output.bytes.as_ref(), &big_return);
    }

    // ── RPC fallback ──────────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_rpc_fallback_records_served_calls() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        let target = Address::from(TEST_ADDRESS);
        let calldata = vec![0xCA, 0xFE];
        let return_data = vec![0xDE, 0xAD];

        let expected_target = target;
        let expected_calldata = calldata.clone();
        let expected_return = return_data.clone();
        set_l1_staticcall_rpc_fetcher(move |t, bn, cd| {
            assert_eq!(t, expected_target);
            assert_eq!(bn, 100);
            assert_eq!(cd, expected_calldata.as_slice());
            Ok(expected_return.clone())
        });

        let input = create_test_input(100, &calldata);
        let result = l1staticcall_run(&input, expected_gas(calldata.len()));
        assert!(result.is_ok(), "RPC fallback should succeed: {:?}", result.err());
        assert_eq!(result.unwrap().bytes.as_ref(), &return_data);

        let served = take_l1_staticcall_rpc_served_calls();
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].target, target);
        assert_eq!(served[0].block_number, 100);
        assert_eq!(served[0].calldata, calldata);
        assert_eq!(served[0].return_data, return_data);

        // Second call should be served from cache even without fetcher
        clear_l1_staticcall_rpc_fetcher();
        let input2 = create_test_input(100, &calldata);
        let result2 = l1staticcall_run(&input2, expected_gas(calldata.len()));
        assert!(result2.is_ok(), "Cache hit should succeed after fetcher cleared");
        assert_eq!(result2.unwrap().bytes.as_ref(), &return_data);
    }

    #[test]
    #[serial]
    fn test_l1staticcall_rpc_fallback_error_propagates() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        set_l1_staticcall_rpc_fetcher(|_, _, _| Err("L1 node unavailable".to_string()));

        let input = create_min_input(100);
        let result = l1staticcall_run(&input, expected_gas(0));
        assert!(result.is_err(), "RPC error should propagate");
        let msg = format!("{:?}", result.unwrap_err());
        assert!(msg.contains("L1 node unavailable"), "Got: {msg}");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_multiple_rpc_calls_tracked() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(110);

        set_l1_staticcall_rpc_fetcher(|_, bn, cd| {
            let mut ret = vec![0u8; 4];
            ret[0] = bn as u8;
            ret[1] = cd.first().copied().unwrap_or(0);
            Ok(ret)
        });

        // Two calls with different blocks
        let input1 = create_test_input(105, &[0x11]);
        let _ = l1staticcall_run(&input1, expected_gas(1)).unwrap();

        let input2 = create_test_input(110, &[0x22]);
        let _ = l1staticcall_run(&input2, expected_gas(1)).unwrap();

        let served = take_l1_staticcall_rpc_served_calls();
        assert_eq!(served.len(), 2, "Should track both RPC-served calls");
        assert_eq!(served[0].block_number, 105);
        assert_eq!(served[0].calldata, vec![0x11]);
        assert_eq!(served[1].block_number, 110);
        assert_eq!(served[1].calldata, vec![0x22]);
    }

    // ── Clear operations ──────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_clear_cache_operations() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        let target = Address::from(TEST_ADDRESS);
        set_l1_staticcall_value(target, 100, &[0x01], vec![0xFF]);

        // Verify cached value is accessible
        let input = create_test_input(100, &[0x01]);
        let result = l1staticcall_run(&input, expected_gas(1));
        assert!(result.is_ok(), "Should find cached value");

        // Clear cache
        clear_l1_staticcall_cache();

        // Now it should fail (no cache, no RPC)
        let result = l1staticcall_run(&input, expected_gas(1));
        assert!(result.is_err(), "Should fail after cache is cleared");

        // Verify anchor/l1origin context is NOT cleared by clear_l1_staticcall_cache
        assert!(get_anchor_block_id().is_some(), "Anchor should survive cache clear");
        assert!(get_l1_origin_block_id().is_some(), "L1 origin should survive cache clear");

        // Test served calls clear
        set_l1_staticcall_rpc_fetcher(|_, _, _| Ok(vec![0x99]));
        let _ = l1staticcall_run(&input, expected_gas(1)).unwrap();
        assert_eq!(take_l1_staticcall_rpc_served_calls().len(), 1);

        // After take, should be empty
        assert!(take_l1_staticcall_rpc_served_calls().is_empty());

        // Explicit clear
        set_l1_staticcall_rpc_fetcher(|_, _, _| Ok(vec![0x88]));
        clear_l1_staticcall_cache(); // force cache miss
        let _ = l1staticcall_run(&input, expected_gas(1)).unwrap();
        clear_l1_staticcall_rpc_served_calls();
        assert!(take_l1_staticcall_rpc_served_calls().is_empty(), "Explicit clear should empty served calls");
    }

    // ── Edge cases ────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_anchor_equals_l1origin() {
        reset_all();
        let block = 100u64;
        set_anchor_block_id(block);
        set_l1_origin_block_id(block);

        let target = Address::from(TEST_ADDRESS);
        set_l1_staticcall_value(target, block, &[], vec![0x01]);

        let input = create_min_input(block);
        let result = l1staticcall_run(&input, expected_gas(0));
        assert!(result.is_ok(), "anchor == l1origin == requested should succeed");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_l1origin_less_than_anchor_rejected() {
        reset_all();
        set_anchor_block_id(200);
        set_l1_origin_block_id(100); // l1origin < anchor — invalid

        let input = create_min_input(100);
        let result = l1staticcall_run(&input, expected_gas(0));
        assert!(result.is_err(), "l1origin < anchor should be rejected");
        let msg = format!("{:?}", result.unwrap_err());
        assert!(msg.contains("l1origin < anchor"), "Got: {msg}");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_max_return_data_size() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        let target = Address::from(TEST_ADDRESS);

        // Exactly at max — should succeed
        let exact_max = vec![0xAA; MAX_RETURN_DATA_SIZE];
        set_l1_staticcall_value(target, 100, &[0x01], exact_max.clone());
        let input = create_test_input(100, &[0x01]);
        let result = l1staticcall_run(&input, expected_gas(1));
        assert!(result.is_ok(), "Exactly MAX_RETURN_DATA_SIZE should succeed");
        assert_eq!(result.unwrap().bytes.len(), MAX_RETURN_DATA_SIZE);

        // One byte over max via RPC — should fail
        clear_l1_staticcall_cache();
        let over_max = vec![0xBB; MAX_RETURN_DATA_SIZE + 1];
        let over_clone = over_max.clone();
        set_l1_staticcall_rpc_fetcher(move |_, _, _| Ok(over_clone.clone()));
        let result = l1staticcall_run(&input, expected_gas(1));
        assert!(result.is_err(), "Over MAX_RETURN_DATA_SIZE should fail");
        let msg = format!("{:?}", result.unwrap_err());
        assert!(msg.contains("return data too large"), "Got: {msg}");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_zero_block_number() {
        reset_all();
        set_anchor_block_id(0);
        set_l1_origin_block_id(100);

        let target = Address::from(TEST_ADDRESS);
        set_l1_staticcall_value(target, 0, &[], vec![0x00]);

        let input = create_min_input(0);
        let result = l1staticcall_run(&input, expected_gas(0));
        assert!(result.is_ok(), "Block 0 within lookback should succeed");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_same_target_different_calldata() {
        reset_all();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        let target = Address::from(TEST_ADDRESS);
        let calldata1 = vec![0x01, 0x02];
        let calldata2 = vec![0x03, 0x04];
        let return1 = vec![0xAA, 0xBB];
        let return2 = vec![0xCC, 0xDD];

        set_l1_staticcall_value(target, 100, &calldata1, return1.clone());
        set_l1_staticcall_value(target, 100, &calldata2, return2.clone());

        let input1 = create_test_input(100, &calldata1);
        let result1 = l1staticcall_run(&input1, expected_gas(2)).unwrap();
        assert_eq!(result1.bytes.as_ref(), &return1);

        let input2 = create_test_input(100, &calldata2);
        let result2 = l1staticcall_run(&input2, expected_gas(2)).unwrap();
        assert_eq!(result2.bytes.as_ref(), &return2);

        assert_ne!(
            result1.bytes, result2.bytes,
            "Same target with different calldata should return different values"
        );
    }
}
