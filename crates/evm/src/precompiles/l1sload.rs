//! L1SLOAD precompile (RIP-7728, address `0x10001`).
//!
//! **Global-state invariant.** The storage cache, fetcher, origin context, and
//! RPC-served-calls set are process-global `LazyLock<Mutex<_>>`. They are safe only when the
//! host drives the precompile single-threaded with one origin context at a time — the normal
//! preflight and ZK-guest pattern. Running two different contexts concurrently silently
//! cross-contaminates cache hits.
//!
//! **Cache lifecycle.** No automatic eviction. The caller must invoke [`clear_l1_storage`] at
//! the top of every new block / batch iteration. Tests use `#[serial]` to serialize the
//! global-state mutations.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex},
};

use alloy_primitives::{Address, B256, Bytes, U256};
use reth_revm::precompile::{PrecompileHalt, PrecompileOutput, PrecompileResult};
use tracing::{debug, trace, warn};

// Re-export the shared origin context functions through this module so consumers (notably
// raiko) can import `alethia_reth_evm::precompiles::l1sload::{set_l1_origin_block_id, ...}`.
// They operate on the same global that `l1staticcall` reads via `super::context::get_*`.
pub use super::context::{
    clear_l1_origin_context, get_l1_origin_block_id, set_l1_origin_block_id,
    set_record_l1_served_calls, should_record_l1_served_calls,
};

/// Fixed gas cost for an L1SLOAD precompile call.
const L1SLOAD_FIXED_GAS: u64 = 2000;
/// Per-load gas cost for each storage slot read.
const L1SLOAD_PER_LOAD_GAS: u64 = 2000;

/// Expected input length: 20B address + 32B storage key + 32B block number = 84 bytes.
const EXPECTED_INPUT_LENGTH: usize = 84;

/// Maximum number of L1 blocks to look back from the L1 origin block.
const L1SLOAD_MAX_BLOCK_LOOKBACK: u64 = 256;

/// Type alias for the L1 storage cache map. Key: `(contract, key, block)`; value: storage value.
type L1StorageCache = HashMap<(Address, B256, B256), B256>;

/// In-memory cache for L1 storage values.
static L1_STORAGE_CACHE: LazyLock<Mutex<L1StorageCache>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Callback for fetching L1 storage values via RPC. Set by the host during preflight (has
/// network access), `None` during ZK proving. `Arc` so callers can clone the pointer out of
/// the lock before the (slow) RPC, keeping `set_*`/`clear_*` unblocked and the mutex
/// un-poisoned if the fetcher panics.
type L1StorageFetcher = Arc<dyn Fn(Address, B256, u64) -> Result<B256, String> + Send + Sync>;

/// Live L1 RPC fetcher for handling cache misses on indirect L1SLOAD calls.
static L1_RPC_FETCHER: LazyLock<Mutex<Option<L1StorageFetcher>>> =
    LazyLock::new(|| Mutex::new(None));

/// Tracks L1SLOAD calls served via the live RPC fetcher (not the pre-fetched cache). The host
/// reads this after execution to fetch Merkle proofs for the ZK prover. Key: `(contract, key,
/// block as B256)`.
static L1_RPC_SERVED_CALLS: LazyLock<Mutex<HashSet<(Address, B256, B256)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

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

/// Clear all L1SLOAD state (cache, origin context, RPC fetcher, tracked calls). Does NOT touch
/// L1STATICCALL state — clear that separately.
pub fn clear_l1_storage() {
    L1_STORAGE_CACHE.lock().expect("L1_STORAGE_CACHE mutex poisoned").clear();
    clear_l1_origin_context();
    clear_l1_rpc_fetcher();
    clear_l1_rpc_served_calls();
}

/// Set the L1 RPC fetcher callback for live storage fetching on cache miss.
pub fn set_l1_rpc_fetcher(
    fetcher: impl Fn(Address, B256, u64) -> Result<B256, String> + Send + Sync + 'static,
) {
    *L1_RPC_FETCHER.lock().expect("L1_RPC_FETCHER mutex poisoned") = Some(Arc::new(fetcher));
}

/// Clear the L1 RPC fetcher (disables live RPC fallback).
pub fn clear_l1_rpc_fetcher() {
    *L1_RPC_FETCHER.lock().expect("L1_RPC_FETCHER mutex poisoned") = None;
}

/// Take (and clear) all L1SLOAD calls served via live RPC, for proof fetching.
pub fn take_l1_rpc_served_calls() -> HashSet<(Address, B256, B256)> {
    std::mem::take(&mut *L1_RPC_SERVED_CALLS.lock().expect("L1_RPC_SERVED_CALLS mutex poisoned"))
}

/// Clear tracked calls served via live L1 RPC.
pub fn clear_l1_rpc_served_calls() {
    L1_RPC_SERVED_CALLS.lock().expect("L1_RPC_SERVED_CALLS mutex poisoned").clear();
}

/// Evict cache entries whose block falls below the `[l1_origin − 256, l1_origin]` lookback
/// window. Called once per block from the executor hook so a long-running live node doesn't
/// accumulate stale entries indefinitely (the precompile would reject reads of those blocks
/// anyway, so they're dead memory). O(N) over the cache, but N is bounded by recent traffic.
pub fn evict_stale_l1_storage_entries(l1_origin: u64) {
    let floor = l1_origin.saturating_sub(L1SLOAD_MAX_BLOCK_LOOKBACK);
    let mut cache = L1_STORAGE_CACHE.lock().expect("L1_STORAGE_CACHE mutex poisoned");
    let prior = cache.len();
    cache.retain(|(_, _, block), _| {
        let block_n: u64 = U256::from_be_bytes(block.0).try_into().unwrap_or(u64::MAX);
        block_n >= floor
    });
    let removed = prior - cache.len();
    if removed > 0 {
        debug!("L1SLOAD: evicted {removed} cache entries below block {floor}");
    }
}

/// Look up a cached L1 storage value by address, key, and block number.
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

/// L1SLOAD precompile (RIP-7728): read an L1 storage value.
/// Input: 20B address + 32B key + 32B block number. Output: 32B value.
pub fn l1sload_run(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    let gas_used = L1SLOAD_FIXED_GAS + L1SLOAD_PER_LOAD_GAS;
    if gas_used > gas_limit {
        return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir));
    }
    if input.len() != EXPECTED_INPUT_LENGTH {
        return Ok(PrecompileOutput::halt(PrecompileHalt::other("Invalid input length"), reservoir));
    }

    let contract_address = Address::from_slice(&input[0..20]);
    let storage_key = B256::from_slice(&input[20..52]);
    let block_number = B256::from_slice(&input[52..84]);

    // The trusted window is `[origin − 256, origin]`, bound on-chain via `originBlockHash`.
    let l1_origin_block_id = match get_l1_origin_block_id() {
        Some(id) => id,
        None => {
            warn!("L1SLOAD: L1 origin block ID not set");
            return Ok(PrecompileOutput::halt(
                PrecompileHalt::other(
                    "L1SLOAD context unset (L1 precompiles not enabled for this fork, or the host \
                     did not set the L1 origin block for this block)",
                ),
                reservoir,
            ));
        }
    };

    let requested_block: u64 = match U256::from_be_bytes(block_number.0).try_into() {
        Ok(n) => n,
        Err(_) => {
            return Ok(PrecompileOutput::halt(
                PrecompileHalt::other("Block number too large"),
                reservoir,
            ));
        }
    };

    if requested_block > l1_origin_block_id {
        debug!("L1SLOAD: rejected block {requested_block} > origin {l1_origin_block_id}");
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::other("Requested block number is after the L1 origin block"),
            reservoir,
        ));
    }
    if l1_origin_block_id - requested_block > L1SLOAD_MAX_BLOCK_LOOKBACK {
        debug!("L1SLOAD: rejected block {requested_block} too old (origin={l1_origin_block_id})");
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::other(
                "Requested block number exceeds max lookback from L1 origin block",
            ),
            reservoir,
        ));
    }

    let value = if let Some(cached) =
        get_l1_storage_value(contract_address, storage_key, block_number)
    {
        trace!(
            "L1SLOAD: cache hit contract={contract_address:?} key={storage_key:?} block={requested_block}"
        );
        cached
    } else {
        // Clone the Arc out of the lock so the (slow) RPC runs without holding the mutex.
        let fetcher =
            L1_RPC_FETCHER.lock().expect("L1_RPC_FETCHER mutex poisoned").as_ref().map(Arc::clone);
        if let Some(fetcher) = fetcher {
            debug!(
                "L1SLOAD: RPC fallback contract={contract_address:?} key={storage_key:?} block={requested_block}"
            );
            let fetched = match fetcher(contract_address, storage_key, requested_block) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(PrecompileOutput::halt(
                        PrecompileHalt::other(format!("L1 RPC error: {e}")),
                        reservoir,
                    ));
                }
            };
            set_l1_storage_value(contract_address, storage_key, block_number, fetched);
            if should_record_l1_served_calls() {
                L1_RPC_SERVED_CALLS.lock().expect("L1_RPC_SERVED_CALLS mutex poisoned").insert((
                    contract_address,
                    storage_key,
                    block_number,
                ));
            }
            fetched
        } else {
            warn!(
                "L1SLOAD: cache miss + no RPC — contract={contract_address:?} key={storage_key:?} block={block_number:?}"
            );
            return Ok(PrecompileOutput::halt(
                PrecompileHalt::other("L1 storage value not found in cache"),
                reservoir,
            ));
        }
    };

    Ok(PrecompileOutput::new(gas_used, Bytes::copy_from_slice(value.as_slice()), reservoir))
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

    fn block_number_b256(n: u64) -> B256 {
        B256::from(U256::from(n))
    }

    /// Build 84-byte input with the given block number.
    fn create_test_input(block_number: u64) -> Vec<u8> {
        let mut input = vec![0u8; EXPECTED_INPUT_LENGTH];
        input[0..20].copy_from_slice(&TEST_ADDRESS);
        input[20..52].copy_from_slice(&TEST_STORAGE_KEY);
        input[52..84].copy_from_slice(block_number_b256(block_number).as_slice());
        input
    }

    /// Seed origin context + cache with a value at `block`.
    fn setup_test_storage(origin: u64, block: u64) -> (Address, B256, B256, B256) {
        let address = Address::from(TEST_ADDRESS);
        let key = B256::from(TEST_STORAGE_KEY);
        let block_num = block_number_b256(block);
        let value = B256::from(TEST_STORAGE_VALUE);
        set_l1_origin_block_id(origin);
        set_l1_storage_value(address, key, block_num, value);
        (address, key, block_num, value)
    }

    #[test]
    #[serial]
    fn test_l1sload_rejects_invalid_input_lengths() {
        clear_l1_storage();
        set_l1_origin_block_id(100);
        let short = Bytes::from(vec![0u8; 52]);
        assert!(
            matches!(&l1sload_run(&short, SUFFICIENT_GAS, 0), Ok(o) if o.is_halt()),
            "reject short input"
        );
        let long = Bytes::from(vec![0u8; 116]);
        assert!(
            matches!(&l1sload_run(&long, SUFFICIENT_GAS, 0), Ok(o) if o.is_halt()),
            "reject long input"
        );
    }

    #[test]
    #[serial]
    fn test_l1sload_fails_without_origin() {
        clear_l1_storage();
        let input = create_test_input(100);
        let out = l1sload_run(&Bytes::from(input), SUFFICIENT_GAS, 0).expect("expected Ok halt");
        assert!(out.is_halt(), "should fail when origin block ID is not set");
        let halt = format!("{:?}", out.halt_reason().expect("halt"));
        assert!(halt.contains("L1SLOAD context unset"), "got: {halt}");
    }

    #[test]
    #[serial]
    fn test_l1sload_fails_without_cached_storage() {
        clear_l1_storage();
        set_l1_origin_block_id(100);
        let input = create_test_input(100);
        assert!(
            matches!(&l1sload_run(&Bytes::from(input), SUFFICIENT_GAS, 0), Ok(o) if o.is_halt()),
            "no cache"
        );
    }

    #[test]
    #[serial]
    fn test_l1sload_succeeds_with_cached_storage() {
        clear_l1_storage();
        let (_, _, _, expected) = setup_test_storage(100, 100);
        let result = l1sload_run(&Bytes::from(create_test_input(100)), SUFFICIENT_GAS, 0);
        assert!(result.is_ok(), "cached value should succeed: {:?}", result.err());
        let output = result.unwrap();
        assert_eq!(
            output.gas_used,
            L1SLOAD_FIXED_GAS + L1SLOAD_PER_LOAD_GAS,
            "fixed + per-load gas"
        );
        assert_eq!(output.bytes.len(), 32, "32-byte output");
        assert_eq!(output.bytes.as_ref(), &expected.0, "output matches cached value");
    }

    #[test]
    #[serial]
    fn test_l1sload_fails_with_insufficient_gas() {
        clear_l1_storage();
        set_l1_origin_block_id(100);
        let input = Bytes::from(create_test_input(100));
        assert!(
            matches!(&l1sload_run(&input, INSUFFICIENT_GAS, 0), Ok(o) if o.is_halt()),
            "insufficient gas"
        );
    }

    #[test]
    #[serial]
    fn test_l1sload_rejects_block_after_origin() {
        clear_l1_storage();
        set_l1_origin_block_id(110);
        let input = create_test_input(111);
        assert!(
            matches!(&l1sload_run(&Bytes::from(input), SUFFICIENT_GAS, 0), Ok(o) if o.is_halt()),
            "block after origin"
        );
    }

    #[test]
    #[serial]
    fn test_l1sload_rejects_block_beyond_lookback() {
        clear_l1_storage();
        set_l1_origin_block_id(1000);
        // origin - 257 = 743, distance 257 > 256.
        let input = create_test_input(1000 - L1SLOAD_MAX_BLOCK_LOOKBACK - 1);
        assert!(
            matches!(&l1sload_run(&Bytes::from(input), SUFFICIENT_GAS, 0), Ok(o) if o.is_halt()),
            "beyond lookback"
        );
    }

    #[test]
    #[serial]
    fn test_l1sload_succeeds_with_older_block() {
        clear_l1_storage();
        let (_, _, _, expected) = setup_test_storage(250, 150); // 100 blocks before origin, within 256
        let result = l1sload_run(&Bytes::from(create_test_input(150)), SUFFICIENT_GAS, 0);
        assert!(result.is_ok(), "block within lookback should succeed");
        assert_eq!(result.unwrap().bytes.as_ref(), &expected.0);
    }

    #[test]
    #[serial]
    fn test_storage_cache_operations() {
        clear_l1_storage();
        let address = Address::from(TEST_ADDRESS);
        let key = B256::from(TEST_STORAGE_KEY);
        let block = block_number_b256(100);
        let value = B256::from(TEST_STORAGE_VALUE);
        assert!(get_l1_storage_value(address, key, block).is_none(), "empty initially");
        set_l1_storage_value(address, key, block, value);
        assert_eq!(get_l1_storage_value(address, key, block), Some(value), "retrieve cached value");
        assert!(
            get_l1_storage_value(address, key, block_number_b256(101)).is_none(),
            "different block"
        );
    }

    #[test]
    #[serial]
    fn test_origin_context() {
        clear_l1_storage();
        assert!(get_l1_origin_block_id().is_none(), "empty initially");
        set_l1_origin_block_id(50);
        assert_eq!(get_l1_origin_block_id(), Some(50));
        clear_l1_storage();
        assert!(get_l1_origin_block_id().is_none(), "cleared");
    }

    #[test]
    #[serial]
    fn test_cache_key_uniqueness() {
        clear_l1_storage();
        let (a1, a2) = (Address::from([1u8; 20]), Address::from([2u8; 20]));
        let (k1, k2) = (B256::from([1u8; 32]), B256::from([2u8; 32]));
        let (b1, b2) = (block_number_b256(100), block_number_b256(200));
        let (v1, v2) = (B256::from([10u8; 32]), B256::from([20u8; 32]));
        set_l1_storage_value(a1, k1, b1, v1);
        set_l1_storage_value(a2, k2, b2, v2);
        assert_eq!(get_l1_storage_value(a1, k1, b1), Some(v1));
        assert_eq!(get_l1_storage_value(a2, k2, b2), Some(v2));
        assert!(get_l1_storage_value(a1, k2, b1).is_none());
        assert!(get_l1_storage_value(a2, k1, b2).is_none());
        assert!(get_l1_storage_value(a1, k1, b2).is_none());
    }

    #[test]
    #[serial]
    fn test_l1sload_rpc_fallback_records_served_calls() {
        clear_l1_storage();
        set_l1_origin_block_id(100);
        let address = Address::from(TEST_ADDRESS);
        let key = B256::from(TEST_STORAGE_KEY);
        let input = Bytes::from(create_test_input(100));
        let fallback_value = B256::from([9u8; 32]);
        set_l1_rpc_fetcher(move |addr, k, block| {
            if addr != address || k != key || block != 100 {
                return Err("unexpected fallback request".to_string());
            }
            Ok(fallback_value)
        });

        let first = l1sload_run(&input, SUFFICIENT_GAS, 0).expect("RPC fallback should succeed");
        assert_eq!(first.bytes.as_ref(), fallback_value.as_slice());

        let served = take_l1_rpc_served_calls();
        assert_eq!(served.len(), 1, "exactly one served call");
        assert!(served.contains(&(address, key, block_number_b256(100))));

        // Second call served from cache even with fallback disabled.
        clear_l1_rpc_fetcher();
        let second = l1sload_run(&input, SUFFICIENT_GAS, 0).expect("cache hit should succeed");
        assert_eq!(second.bytes.as_ref(), fallback_value.as_slice());
    }

    #[test]
    #[serial]
    fn test_clear_l1_storage_clears_rpc_fallback_state() {
        clear_l1_storage();
        set_l1_origin_block_id(100);
        let input = Bytes::from(create_test_input(100));
        set_l1_rpc_fetcher(move |_, _, _| Ok(B256::from([11u8; 32])));
        let _ = l1sload_run(&input, SUFFICIENT_GAS, 0).expect("fallback serves first request");
        assert_eq!(take_l1_rpc_served_calls().len(), 1, "served call tracked");

        clear_l1_storage();
        assert!(take_l1_rpc_served_calls().is_empty(), "served calls cleared");

        // Context cleared → call halts before any fetcher use.
        let out = l1sload_run(&input, SUFFICIENT_GAS, 0).expect("expected Ok halt");
        assert!(out.is_halt(), "context cleared");
        assert!(
            format!("{:?}", out.halt_reason().expect("halt")).contains("L1SLOAD context unset")
        );
    }

    #[test]
    #[serial]
    fn test_l1sload_exact_lookback_boundary() {
        clear_l1_storage();
        let origin = 1000u64;
        let block = origin - L1SLOAD_MAX_BLOCK_LOOKBACK; // 744
        let (_, _, _, expected) = setup_test_storage(origin, block);
        let result = l1sload_run(&Bytes::from(create_test_input(block)), SUFFICIENT_GAS, 0);
        assert!(result.is_ok(), "block at exact lookback boundary should succeed");
        assert_eq!(result.unwrap().bytes.as_ref(), &expected.0);
    }

    #[test]
    #[serial]
    fn test_l1sload_exact_origin() {
        clear_l1_storage();
        let (_, _, _, expected) = setup_test_storage(200, 200);
        let result = l1sload_run(&Bytes::from(create_test_input(200)), SUFFICIENT_GAS, 0);
        assert!(result.is_ok(), "block at exact origin should succeed");
        assert_eq!(result.unwrap().bytes.as_ref(), &expected.0);
    }

    #[test]
    #[serial]
    fn test_l1sload_exact_gas_boundary() {
        clear_l1_storage();
        setup_test_storage(100, 100);
        let input = create_test_input(100);
        assert!(l1sload_run(&Bytes::from(input.clone()), SUFFICIENT_GAS, 0).is_ok(), "exact gas");
        assert!(
            matches!(&l1sload_run(&Bytes::from(input.clone()), SUFFICIENT_GAS - 1, 0), Ok(o) if o.is_halt()),
            "one below"
        );
        assert!(
            matches!(&l1sload_run(&Bytes::from(input), 0, 0), Ok(o) if o.is_halt()),
            "zero gas"
        );
    }

    #[test]
    #[serial]
    fn test_l1sload_zero_block_number() {
        clear_l1_storage();
        let (_, _, _, expected) = setup_test_storage(100, 0); // distance 100, within 256
        let result = l1sload_run(&Bytes::from(create_test_input(0)), SUFFICIENT_GAS, 0);
        assert!(result.is_ok(), "block 0 within lookback should succeed");
        assert_eq!(result.unwrap().bytes.as_ref(), &expected.0);
    }

    #[test]
    #[serial]
    fn test_l1sload_same_key_different_blocks() {
        clear_l1_storage();
        set_l1_origin_block_id(110);
        let address = Address::from(TEST_ADDRESS);
        let key = B256::from(TEST_STORAGE_KEY);
        let (v105, v110) = (B256::from([0xAAu8; 32]), B256::from([0xBBu8; 32]));
        set_l1_storage_value(address, key, block_number_b256(105), v105);
        set_l1_storage_value(address, key, block_number_b256(110), v110);
        let r105 = l1sload_run(&Bytes::from(create_test_input(105)), SUFFICIENT_GAS, 0).unwrap();
        let r110 = l1sload_run(&Bytes::from(create_test_input(110)), SUFFICIENT_GAS, 0).unwrap();
        assert_eq!(r105.bytes.as_ref(), v105.as_slice());
        assert_eq!(r110.bytes.as_ref(), v110.as_slice());
        assert_ne!(r105.bytes, r110.bytes, "different blocks → different values");
    }

    #[test]
    #[serial]
    fn test_l1sload_zero_storage_value() {
        clear_l1_storage();
        set_l1_origin_block_id(100);
        let address = Address::from(TEST_ADDRESS);
        let key = B256::from(TEST_STORAGE_KEY);
        set_l1_storage_value(address, key, block_number_b256(100), B256::ZERO);
        let result = l1sload_run(&Bytes::from(create_test_input(100)), SUFFICIENT_GAS, 0);
        assert!(result.is_ok(), "explicit zero value should succeed");
        assert_eq!(result.unwrap().bytes.as_ref(), B256::ZERO.as_slice());
    }

    #[test]
    #[serial]
    fn test_l1sload_rpc_fallback_error_propagates() {
        clear_l1_storage();
        set_l1_origin_block_id(100);
        set_l1_rpc_fetcher(|_, _, _| Err("L1 node unavailable".to_string()));
        let result = l1sload_run(&Bytes::from(create_test_input(100)), SUFFICIENT_GAS, 0);
        assert!(matches!(&result, Ok(o) if o.is_halt()), "RPC error should propagate");
        let halt = format!("{:?}", result.unwrap().halt_reason().expect("halt"));
        assert!(halt.contains("L1 node unavailable"), "error should contain RPC error: {halt}");
    }

    #[test]
    #[serial]
    fn test_l1sload_multiple_rpc_calls_tracked() {
        clear_l1_storage();
        set_l1_origin_block_id(110);
        let (v1, v2) = (B256::from([0x11u8; 32]), B256::from([0x22u8; 32]));
        set_l1_rpc_fetcher(move |_, _, block| match block {
            105 => Ok(v1),
            110 => Ok(v2),
            _ => Err("unexpected block".to_string()),
        });
        let _ = l1sload_run(&Bytes::from(create_test_input(105)), SUFFICIENT_GAS, 0).unwrap();
        let _ = l1sload_run(&Bytes::from(create_test_input(110)), SUFFICIENT_GAS, 0).unwrap();
        assert_eq!(take_l1_rpc_served_calls().len(), 2, "both RPC-served calls tracked");
    }
}
