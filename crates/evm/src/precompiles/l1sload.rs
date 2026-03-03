use std::{
    collections::{HashMap, HashSet},
    sync::{LazyLock, Mutex},
};

use alloy_primitives::{Address, Bytes, B256, U256};
use reth_revm::precompile::{
    u64_to_address, Precompile, PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult,
};
use tracing::{trace, warn};

/// The L1SLOAD precompile instance registered at address `0x10001`.
pub const L1SLOAD: Precompile = Precompile::new(
    PrecompileId::Custom(std::borrow::Cow::Borrowed("L1SLOAD")),
    u64_to_address(0x10001),
    l1sload_run,
);

/// Fixed gas cost for an L1SLOAD precompile call.
pub const L1SLOAD_FIXED_GAS: u64 = 2000;
/// Per-load gas cost for each storage slot read.
pub const L1SLOAD_PER_LOAD_GAS: u64 = 2000;

/// Expected input length: 20 bytes (address) + 32 bytes (storage key) + 32 bytes (block number),
/// total 84 bytes
pub const EXPECTED_INPUT_LENGTH: usize = 84;

/// Maximum number of L1 blocks to look back from the anchor block
pub const L1SLOAD_MAX_BLOCK_LOOKBACK: u64 = 256;

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
type L1StorageFetcher = Box<dyn Fn(Address, B256, u64) -> Result<B256, String> + Send + Sync>;

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

/// Clear the anchor block ID context
pub fn clear_anchor_block_id() {
    let mut ctx = CURRENT_ANCHOR_BLOCK_ID.lock().expect("CURRENT_ANCHOR_BLOCK_ID mutex poisoned");
    *ctx = None;
}

/// Get the current anchor block ID
fn get_anchor_block_id() -> Option<u64> {
    *CURRENT_ANCHOR_BLOCK_ID.lock().expect("CURRENT_ANCHOR_BLOCK_ID mutex poisoned")
}

/// Set the current L1 origin block ID context.
pub fn set_l1_origin_block_id(l1_origin_block_id: u64) {
    let mut ctx =
        CURRENT_L1_ORIGIN_BLOCK_ID.lock().expect("CURRENT_L1_ORIGIN_BLOCK_ID mutex poisoned");
    *ctx = Some(l1_origin_block_id);
}

/// Clear the L1 origin block ID context.
pub fn clear_l1_origin_block_id() {
    let mut ctx =
        CURRENT_L1_ORIGIN_BLOCK_ID.lock().expect("CURRENT_L1_ORIGIN_BLOCK_ID mutex poisoned");
    *ctx = None;
}

/// Get the current L1 origin block ID.
fn get_l1_origin_block_id() -> Option<u64> {
    *CURRENT_L1_ORIGIN_BLOCK_ID.lock().expect("CURRENT_L1_ORIGIN_BLOCK_ID mutex poisoned")
}

/// Set a value in the L1 storage cache (keyed by address, storage key, and block number)
pub fn set_l1_storage_value(
    contract_address: Address,
    storage_key: B256,
    block_number: B256,
    value: B256,
) {
    let mut cache = L1_STORAGE_CACHE.lock().expect("L1_STORAGE_CACHE mutex poisoned");
    cache.insert((contract_address, storage_key, block_number), value);
}

/// Clear L1 storage cache and anchor block ID context
pub fn clear_l1_storage() {
    L1_STORAGE_CACHE.lock().expect("L1_STORAGE_CACHE mutex poisoned").clear();
    clear_anchor_block_id();
    clear_l1_origin_block_id();
    clear_l1_rpc_fetcher();
    clear_l1_rpc_served_calls();
}

/// Set the L1 RPC fetcher callback for live storage value fetching on cache miss.
/// Called by surge-raiko during preflight when network access is available.
pub fn set_l1_rpc_fetcher(
    fetcher: impl Fn(Address, B256, u64) -> Result<B256, String> + Send + Sync + 'static,
) {
    *L1_RPC_FETCHER.lock().expect("L1_RPC_FETCHER mutex poisoned") = Some(Box::new(fetcher));
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

/// Get a value from the L1 storage cache
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

/// L1SLOAD precompile - read storage values from L1 contracts (RIP-7728).
///
/// The input to the L1SLOAD precompile consists of:
/// - [0..20]  (20 bytes)  - address: The L1 contract address
/// - [20..52] (32 bytes)  - storageKey: The storage key to read
/// - [52..84] (32 bytes)  - blockNumber: The L1 block number to read from
///
/// Output: Storage value (32 bytes)
///
/// The requested block number must be:
/// - at or before the L1 origin block (`requested <= l1origin`)
/// - not older than `L1SLOAD_MAX_BLOCK_LOOKBACK` relative to the anchor for backward reads
///   (`requested < anchor => anchor - requested <= max`)
pub fn l1sload_run(input: &[u8], gas_limit: u64) -> PrecompileResult {
    trace!("L1SLOAD: precompile called, input_len={}", input.len());

    let gas_used = L1SLOAD_FIXED_GAS + L1SLOAD_PER_LOAD_GAS;
    if gas_used > gas_limit {
        trace!("L1SLOAD: out of gas (need={}, have={})", gas_used, gas_limit);
        return Err(PrecompileError::OutOfGas);
    }

    if input.len() != EXPECTED_INPUT_LENGTH {
        trace!("L1SLOAD: invalid input length {}", input.len());
        return Err(PrecompileError::Other("Invalid input length".into()));
    }

    let contract_address = Address::from_slice(&input[0..20]);
    let storage_key = B256::from_slice(&input[20..52]);
    let block_number = B256::from_slice(&input[52..84]);

    trace!(
        "L1SLOAD: request contract={:?}, key={:?}, block={:?}",
        contract_address,
        storage_key,
        block_number
    );

    let anchor_block_id = match get_anchor_block_id() {
        Some(id) => id,
        None => {
            warn!("L1SLOAD: anchor block ID not set");
            return Err(PrecompileError::Other("Anchor block ID not set".into()));
        }
    };
    // Backward compatibility: if origin is unset, fall back to anchor-only mode.
    let l1_origin_block_id = get_l1_origin_block_id().unwrap_or(anchor_block_id);

    if l1_origin_block_id < anchor_block_id {
        warn!(
            "L1SLOAD: invalid context l1origin {} < anchor {}",
            l1_origin_block_id, anchor_block_id
        );
        return Err(PrecompileError::Other("Invalid L1SLOAD context: l1origin < anchor".into()));
    }

    let block_number_u256 = U256::from_be_bytes(block_number.0);
    let requested_block: u64 = block_number_u256
        .try_into()
        .map_err(|_| PrecompileError::Other("Block number too large".into()))?;

    if requested_block > l1_origin_block_id {
        warn!(
            "L1SLOAD: requested block {} > l1origin {} (anchor={})",
            requested_block, l1_origin_block_id, anchor_block_id
        );
        return Err(PrecompileError::Other(
            "Requested block number is after the L1 origin block".into(),
        ));
    }

    if requested_block < anchor_block_id
        && anchor_block_id - requested_block > L1SLOAD_MAX_BLOCK_LOOKBACK
    {
        warn!("L1SLOAD: block {} too old (anchor={})", requested_block, anchor_block_id);
        return Err(PrecompileError::Other(
            "Requested block number exceeds max lookback from anchor".into(),
        ));
    }

    let storage_value = get_l1_storage_value(contract_address, storage_key, block_number);

    match storage_value {
        Some(value) => {
            trace!("L1SLOAD: cache hit, value={:?}", value);
            let mut output = [0u8; 32];
            output.copy_from_slice(value.as_slice());
            Ok(PrecompileOutput::new(gas_used, Bytes::from(output)))
        }
        None => {
            // Cache miss — try live L1 RPC fallback (only available during preflight)
            let fetcher_guard = L1_RPC_FETCHER.lock().expect("L1_RPC_FETCHER mutex poisoned");
            if let Some(ref fetcher) = *fetcher_guard {
                let value = fetcher(contract_address, storage_key, requested_block)
                    .map_err(|e| PrecompileError::Other(format!("L1 RPC error: {e}").into()))?;
                drop(fetcher_guard);

                // Cache the fetched value for subsequent calls to the same slot
                set_l1_storage_value(contract_address, storage_key, block_number, value);

                // Record this call so surge-raiko can fetch its Merkle proof later
                L1_RPC_SERVED_CALLS.lock().expect("L1_RPC_SERVED_CALLS mutex poisoned").insert((
                    contract_address,
                    storage_key,
                    block_number,
                ));

                trace!("L1SLOAD: RPC fallback hit, value={:?}", value);
                let mut output = [0u8; 32];
                output.copy_from_slice(value.as_slice());
                Ok(PrecompileOutput::new(gas_used, Bytes::from(output)))
            } else {
                // No RPC available (ZK proving mode) — error
                warn!(
                    "L1SLOAD: cache miss for contract={:?}, key={:?}, block={:?}",
                    contract_address, storage_key, block_number
                );
                Err(PrecompileError::Other("L1 storage value not found in cache".into()))
            }
        }
    }
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
        set_anchor_block_id(1000);
        set_l1_origin_block_id(1000);

        // Request block 1000 - 257 = 743, which exceeds L1SLOAD_MAX_BLOCK_LOOKBACK (256)
        let input = create_test_input(1000 - L1SLOAD_MAX_BLOCK_LOOKBACK - 1);
        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(result.is_err(), "Should reject block beyond max lookback");
    }

    #[test]
    #[serial]
    fn test_l1sload_succeeds_with_older_block() {
        clear_l1_storage();

        let anchor = 200u64;
        let l1_origin = 250u64;
        let block = 150u64; // 50 blocks before anchor, within 256 lookback
        let (_, _, _, expected_value) = setup_test_storage(anchor, l1_origin, block);
        let input = create_test_input(block);

        let result = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS);
        assert!(result.is_ok(), "Should succeed for block within lookback range");
        assert_eq!(result.unwrap().bytes.as_ref(), &expected_value.0);
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
        clear_anchor_block_id();
        clear_l1_origin_block_id();

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

        clear_anchor_block_id();
        clear_l1_origin_block_id();
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
        clear_l1_rpc_served_calls();
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
        clear_l1_rpc_served_calls();
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
}
