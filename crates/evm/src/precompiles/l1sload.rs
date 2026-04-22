//! L1SLOAD precompile (RIP-7728, address `0x10001`).
//!
//! **Global-state invariant.** The storage cache, fetcher, anchor / l1-origin context, and
//! RPC-served-calls set are process-global `LazyLock<Mutex<_>>`. They are safe only when the
//! host drives the precompile single-threaded with one `(anchor, l1origin)` context at a time —
//! the normal preflight and ZK-guest pattern. Running two different contexts concurrently will
//! silently cross-contaminate cache hits.
//!
//! **Cache lifecycle.** No automatic eviction. The caller (surge-raiko) must invoke
//! [`clear_l1_storage`] (or the sibling [`crate::precompiles::l1staticcall::clear_l1_staticcall_cache`])
//! at the top of every new block / batch iteration. Tests use `#[serial]` to serialize global-state
//! mutations.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex},
};

use alloy_primitives::{Address, B256, Bytes, U256};
use reth_revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};
use tracing::{debug, trace, warn};

/// Fixed gas cost for an L1SLOAD precompile call.
const L1SLOAD_FIXED_GAS: u64 = 2000;
/// Per-load gas cost for each storage slot read.
const L1SLOAD_PER_LOAD_GAS: u64 = 2000;

/// Expected input length: 20 bytes (address) + 32 bytes (storage key) + 32 bytes (block number),
/// total 84 bytes
const EXPECTED_INPUT_LENGTH: usize = 84;

/// Maximum number of L1 blocks to look back from L1 origin
const L1SLOAD_MAX_BLOCK_LOOKBACK: u64 = 256;

/// Type alias for the L1 storage cache map.
type L1StorageCache = HashMap<(Address, B256, B256), B256>;

/// In-memory cache for L1 storage values
/// Key: (contract_address, storage_key, block_number) -> Value: storage_value
static L1_STORAGE_CACHE: LazyLock<Mutex<L1StorageCache>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Current anchor block ID context for L1SLOAD operations (stored as u64 for range checking)
static CURRENT_ANCHOR_BLOCK_ID: LazyLock<Mutex<Option<u64>>> = LazyLock::new(|| Mutex::new(None));
/// Current L1 origin block ID context for L1SLOAD operations (upper bound for forward reads).
static CURRENT_L1_ORIGIN_BLOCK_ID: LazyLock<Mutex<Option<u64>>> =
    LazyLock::new(|| Mutex::new(None));

/// Callback function type for fetching L1 storage values via RPC.
/// Set by surge-raiko during preflight (has network access), None during ZK proving.
/// Arguments: (contract_address, storage_key, block_number_u64) -> storage_value
///
/// Wrapped in `Arc` so the caller can clone the pointer out of the global mutex before
/// invoking the (slow) RPC. Keeps concurrent `set_*`/`clear_*` callers unblocked on lock
/// wait time and avoids poisoning the mutex if the fetcher panics.
type L1StorageFetcher = Arc<dyn Fn(Address, B256, u64) -> Result<B256, String> + Send + Sync>;

/// Live L1 RPC fetcher for handling cache misses on indirect L1SLOAD calls.
static L1_RPC_FETCHER: LazyLock<Mutex<Option<L1StorageFetcher>>> =
    LazyLock::new(|| Mutex::new(None));

/// Tracks L1SLOAD calls served via the live RPC fetcher (not from pre-fetched cache).
/// surge-raiko reads this after execution to fetch Merkle proofs for ZK prover.
/// Key: (contract_address, storage_key, block_number as B256)
static L1_RPC_SERVED_CALLS: LazyLock<Mutex<HashSet<(Address, B256, B256)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Set the current anchor block ID context
pub fn set_anchor_block_id(anchor_block_id: u64) {
    let mut ctx = CURRENT_ANCHOR_BLOCK_ID.lock().expect("CURRENT_ANCHOR_BLOCK_ID mutex poisoned");
    *ctx = Some(anchor_block_id);
}

/// Reads the current anchor block ID from global context.
pub(crate) fn get_anchor_block_id() -> Option<u64> {
    *CURRENT_ANCHOR_BLOCK_ID.lock().expect("CURRENT_ANCHOR_BLOCK_ID mutex poisoned")
}

/// Set the L1 origin block ID context.
pub fn set_l1_origin_block_id(l1_origin_block_id: u64) {
    let mut ctx =
        CURRENT_L1_ORIGIN_BLOCK_ID.lock().expect("CURRENT_L1_ORIGIN_BLOCK_ID mutex poisoned");
    *ctx = Some(l1_origin_block_id);
}

/// Reads the current L1 origin block ID from global context.
pub(crate) fn get_l1_origin_block_id() -> Option<u64> {
    *CURRENT_L1_ORIGIN_BLOCK_ID.lock().expect("CURRENT_L1_ORIGIN_BLOCK_ID mutex poisoned")
}

/// Insert a value into the L1 storage cache.
pub fn set_l1_storage_value(
    contract_address: Address,
    storage_key: B256,
    block_number: B256,
    value: B256,
) {
    let mut cache = L1_STORAGE_CACHE.lock().expect("L1_STORAGE_CACHE mutex poisoned");
    cache.insert((contract_address, storage_key, block_number), value);
}

/// Clear all L1SLOAD state (cache, context, RPC fetcher, tracked calls).
pub fn clear_l1_storage() {
    L1_STORAGE_CACHE.lock().expect("L1_STORAGE_CACHE mutex poisoned").clear();
    *CURRENT_ANCHOR_BLOCK_ID.lock().expect("CURRENT_ANCHOR_BLOCK_ID mutex poisoned") = None;
    *CURRENT_L1_ORIGIN_BLOCK_ID.lock().expect("CURRENT_L1_ORIGIN_BLOCK_ID mutex poisoned") = None;
    clear_l1_rpc_fetcher();
    clear_l1_rpc_served_calls();
}

/// Set the L1 RPC fetcher callback for live storage value fetching on cache miss.
/// Called by surge-raiko during preflight when network access is available.
pub fn set_l1_rpc_fetcher(
    fetcher: impl Fn(Address, B256, u64) -> Result<B256, String> + Send + Sync + 'static,
) {
    *L1_RPC_FETCHER.lock().expect("L1_RPC_FETCHER mutex poisoned") = Some(Arc::new(fetcher));
}

/// Clear the L1 RPC fetcher (disables live RPC fallback).
pub fn clear_l1_rpc_fetcher() {
    *L1_RPC_FETCHER.lock().expect("L1_RPC_FETCHER mutex poisoned") = None;
}

/// Get all L1SLOAD calls that were served via live RPC (not from cache).
/// Used by surge-raiko after execution to fetch Merkle proofs for these calls.
pub fn take_l1_rpc_served_calls() -> HashSet<(Address, B256, B256)> {
    std::mem::take(&mut *L1_RPC_SERVED_CALLS.lock().expect("L1_RPC_SERVED_CALLS mutex poisoned"))
}

/// Clear tracked calls served via live L1 RPC.
pub fn clear_l1_rpc_served_calls() {
    L1_RPC_SERVED_CALLS.lock().expect("L1_RPC_SERVED_CALLS mutex poisoned").clear();
}

/// Looks up a cached L1 storage value by address, key, and block number.
fn get_l1_storage_value(
    contract_address: Address,
    storage_key: B256,
    block_number: B256,
) -> Option<B256> {
    L1_STORAGE_CACHE
        .lock()
        .expect("L1_STORAGE_CACHE mutex poisoned")
        .get(&(contract_address, storage_key, block_number))
        .copied()
}

/// L1SLOAD precompile (RIP-7728): read L1 storage values.
/// Input: 20B address + 32B key + 32B block number. Output: 32B value.
pub fn l1sload_run(input: &[u8], gas_limit: u64) -> PrecompileResult {
    let gas_used = L1SLOAD_FIXED_GAS + L1SLOAD_PER_LOAD_GAS;
    if gas_used > gas_limit {
        return Err(PrecompileError::OutOfGas);
    }

    if input.len() != EXPECTED_INPUT_LENGTH {
        return Err(PrecompileError::Other("Invalid input length".into()));
    }

    let contract_address = Address::from_slice(&input[0..20]);
    let storage_key = B256::from_slice(&input[20..52]);
    let block_number = B256::from_slice(&input[52..84]);

    // Block-range validation uses only `l1_origin_block_id` for the `[l1origin − 256, l1origin]`
    // window. `_anchor_block_id` participates only in the `l1origin < anchor` invariant check
    // — the underscore prefix signals that anchor is *not* a bound of the lookback window.
    let _anchor_block_id = match get_anchor_block_id() {
        Some(id) => id,
        None => {
            warn!("L1SLOAD: anchor block ID not set");
            return Err(PrecompileError::Other("Anchor block ID not set".into()));
        }
    };
    let l1_origin_block_id = match get_l1_origin_block_id() {
        Some(id) => id,
        None => {
            warn!("L1SLOAD: L1 origin block ID not set");
            return Err(PrecompileError::Other("L1 origin block ID not set".into()));
        }
    };

    if l1_origin_block_id < _anchor_block_id {
        return Err(PrecompileError::Other("Invalid L1SLOAD context: l1origin < anchor".into()));
    }

    let block_number_u256 = U256::from_be_bytes(block_number.0);
    let requested_block: u64 = block_number_u256
        .try_into()
        .map_err(|_| PrecompileError::Other("Block number too large".into()))?;

    if requested_block > l1_origin_block_id {
        debug!(
            "L1SLOAD: rejected block {} > l1origin {} (anchor={})",
            requested_block, l1_origin_block_id, _anchor_block_id
        );
        return Err(PrecompileError::Other(
            "Requested block number is after the L1 origin block".into(),
        ));
    }

    if l1_origin_block_id - requested_block > L1SLOAD_MAX_BLOCK_LOOKBACK {
        debug!(
            "L1SLOAD: rejected block {} too old (l1origin={}, lookback={})",
            requested_block, l1_origin_block_id, L1SLOAD_MAX_BLOCK_LOOKBACK
        );
        return Err(PrecompileError::Other(
            "Requested block number exceeds max lookback from L1 origin".into(),
        ));
    }

    let value = if let Some(cached) =
        get_l1_storage_value(contract_address, storage_key, block_number)
    {
        trace!(
            "L1SLOAD: cache hit contract={:?} key={:?} block={}",
            contract_address, storage_key, requested_block
        );
        cached
    } else {
        // Clone the Arc out of the lock so we can drop the guard before the (slow) RPC runs.
        let fetcher =
            L1_RPC_FETCHER.lock().expect("L1_RPC_FETCHER mutex poisoned").as_ref().map(Arc::clone);
        if let Some(fetcher) = fetcher {
            debug!(
                "L1SLOAD: RPC fallback contract={:?} key={:?} block={}",
                contract_address, storage_key, requested_block
            );
            let fetched = fetcher(contract_address, storage_key, requested_block)
                .map_err(|e| PrecompileError::Other(format!("L1 RPC error: {e}").into()))?;

            set_l1_storage_value(contract_address, storage_key, block_number, fetched);

            L1_RPC_SERVED_CALLS.lock().expect("L1_RPC_SERVED_CALLS mutex poisoned").insert((
                contract_address,
                storage_key,
                block_number,
            ));

            fetched
        } else {
            warn!(
                "L1SLOAD: cache miss + no RPC — contract={:?} key={:?} block={:?}",
                contract_address, storage_key, block_number
            );
            return Err(PrecompileError::Other("L1 storage value not found in cache".into()));
        }
    };

    let mut output = [0u8; 32];
    output.copy_from_slice(value.as_slice());
    Ok(PrecompileOutput::new(gas_used, Bytes::from(output)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    const TEST_ADDRESS: [u8; 20] = [1u8; 20];
    const TEST_STORAGE_KEY: [u8; 32] = [2u8; 32];
    const TEST_STORAGE_VALUE: [u8; 32] = [5u8; 32];
    const SUFFICIENT_GAS: u64 = L1SLOAD_FIXED_GAS + L1SLOAD_PER_LOAD_GAS;
    const INSUFFICIENT_GAS: u64 = SUFFICIENT_GAS - 1;

    /// Helper: create a B256 block number from a u64
    fn block_number_b256(n: u64) -> B256 {
        B256::from(U256::from(n))
    }

    /// Helper function to create test input (84 bytes) with given block number
    fn create_test_input(block_number: u64) -> Vec<u8> {
        let mut input = vec![0u8; EXPECTED_INPUT_LENGTH];
        input[0..20].copy_from_slice(&TEST_ADDRESS);
        input[20..52].copy_from_slice(&TEST_STORAGE_KEY);
        let bn_b256 = block_number_b256(block_number);
        input[52..84].copy_from_slice(bn_b256.as_slice());
        input
    }

    /// Helper function to setup storage cache with test data
    fn setup_test_storage(anchor: u64, l1_origin: u64, block: u64) -> (Address, B256, B256, B256) {
        let address = Address::from(TEST_ADDRESS);
        let key = B256::from(TEST_STORAGE_KEY);
        let block_num = block_number_b256(block);
        let value = B256::from(TEST_STORAGE_VALUE);

        set_anchor_block_id(anchor);
        set_l1_origin_block_id(l1_origin);
        set_l1_storage_value(address, key, block_num, value);
        (address, key, block_num, value)
    }

    #[test]
    #[serial]
    fn test_l1sload_rejects_invalid_input_lengths() {
        clear_l1_storage();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        // Test input too short (52 bytes - old format)
        let short_input = Bytes::from(vec![0u8; 52]);
        let result = l1sload_run(&short_input, SUFFICIENT_GAS);
        assert!(result.is_err(), "Should reject too short input");

        // Test input too long
        let long_input = Bytes::from(vec![0u8; 116]);
        let result = l1sload_run(&long_input, SUFFICIENT_GAS);
        assert!(result.is_err(), "Should reject too long input");
    }

    #[test]
    #[serial]
    fn test_l1sload_fails_without_anchor_block_id() {
        clear_l1_storage();

        let input = create_test_input(100);
        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);

        assert!(result.is_err(), "Should fail when anchor block ID is not set");
    }

    #[test]
    #[serial]
    fn test_l1sload_fails_without_cached_storage() {
        clear_l1_storage();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        let input = create_test_input(100);
        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);

        assert!(result.is_err(), "Should fail when storage value is not cached");
    }

    #[test]
    #[serial]
    fn test_l1sload_succeeds_with_cached_storage() {
        clear_l1_storage();

        let anchor = 100u64;
        let l1_origin = 100u64;
        let block = 100u64;
        let (_, _, _, expected_value) = setup_test_storage(anchor, l1_origin, block);
        let input = create_test_input(block);

        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(
            result.is_ok(),
            "Should succeed when storage value is cached. Error: {:?}",
            result.err()
        );

        let output = result.unwrap();

        // Verify gas usage
        let expected_gas = L1SLOAD_FIXED_GAS + L1SLOAD_PER_LOAD_GAS;
        assert_eq!(output.gas_used, expected_gas, "Gas usage should be fixed cost + per-load cost");

        // Verify output length
        assert_eq!(output.bytes.len(), 32, "Output should be exactly 32 bytes");

        // Verify output content matches stored value
        assert_eq!(
            output.bytes.as_ref(),
            &expected_value.0,
            "Output should match the cached storage value"
        );
    }

    #[test]
    #[serial]
    fn test_l1sload_fails_with_insufficient_gas() {
        clear_l1_storage();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        let input = Bytes::from(create_test_input(100));
        let result = l1sload_run(&input, INSUFFICIENT_GAS);

        assert!(result.is_err(), "Should fail when gas limit is insufficient");
    }

    #[test]
    #[serial]
    fn test_l1sload_rejects_block_after_l1_origin() {
        clear_l1_storage();
        set_anchor_block_id(100);
        set_l1_origin_block_id(110);

        // Request block 111 which is after l1origin block 110
        let input = create_test_input(111);
        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(result.is_err(), "Should reject block number after l1origin");
    }

    #[test]
    #[serial]
    fn test_l1sload_accepts_block_between_anchor_and_l1origin() {
        clear_l1_storage();

        let anchor = 100u64;
        let l1_origin = 120u64;
        let block = 115u64;
        let (_, _, _, expected_value) = setup_test_storage(anchor, l1_origin, block);
        let input = create_test_input(block);

        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(result.is_ok(), "Should accept forward block up to l1origin");
        assert_eq!(result.unwrap().bytes.as_ref(), &expected_value.0);
    }

    #[test]
    #[serial]
    fn test_l1sload_rejects_block_beyond_lookback() {
        clear_l1_storage();
        set_anchor_block_id(900);
        set_l1_origin_block_id(1000);

        // Request block 1000 - 257 = 743. Distance from l1origin: 257 > 256
        let input = create_test_input(1000 - L1SLOAD_MAX_BLOCK_LOOKBACK - 1);
        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(result.is_err(), "Should reject block beyond max lookback from l1origin");
    }

    #[test]
    #[serial]
    fn test_l1sload_succeeds_with_older_block() {
        clear_l1_storage();

        let anchor = 200u64;
        let l1_origin = 250u64;
        let block = 150u64; // 100 blocks before l1origin, within 256 lookback
        let (_, _, _, expected_value) = setup_test_storage(anchor, l1_origin, block);
        let input = create_test_input(block);

        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(result.is_ok(), "Should succeed for block within lookback range from l1origin");
        assert_eq!(result.unwrap().bytes.as_ref(), &expected_value.0);
    }

    #[test]
    #[serial]
    fn test_l1sload_rejects_block_near_anchor_but_far_from_l1origin() {
        clear_l1_storage();

        // anchor=100, l1origin=500. Block 100 is at anchor but 400 blocks from l1origin.
        set_anchor_block_id(100);
        set_l1_origin_block_id(500);
        let block_num = block_number_b256(100);
        set_l1_storage_value(
            Address::from(TEST_ADDRESS),
            B256::from(TEST_STORAGE_KEY),
            block_num,
            B256::from(TEST_STORAGE_VALUE),
        );

        let input = create_test_input(100);
        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(result.is_err(), "Block at anchor should be rejected when too far from l1origin");
    }

    #[test]
    #[serial]
    fn test_storage_cache_operations() {
        clear_l1_storage();

        let address = Address::from(TEST_ADDRESS);
        let key = B256::from(TEST_STORAGE_KEY);
        let block = block_number_b256(100);
        let value = B256::from(TEST_STORAGE_VALUE);

        // Verify cache is initially empty
        assert!(
            get_l1_storage_value(address, key, block).is_none(),
            "Cache should be empty initially"
        );

        set_l1_storage_value(address, key, block, value);
        assert_eq!(
            get_l1_storage_value(address, key, block),
            Some(value),
            "Should retrieve the cached value"
        );

        // Different block number should not have the value
        let other_block = block_number_b256(101);
        assert!(
            get_l1_storage_value(address, key, other_block).is_none(),
            "Different block number should not match"
        );
    }

    #[test]
    #[serial]
    fn test_anchor_block_id_context() {
        clear_l1_storage();

        // Verify context is initially empty
        assert!(get_anchor_block_id().is_none(), "Context should be empty initially");
        assert!(get_l1_origin_block_id().is_none(), "L1 origin context should be empty initially");

        set_anchor_block_id(42);
        set_l1_origin_block_id(50);
        assert_eq!(get_anchor_block_id(), Some(42), "Should retrieve the set anchor block ID");
        assert_eq!(
            get_l1_origin_block_id(),
            Some(50),
            "Should retrieve the set l1 origin block ID"
        );

        clear_l1_storage();
        assert!(get_anchor_block_id().is_none(), "Context should be cleared");
        assert!(get_l1_origin_block_id().is_none(), "L1 origin context should be cleared");
    }

    #[test]
    #[serial]
    fn test_cache_key_uniqueness() {
        clear_l1_storage();

        let address1 = Address::from([1u8; 20]);
        let address2 = Address::from([2u8; 20]);
        let key1 = B256::from([1u8; 32]);
        let key2 = B256::from([2u8; 32]);
        let block1 = block_number_b256(100);
        let block2 = block_number_b256(200);
        let value1 = B256::from([10u8; 32]);
        let value2 = B256::from([20u8; 32]);

        set_l1_storage_value(address1, key1, block1, value1);
        set_l1_storage_value(address2, key2, block2, value2);

        // Verify each key returns its correct value
        assert_eq!(get_l1_storage_value(address1, key1, block1), Some(value1));
        assert_eq!(get_l1_storage_value(address2, key2, block2), Some(value2));

        // Verify non-existent combinations return None
        assert!(get_l1_storage_value(address1, key2, block1).is_none());
        assert!(get_l1_storage_value(address2, key1, block2).is_none());
        assert!(get_l1_storage_value(address1, key1, block2).is_none());
    }

    #[test]
    #[serial]
    fn test_l1sload_rpc_fallback_records_served_calls() {
        clear_l1_storage();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        let address = Address::from(TEST_ADDRESS);
        let key = B256::from(TEST_STORAGE_KEY);
        let input = Bytes::from(create_test_input(100));
        let fallback_value = B256::from([9u8; 32]);

        set_l1_rpc_fetcher(move |requested_addr, requested_key, requested_block| {
            if requested_addr != address || requested_key != key || requested_block != 100 {
                return Err("unexpected fallback request".to_string());
            }
            Ok(fallback_value)
        });

        let first = l1sload_run(&input, SUFFICIENT_GAS).expect("RPC fallback should succeed");
        assert_eq!(first.bytes.as_ref(), fallback_value.as_slice());

        let served = take_l1_rpc_served_calls();
        assert_eq!(served.len(), 1, "Expected exactly one served call");
        assert!(served.contains(&(address, key, block_number_b256(100))));

        // Second call should be served from cache even if fallback is disabled.
        clear_l1_rpc_fetcher();
        let second = l1sload_run(&input, SUFFICIENT_GAS).expect("Cache hit should still succeed");
        assert_eq!(second.bytes.as_ref(), fallback_value.as_slice());
    }

    #[test]
    #[serial]
    fn test_clear_l1_storage_clears_rpc_fallback_state() {
        clear_l1_storage();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        let input = Bytes::from(create_test_input(100));
        let fallback_value = B256::from([11u8; 32]);
        set_l1_rpc_fetcher(move |_, _, _| Ok(fallback_value));

        let _ = l1sload_run(&input, SUFFICIENT_GAS).expect("Fallback should serve first request");
        assert_eq!(take_l1_rpc_served_calls().len(), 1, "Served call must be tracked");

        clear_l1_storage();

        // Served-call tracker should be cleared after storage reset.
        assert!(
            take_l1_rpc_served_calls().is_empty(),
            "Served calls must be empty after clear_l1_storage"
        );

        // Context should be cleared, so call must fail before any fetcher usage.
        let err = l1sload_run(&input, SUFFICIENT_GAS).expect_err("Context should be cleared");
        assert!(
            format!("{err:?}").contains("Anchor block ID not set"),
            "Expected missing-anchor context error, got {err:?}"
        );
    }

    #[test]
    #[serial]
    fn test_l1sload_exact_lookback_boundary() {
        clear_l1_storage();

        // Block exactly at lookback limit: l1origin - 256
        let anchor = 700u64;
        let l1_origin = 1000u64;
        let block = l1_origin - L1SLOAD_MAX_BLOCK_LOOKBACK; // 744
        let (_, _, _, expected_value) = setup_test_storage(anchor, l1_origin, block);
        let input = create_test_input(block);

        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(result.is_ok(), "Block at exact lookback boundary should succeed");
        assert_eq!(result.unwrap().bytes.as_ref(), &expected_value.0);
    }

    #[test]
    #[serial]
    fn test_l1sload_exact_l1_origin() {
        clear_l1_storage();

        // Request exactly at l1_origin — should succeed
        let anchor = 100u64;
        let l1_origin = 200u64;
        let block = 200u64;
        let (_, _, _, expected_value) = setup_test_storage(anchor, l1_origin, block);
        let input = create_test_input(block);

        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(result.is_ok(), "Block at exact l1origin should succeed");
        assert_eq!(result.unwrap().bytes.as_ref(), &expected_value.0);
    }

    #[test]
    #[serial]
    fn test_l1sload_exact_gas_boundary() {
        clear_l1_storage();
        setup_test_storage(100, 100, 100);
        let input = create_test_input(100);

        // Exactly enough gas
        let result = l1sload_run(&Bytes::from(input.clone()), SUFFICIENT_GAS);
        assert!(result.is_ok(), "Exact gas amount should succeed");

        // One less gas
        let result = l1sload_run(&Bytes::from(input.clone()), SUFFICIENT_GAS - 1);
        assert!(result.is_err(), "One gas below should fail");

        // Zero gas
        let result = l1sload_run(&Bytes::from(input), 0);
        assert!(result.is_err(), "Zero gas should fail");
    }

    #[test]
    #[serial]
    fn test_l1sload_zero_block_number() {
        clear_l1_storage();

        // Block 0 with l1origin at 100 — distance is 100, within 256 lookback
        let anchor = 0u64;
        let l1_origin = 100u64;
        let block = 0u64;
        let (_, _, _, expected_value) = setup_test_storage(anchor, l1_origin, block);
        let input = create_test_input(block);

        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(result.is_ok(), "Block 0 within lookback range should succeed");
        assert_eq!(result.unwrap().bytes.as_ref(), &expected_value.0);
    }

    #[test]
    #[serial]
    fn test_l1sload_same_key_different_blocks() {
        clear_l1_storage();
        set_anchor_block_id(100);
        set_l1_origin_block_id(110);

        let address = Address::from(TEST_ADDRESS);
        let key = B256::from(TEST_STORAGE_KEY);
        let value_at_105 = B256::from([0xAAu8; 32]);
        let value_at_110 = B256::from([0xBBu8; 32]);

        set_l1_storage_value(address, key, block_number_b256(105), value_at_105);
        set_l1_storage_value(address, key, block_number_b256(110), value_at_110);

        let result_105 = l1sload_run(&Bytes::from(create_test_input(105)), SUFFICIENT_GAS).unwrap();
        let result_110 = l1sload_run(&Bytes::from(create_test_input(110)), SUFFICIENT_GAS).unwrap();

        assert_eq!(result_105.bytes.as_ref(), value_at_105.as_slice());
        assert_eq!(result_110.bytes.as_ref(), value_at_110.as_slice());
        assert_ne!(
            result_105.bytes, result_110.bytes,
            "Different blocks should return different values"
        );
    }

    #[test]
    #[serial]
    fn test_l1sload_zero_storage_value() {
        clear_l1_storage();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        // Store B256::ZERO explicitly — should return zeros, not error
        let address = Address::from(TEST_ADDRESS);
        let key = B256::from(TEST_STORAGE_KEY);
        set_l1_storage_value(address, key, block_number_b256(100), B256::ZERO);

        let input = create_test_input(100);
        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(result.is_ok(), "Zero storage value should succeed");
        assert_eq!(result.unwrap().bytes.as_ref(), B256::ZERO.as_slice());
    }

    #[test]
    #[serial]
    fn test_l1sload_rpc_fallback_error_propagates() {
        clear_l1_storage();
        set_anchor_block_id(100);
        set_l1_origin_block_id(100);

        set_l1_rpc_fetcher(|_, _, _| Err("L1 node unavailable".to_string()));

        let input = create_test_input(100);
        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(result.is_err(), "RPC error should propagate");
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains("L1 node unavailable"),
            "Error message should contain RPC error: {err_msg}"
        );
    }

    #[test]
    #[serial]
    fn test_l1sload_anchor_equals_l1origin() {
        clear_l1_storage();

        // anchor == l1origin is the simplest valid case
        let block = 100u64;
        let (_, _, _, expected_value) = setup_test_storage(block, block, block);
        let input = create_test_input(block);

        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(result.is_ok(), "anchor == l1origin == requested should succeed");
        assert_eq!(result.unwrap().bytes.as_ref(), &expected_value.0);
    }

    #[test]
    #[serial]
    fn test_l1sload_l1origin_less_than_anchor_rejected() {
        clear_l1_storage();
        set_anchor_block_id(200);
        set_l1_origin_block_id(100); // l1origin < anchor — invalid

        let input = create_test_input(100);
        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(result.is_err(), "l1origin < anchor should be rejected");
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("l1origin < anchor"), "Expected context error: {err_msg}");
    }

    #[test]
    #[serial]
    fn test_l1sload_fails_without_l1_origin_block_id() {
        clear_l1_storage();
        set_anchor_block_id(100);
        // Deliberately do NOT set l1_origin_block_id

        let input = create_test_input(100);
        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);

        assert!(result.is_err(), "Should fail when L1 origin block ID is not set");
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains("L1 origin block ID not set"),
            "Expected missing l1_origin error: {err_msg}"
        );
    }

    #[test]
    #[serial]
    fn test_l1sload_multiple_rpc_calls_tracked() {
        clear_l1_storage();
        set_anchor_block_id(100);
        set_l1_origin_block_id(110);

        let val1 = B256::from([0x11u8; 32]);
        let val2 = B256::from([0x22u8; 32]);

        set_l1_rpc_fetcher(move |_, _, block| {
            if block == 105 {
                Ok(val1)
            } else if block == 110 {
                Ok(val2)
            } else {
                Err("unexpected block".to_string())
            }
        });

        let _ = l1sload_run(&Bytes::from(create_test_input(105)), SUFFICIENT_GAS).unwrap();
        let _ = l1sload_run(&Bytes::from(create_test_input(110)), SUFFICIENT_GAS).unwrap();

        let served = take_l1_rpc_served_calls();
        assert_eq!(served.len(), 2, "Should track both RPC-served calls");
    }
}
