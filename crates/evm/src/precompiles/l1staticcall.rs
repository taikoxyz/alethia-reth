//! L1STATICCALL precompile (address `0x10002`).
//!
//! **Global-state invariant.** The cache, fetcher, and RPC-served-calls list are process-global
//! `LazyLock<Mutex<_>>`. They are safe only when the host drives the precompile single-threaded
//! with one L1 origin context at a time — the normal preflight and ZK-guest pattern. Running two
//! different contexts concurrently silently cross-contaminates cache hits.
//!
//! **Cache lifecycle.** No automatic eviction. The caller must invoke [`clear_l1_staticcall_cache`]
//! at the top of every new block / batch iteration. Tests use `#[serial]` to serialize the
//! global-state mutations.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use reth_revm::precompile::{PrecompileHalt, PrecompileOutput, PrecompileResult};
use tracing::{debug, error, trace, warn};

use super::context::{get_l1_origin_block_id, should_record_l1_served_calls};

/// Fixed gas cost for an L1STATICCALL precompile call.
const L1STATICCALL_FIXED_GAS: u64 = 2000;
/// Per-call overhead gas cost.
const L1STATICCALL_PER_CALL_OVERHEAD: u64 = 10000;
/// Per-byte gas cost for calldata beyond the minimum 52-byte header.
const L1STATICCALL_PER_BYTE_CALLDATA_GAS: u64 = 16;

/// Minimum input length: 20B address + 32B block number = 52 bytes. Calldata beyond this is
/// optional (variable-length).
const MIN_INPUT_LENGTH: usize = 52;

/// Maximum size of return data from an L1STATICCALL (24 KB).
const MAX_RETURN_DATA_SIZE: usize = 24576;

/// Maximum number of L1 blocks to look back from the L1 origin block.
const L1STATICCALL_MAX_BLOCK_LOOKBACK: u64 = 256;

/// Maximum L1 gas budget for a single L1STATICCALL execution. Shared across the L2 precompile
/// (live execution + cache clamp), the live `debug_traceCall` fetcher (alethia-reth-l1-client),
/// and the prover's preflight + guest re-execution. Keeping them in lockstep prevents
/// sequencer↔prover OOM divergence: if any party uses a different budget, an honest L1 view
/// could land on a different code path and the proof would fail.
pub const L1STATICCALL_GAS_CAP: u64 = 30_000_000;

/// Caller address pinned for all L1 reads through the L1STATICCALL pipeline (live + prover).
/// `Address::ZERO` matches Nethermind's `debug_traceCall` / `proof_call` defaults and keeps the
/// guest re-executor's `TxEnv.caller` byte-identical to the sequencer's view.
pub const L1_PRECOMPILE_CALLER: Address = Address::ZERO;

/// Type alias for the L1 staticcall cache map.
/// Key: `(target, block, keccak256(calldata))`; value: `(l1_gas_used, return_data, is_reverted)`.
type L1StaticCallCache = HashMap<(Address, u64, B256), (u64, Vec<u8>, bool)>;

/// In-memory cache for L1 staticcall results.
static L1_STATICCALL_CACHE: LazyLock<Mutex<L1StaticCallCache>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Callback for fetching L1 staticcall results via RPC. Must call `debug_traceCall` (not
/// `eth_call`) to capture actual gas. Args: `(target, block, gas_limit, calldata)` ->
/// `(gas_used, return_data, is_reverted)`. `Arc` so callers can drop the lock before the
/// (slow) RPC and a fetcher panic doesn't poison the mutex.
type L1StaticCallFetcher =
    Arc<dyn Fn(Address, u64, u64, &[u8]) -> Result<(u64, Vec<u8>, bool), String> + Send + Sync>;

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
    /// Actual gas consumed on L1, as reported by `debug_traceCall`.
    pub gas_used: u64,
    /// Whether the traced L1 call reverted.
    pub is_reverted: bool,
}

/// Tracks L1STATICCALL calls served via the live RPC fetcher. The host reads this after
/// execution to fetch proofs/witnesses for the ZK prover.
static L1_STATICCALL_RPC_SERVED_CALLS: LazyLock<Mutex<Vec<L1StaticCallRecord>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Insert a value into the L1 staticcall cache (keyed by `keccak256(calldata)`).
///
/// Oversized inputs are dropped (with a high-visibility `error!` + backtrace): the host trims
/// to the same ceiling before calling, so this only fires on a buggy caller. Failing here keeps
/// oversized values out of the cache rather than surfacing later as a confusing read-side error.
///
/// **Revert invariant**: when `is_reverted == true`, both `gas_used` and `result` must be
/// zeroed — matches NMC's `GethLikeTxTracer.MarkAsFailed` contract that the guest verifier
/// enforces. Asserted in debug builds; release builds silently accept inconsistent reverted
/// entries (the precompile's revert branch ignores `gas_used`/`result` anyway, but downstream
/// callers reading the cache may be confused).
pub fn set_l1_staticcall_value(
    target: Address,
    block_number: u64,
    calldata: &[u8],
    gas_used: u64,
    result: Vec<u8>,
    is_reverted: bool,
) {
    debug_assert!(
        !is_reverted || (gas_used == 0 && result.is_empty()),
        "L1STATICCALL revert contract violated: is_reverted=true requires gas_used==0 && \
         result.is_empty() (got gas_used={gas_used}, result.len()={})",
        result.len(),
    );
    if result.len() > MAX_RETURN_DATA_SIZE {
        error!(
            "L1STATICCALL: refusing to cache oversized return data — target={target:?} \
             block={block_number} bytes={} (cap={})\nbacktrace:\n{}",
            result.len(),
            MAX_RETURN_DATA_SIZE,
            std::backtrace::Backtrace::capture(),
        );
        return;
    }
    let calldata_hash = keccak256(calldata);
    let mut cache = L1_STATICCALL_CACHE.lock().expect("L1_STATICCALL_CACHE mutex poisoned");
    cache.insert((target, block_number, calldata_hash), (gas_used, result, is_reverted));
}

/// Clear the L1STATICCALL cache only (does NOT clear the shared L1 origin context).
pub fn clear_l1_staticcall_cache() {
    let mut cache = L1_STATICCALL_CACHE.lock().expect("L1_STATICCALL_CACHE mutex poisoned");
    let prior = cache.len();
    cache.clear();
    if prior > 0 {
        debug!("L1STATICCALL: cache cleared (evicted {prior} entries)");
    }
}

/// Set the L1 staticcall RPC fetcher callback for live fetching on cache miss.
pub fn set_l1_staticcall_rpc_fetcher(
    fetcher: impl Fn(Address, u64, u64, &[u8]) -> Result<(u64, Vec<u8>, bool), String>
    + Send
    + Sync
    + 'static,
) {
    debug!("L1STATICCALL: RPC fetcher installed (live-fetch fallback enabled)");
    *L1_STATICCALL_RPC_FETCHER.lock().expect("L1_STATICCALL_RPC_FETCHER mutex poisoned") =
        Some(Arc::new(fetcher));
}

/// Clear the L1 staticcall RPC fetcher (disables live RPC fallback).
pub fn clear_l1_staticcall_rpc_fetcher() {
    let mut slot =
        L1_STATICCALL_RPC_FETCHER.lock().expect("L1_STATICCALL_RPC_FETCHER mutex poisoned");
    if slot.is_some() {
        debug!("L1STATICCALL: RPC fetcher cleared (live-fetch fallback disabled)");
    }
    *slot = None;
}

/// Take (and clear) all L1STATICCALL calls served via live RPC, for witness fetching.
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

/// Evict cache entries whose block falls below the `[l1_origin − 256, l1_origin]` lookback
/// window. Called once per block from the executor hook so a long-running live node doesn't
/// accumulate stale entries indefinitely. Pairs with `evict_stale_l1_storage_entries` for
/// L1Sload.
pub fn evict_stale_l1_staticcall_entries(l1_origin: u64) {
    let floor = l1_origin.saturating_sub(L1STATICCALL_MAX_BLOCK_LOOKBACK);
    let mut cache = L1_STATICCALL_CACHE.lock().expect("L1_STATICCALL_CACHE mutex poisoned");
    let prior = cache.len();
    cache.retain(|(_, block_n, _), _| *block_n >= floor);
    let removed = prior - cache.len();
    if removed > 0 {
        debug!("L1STATICCALL: evicted {removed} cache entries below block {floor}");
    }
}

/// Look up a cached L1 staticcall result, returning `(gas_used, return_data, is_reverted)`.
fn get_l1_staticcall_value(
    target: Address,
    block_number: u64,
    calldata: &[u8],
) -> Option<(u64, Vec<u8>, bool)> {
    let calldata_hash = keccak256(calldata);
    L1_STATICCALL_CACHE
        .lock()
        .expect("L1_STATICCALL_CACHE mutex poisoned")
        .get(&(target, block_number, calldata_hash))
        .cloned()
}

/// L1STATICCALL precompile: execute a static call against an L1 contract.
///
/// Input: `[0..20)` target address, `[20..52)` block number (big-endian U256), `[52..)` calldata
/// (variable, may be empty). Output: variable-length return data (capped at 24 KB).
pub fn l1staticcall_run(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    debug!("L1STATICCALL: precompile called, input_len={} gas_limit={}", input.len(), gas_limit);

    // Validate input before charging gas so malformed short inputs surface as "Invalid input
    // length" instead of OutOfGas.
    if input.len() < MIN_INPUT_LENGTH {
        warn!(
            "L1STATICCALL: rejected invalid input length {}, minimum {}",
            input.len(),
            MIN_INPUT_LENGTH
        );
        return Ok(PrecompileOutput::halt(PrecompileHalt::other("Invalid input length"), reservoir));
    }

    // Static gas: fixed + overhead + per-byte for calldata beyond the header. Dynamic L1 gas is
    // added after execution from cache or fetcher.
    let extra_calldata_len = input.len() - MIN_INPUT_LENGTH;
    let static_gas = L1STATICCALL_FIXED_GAS +
        L1STATICCALL_PER_CALL_OVERHEAD +
        L1STATICCALL_PER_BYTE_CALLDATA_GAS * (extra_calldata_len as u64);
    if static_gas > gas_limit {
        return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir));
    }

    // Parse input fields.
    let target = Address::from_slice(&input[0..20]);
    let block_number_bytes = &input[20..52];
    let calldata = &input[52..];

    let requested_block: u64 = match U256::from_be_slice(block_number_bytes).try_into() {
        Ok(n) => n,
        Err(_) => {
            return Ok(PrecompileOutput::halt(
                PrecompileHalt::other("Block number too large"),
                reservoir,
            ));
        }
    };

    // The trusted window is `[origin − 256, origin]`, bound on-chain via `originBlockHash`.
    let l1_origin_block_id = match get_l1_origin_block_id() {
        Some(id) => id,
        None => {
            warn!("L1STATICCALL: L1 origin block ID not set");
            return Ok(PrecompileOutput::halt(
                PrecompileHalt::other(
                    "L1STATICCALL context unset (L1 precompiles not enabled for this fork, or the \
                     host did not set the L1 origin block for this block)",
                ),
                reservoir,
            ));
        }
    };

    if requested_block > l1_origin_block_id {
        debug!("L1STATICCALL: rejected block {requested_block} > origin {l1_origin_block_id}");
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::other("Requested block number is after the L1 origin block"),
            reservoir,
        ));
    }
    if l1_origin_block_id - requested_block > L1STATICCALL_MAX_BLOCK_LOOKBACK {
        debug!(
            "L1STATICCALL: rejected block {requested_block} too old (origin={l1_origin_block_id})"
        );
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::other(
                "Requested block number exceeds max lookback from L1 origin block",
            ),
            reservoir,
        ));
    }

    let (l1_gas, result, is_reverted) = if let Some((cached_gas, cached_data, cached_reverted)) =
        get_l1_staticcall_value(target, requested_block, calldata)
    {
        trace!(
            "L1STATICCALL: cache hit target={target:?} block={requested_block} calldata_len={}",
            calldata.len()
        );
        // Clamp cached L1 gas the same way the RPC path does, so a cache entry seeded with a
        // larger budget can't overcharge a later caller with a smaller `gas_limit`.
        //
        // Implication: two callers in the same block with different remaining budgets see
        // different reported gas off the same cache entry. This is deterministic per-frame
        // (the L2 block's gas_limit is committed in the block), so the prover sees the same
        // value as the live executor — safe for ZK proving even though cache hits aren't
        // pure functions of `(target, block, calldata)`.
        let effective_gas_limit = gas_limit.min(L1STATICCALL_GAS_CAP);
        (cached_gas.min(effective_gas_limit), cached_data, cached_reverted)
    } else {
        // Clone the Arc out of the lock so the (slow) fetcher runs without holding the mutex.
        let fetcher = L1_STATICCALL_RPC_FETCHER
            .lock()
            .expect("L1_STATICCALL_RPC_FETCHER mutex poisoned")
            .as_ref()
            .map(Arc::clone);
        if let Some(fetcher) = fetcher {
            debug!(
                "L1STATICCALL: RPC fallback target={target:?} block={requested_block} calldata_len={}",
                calldata.len()
            );
            let effective_gas_limit = gas_limit.min(L1STATICCALL_GAS_CAP);
            let (fetched_gas, fetched_data, is_reverted) =
                match fetcher(target, requested_block, effective_gas_limit, calldata) {
                    Ok(v) => v,
                    Err(e) => {
                        return Ok(PrecompileOutput::halt(
                            PrecompileHalt::other(format!("L1 RPC error: {e}")),
                            reservoir,
                        ));
                    }
                };

            debug!(
                "L1STATICCALL: RPC returned target={target:?} block={requested_block} gas={fetched_gas} return_len={} reverted={is_reverted}",
                fetched_data.len()
            );

            // Sanitize to NMC's `GethLikeTxTracer.MarkAsFailed` contract: any reverted call has
            // `gas == 0 && data.is_empty()`. Mainline NMC already enforces this on its side, but
            // a misbehaving fetcher (or a different L1 EL that doesn't follow the contract)
            // shouldn't be able to poison the cache with inconsistent revert entries — the guest
            // verifier rejects them outright, so we'd fail proving later. Sanitize here so the
            // live path and the prover always see the same shape.
            let (l1_gas, fetched_data) = if is_reverted {
                (0u64, Vec::new())
            } else {
                (fetched_gas.min(effective_gas_limit), fetched_data)
            };

            if fetched_data.len() > MAX_RETURN_DATA_SIZE {
                warn!(
                    "L1STATICCALL: return data too large ({} bytes, max {})",
                    fetched_data.len(),
                    MAX_RETURN_DATA_SIZE
                );
                return Ok(PrecompileOutput::halt(
                    PrecompileHalt::other(format!(
                        "L1STATICCALL return data too large: {} > {} bytes",
                        fetched_data.len(),
                        MAX_RETURN_DATA_SIZE
                    )),
                    reservoir,
                ));
            }

            set_l1_staticcall_value(
                target,
                requested_block,
                calldata,
                l1_gas,
                fetched_data.clone(),
                is_reverted,
            );
            if should_record_l1_served_calls() {
                L1_STATICCALL_RPC_SERVED_CALLS
                    .lock()
                    .expect("L1_STATICCALL_RPC_SERVED_CALLS mutex poisoned")
                    .push(L1StaticCallRecord {
                        target,
                        block_number: requested_block,
                        calldata: calldata.to_vec(),
                        return_data: fetched_data.clone(),
                        gas_used: l1_gas,
                        is_reverted,
                    });
            }

            (l1_gas, fetched_data, is_reverted)
        } else {
            warn!(
                "L1STATICCALL: cache miss + no RPC — target={target:?} block={requested_block} calldata_len={}",
                calldata.len()
            );
            return Ok(PrecompileOutput::halt(
                PrecompileHalt::other("L1STATICCALL result not found in cache"),
                reservoir,
            ));
        }
    };

    if is_reverted {
        // NMC parity: reverted `debug_traceCall` invocations carry `gas=0` + empty return data
        // (`GethLikeTxTracer.MarkAsFailed`), matching the guest-side revert assertion. A halt
        // can't carry post-call gas, so reverted L1 calls still diverge from NMC's
        // "charge gas on failure" path — a known limitation.
        debug!("L1STATICCALL: reverted target={target:?} block={requested_block} — surfacing halt");
        return Ok(PrecompileOutput::halt(PrecompileHalt::other("L1 call reverted"), reservoir));
    }

    if result.len() > MAX_RETURN_DATA_SIZE {
        warn!(
            "L1STATICCALL: cached return data too large ({} bytes, max {})",
            result.len(),
            MAX_RETURN_DATA_SIZE
        );
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::other(format!(
                "L1STATICCALL return data too large: {} > {} bytes",
                result.len(),
                MAX_RETURN_DATA_SIZE
            )),
            reservoir,
        ));
    }

    // `total_gas` may exceed `gas_limit`; we deliberately do NOT clamp. revm reads the reported
    // `gas_used` and OOG-halts the calling frame if it exceeds the remaining budget — the same
    // error a contract would see on direct execution. Clamping would let an underfunded call
    // appear to succeed. Test: `test_l1staticcall_l1_gas_alone_oog_when_exceeding_limit`.
    let total_gas = static_gas + l1_gas;
    debug!(
        "L1STATICCALL: success target={target:?} block={requested_block} return_len={} l1_gas={l1_gas} total_gas={total_gas}",
        result.len()
    );
    Ok(PrecompileOutput::new(total_gas, Bytes::from(result), reservoir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precompiles::l1sload::{clear_l1_storage, set_l1_origin_block_id};
    use serial_test::serial;

    const TEST_ADDRESS: [u8; 20] = [1u8; 20];

    /// Build input bytes with the given block number and calldata.
    fn create_test_input(block_number: u64, calldata: &[u8]) -> Vec<u8> {
        let mut input = Vec::with_capacity(52 + calldata.len());
        input.extend_from_slice(&TEST_ADDRESS);
        input.extend_from_slice(&U256::from(block_number).to_be_bytes::<32>());
        input.extend_from_slice(calldata);
        input
    }

    /// Minimal 52-byte input (empty calldata).
    fn create_min_input(block_number: u64) -> Vec<u8> {
        create_test_input(block_number, &[])
    }

    /// Gas for a call with `extra_calldata_bytes` bytes beyond the 52-byte header.
    fn expected_gas(extra_calldata_bytes: usize) -> u64 {
        L1STATICCALL_FIXED_GAS +
            L1STATICCALL_PER_CALL_OVERHEAD +
            L1STATICCALL_PER_BYTE_CALLDATA_GAS * (extra_calldata_bytes as u64)
    }

    /// Reset all L1STATICCALL-specific state AND the shared L1 origin context.
    fn reset_all() {
        clear_l1_storage(); // clears origin context + l1sload cache/rpc state
        clear_l1_staticcall_cache();
        clear_l1_staticcall_rpc_fetcher();
        clear_l1_staticcall_rpc_served_calls();
    }

    // ── Input validation ──────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_rejects_short_input() {
        reset_all();
        set_l1_origin_block_id(100);
        let short = vec![0u8; MIN_INPUT_LENGTH - 1];
        assert!(
            matches!(&l1staticcall_run(&short, expected_gas(0), 0), Ok(o) if o.is_halt()),
            "reject < 52 bytes"
        );
    }

    #[test]
    #[serial]
    fn test_l1staticcall_accepts_exact_min_input() {
        reset_all();
        set_l1_origin_block_id(100);
        let input = create_min_input(100);
        assert_eq!(input.len(), 52, "minimum input is exactly 52 bytes");
        set_l1_staticcall_value(Address::from(TEST_ADDRESS), 100, &[], 0, vec![0xAA], false);
        let result = l1staticcall_run(&input, expected_gas(0), 0);
        assert!(result.is_ok(), "52-byte input should be accepted: {:?}", result.err());
    }

    #[test]
    #[serial]
    fn test_l1staticcall_accepts_variable_length_input() {
        reset_all();
        set_l1_origin_block_id(100);
        let target = Address::from(TEST_ADDRESS);
        for extra_len in [1, 4, 32, 100, 256] {
            let calldata = vec![0xBBu8; extra_len];
            set_l1_staticcall_value(target, 100, &calldata, 0, vec![0xCC], false);
            let input = create_test_input(100, &calldata);
            assert_eq!(input.len(), 52 + extra_len);
            let result = l1staticcall_run(&input, expected_gas(extra_len), 0);
            assert!(
                result.is_ok(),
                "{extra_len} extra bytes should be accepted: {:?}",
                result.err()
            );
        }
    }

    // ── Context validation ────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_fails_without_origin() {
        reset_all();
        let result = l1staticcall_run(&create_min_input(100), expected_gas(0), 0);
        assert!(matches!(&result, Ok(o) if o.is_halt()), "should fail without origin");
        let msg = format!("{:?}", result.unwrap().halt_reason().expect("halt"));
        assert!(msg.contains("L1STATICCALL context unset"), "got: {msg}");
    }

    // ── Cache miss without RPC ────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_fails_without_cached_value() {
        reset_all();
        set_l1_origin_block_id(100);
        let result = l1staticcall_run(&create_min_input(100), expected_gas(0), 0);
        assert!(matches!(&result, Ok(o) if o.is_halt()), "no cache + no RPC");
    }

    // ── Cache hit ─────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_succeeds_with_cached_value() {
        reset_all();
        set_l1_origin_block_id(100);
        let target = Address::from(TEST_ADDRESS);
        let calldata = vec![0x01, 0x02, 0x03, 0x04];
        let return_data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        set_l1_staticcall_value(target, 100, &calldata, 0, return_data.clone(), false);
        let output =
            l1staticcall_run(&create_test_input(100, &calldata), expected_gas(calldata.len()), 0)
                .unwrap();
        assert_eq!(output.bytes.as_ref(), &return_data);
        assert_eq!(output.gas_used, expected_gas(calldata.len()));
    }

    // ── Gas calculation ───────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_gas_calculation_varies_by_calldata_length() {
        reset_all();
        set_l1_origin_block_id(100);
        let target = Address::from(TEST_ADDRESS);
        assert_eq!(expected_gas(0), 2000 + 10000);
        assert_eq!(expected_gas(10), 2000 + 10000 + 16 * 10);
        assert_eq!(expected_gas(100), 2000 + 10000 + 16 * 100);
        let calldata = vec![0xAA; 100];
        set_l1_staticcall_value(target, 100, &calldata, 0, vec![0x01], false);
        let output =
            l1staticcall_run(&create_test_input(100, &calldata), expected_gas(100), 0).unwrap();
        assert_eq!(output.gas_used, expected_gas(100), "static gas with 0 L1 gas");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_fails_with_insufficient_gas() {
        reset_all();
        set_l1_origin_block_id(100);
        let input = create_min_input(100);
        let min_gas = expected_gas(0);
        assert!(
            matches!(&l1staticcall_run(&input, min_gas - 1, 0), Ok(o) if o.is_halt()),
            "one below"
        );
        assert!(matches!(&l1staticcall_run(&input, 0, 0), Ok(o) if o.is_halt()), "zero gas");
    }

    // ── Block range checks ────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_rejects_block_after_origin() {
        reset_all();
        set_l1_origin_block_id(110);
        let result = l1staticcall_run(&create_min_input(111), expected_gas(0), 0);
        assert!(matches!(&result, Ok(o) if o.is_halt()), "block after origin");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_accepts_block_within_window() {
        reset_all();
        set_l1_origin_block_id(120);
        let target = Address::from(TEST_ADDRESS);
        let calldata = vec![0x11];
        let return_data = vec![0x22, 0x33];
        set_l1_staticcall_value(target, 115, &calldata, 0, return_data.clone(), false);
        let result =
            l1staticcall_run(&create_test_input(115, &calldata), expected_gas(calldata.len()), 0);
        assert!(result.is_ok(), "block within window should succeed");
        assert_eq!(result.unwrap().bytes.as_ref(), &return_data);
    }

    #[test]
    #[serial]
    fn test_l1staticcall_rejects_block_beyond_lookback() {
        reset_all();
        set_l1_origin_block_id(1000);
        // origin - 257 = 743, distance 257 > 256.
        let input = create_min_input(1000 - L1STATICCALL_MAX_BLOCK_LOOKBACK - 1);
        assert!(
            matches!(&l1staticcall_run(&input, expected_gas(0), 0), Ok(o) if o.is_halt()),
            "beyond lookback"
        );
    }

    #[test]
    #[serial]
    fn test_l1staticcall_exact_lookback_boundary() {
        reset_all();
        set_l1_origin_block_id(1000);
        let block = 1000 - L1STATICCALL_MAX_BLOCK_LOOKBACK; // 744
        set_l1_staticcall_value(Address::from(TEST_ADDRESS), block, &[], 0, vec![0xFF], false);
        let result = l1staticcall_run(&create_min_input(block), expected_gas(0), 0);
        assert!(result.is_ok(), "block at exact lookback boundary should succeed");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_exact_origin() {
        reset_all();
        set_l1_origin_block_id(200);
        let target = Address::from(TEST_ADDRESS);
        let return_data = vec![0x42];
        set_l1_staticcall_value(target, 200, &[], 0, return_data.clone(), false);
        let result = l1staticcall_run(&create_min_input(200), expected_gas(0), 0);
        assert!(result.is_ok(), "block at exact origin should succeed");
        assert_eq!(result.unwrap().bytes.as_ref(), &return_data);
    }

    // ── Cache key uses calldata hash ──────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_cache_key_includes_calldata_hash() {
        reset_all();
        set_l1_origin_block_id(100);
        let target = Address::from(TEST_ADDRESS);
        let (cd_a, cd_b) = (vec![0xAA; 4], vec![0xBB; 4]);
        let (ret_a, ret_b) = (vec![0x11], vec![0x22]);
        set_l1_staticcall_value(target, 100, &cd_a, 0, ret_a.clone(), false);
        set_l1_staticcall_value(target, 100, &cd_b, 0, ret_b.clone(), false);
        assert_eq!(
            l1staticcall_run(&create_test_input(100, &cd_a), expected_gas(4), 0)
                .unwrap()
                .bytes
                .as_ref(),
            &ret_a
        );
        assert_eq!(
            l1staticcall_run(&create_test_input(100, &cd_b), expected_gas(4), 0)
                .unwrap()
                .bytes
                .as_ref(),
            &ret_b
        );
    }

    // ── Variable-length return data ───────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_variable_length_return_data() {
        reset_all();
        set_l1_origin_block_id(100);
        let target = Address::from(TEST_ADDRESS);

        set_l1_staticcall_value(target, 100, &[0x01], 0, vec![], false);
        assert!(
            l1staticcall_run(&create_test_input(100, &[0x01]), expected_gas(1), 0)
                .unwrap()
                .bytes
                .is_empty(),
            "empty allowed"
        );

        set_l1_staticcall_value(target, 100, &[0x02], 0, vec![0xFF], false);
        assert_eq!(
            l1staticcall_run(&create_test_input(100, &[0x02]), expected_gas(1), 0)
                .unwrap()
                .bytes
                .as_ref(),
            &[0xFF]
        );

        let big_return = vec![0xAB; 1024];
        set_l1_staticcall_value(target, 100, &[0x03], 0, big_return.clone(), false);
        let output =
            l1staticcall_run(&create_test_input(100, &[0x03]), expected_gas(1), 0).unwrap();
        assert_eq!(output.bytes.len(), 1024);
        assert_eq!(output.bytes.as_ref(), &big_return);
    }

    // ── RPC fallback ──────────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_rpc_fallback_records_served_calls() {
        reset_all();
        set_l1_origin_block_id(100);
        let target = Address::from(TEST_ADDRESS);
        let calldata = vec![0xCA, 0xFE];
        let return_data = vec![0xDE, 0xAD];
        let (et, ecd, er) = (target, calldata.clone(), return_data.clone());
        set_l1_staticcall_rpc_fetcher(move |t, bn, _gl, cd| {
            assert_eq!(t, et);
            assert_eq!(bn, 100);
            assert_eq!(cd, ecd.as_slice());
            Ok((0, er.clone(), false))
        });

        let result =
            l1staticcall_run(&create_test_input(100, &calldata), expected_gas(calldata.len()), 0);
        assert!(result.is_ok(), "RPC fallback should succeed: {:?}", result.err());
        assert_eq!(result.unwrap().bytes.as_ref(), &return_data);

        let served = take_l1_staticcall_rpc_served_calls();
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].target, target);
        assert_eq!(served[0].block_number, 100);
        assert_eq!(served[0].calldata, calldata);
        assert_eq!(served[0].return_data, return_data);
        assert_eq!(served[0].gas_used, 0);
        assert!(!served[0].is_reverted);

        // Second call served from cache even without the fetcher.
        clear_l1_staticcall_rpc_fetcher();
        let result2 =
            l1staticcall_run(&create_test_input(100, &calldata), expected_gas(calldata.len()), 0);
        assert!(result2.is_ok(), "cache hit should succeed after fetcher cleared");
        assert_eq!(result2.unwrap().bytes.as_ref(), &return_data);
    }

    #[test]
    #[serial]
    fn test_l1staticcall_rpc_fallback_error_propagates() {
        reset_all();
        set_l1_origin_block_id(100);
        set_l1_staticcall_rpc_fetcher(|_, _, _, _| Err("L1 node unavailable".to_string()));
        let result = l1staticcall_run(&create_min_input(100), expected_gas(0), 0);
        assert!(matches!(&result, Ok(o) if o.is_halt()), "RPC error should propagate");
        let msg = format!("{:?}", result.unwrap().halt_reason().expect("halt"));
        assert!(msg.contains("L1 node unavailable"), "got: {msg}");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_multiple_rpc_calls_tracked() {
        reset_all();
        set_l1_origin_block_id(110);
        set_l1_staticcall_rpc_fetcher(|_, bn, _, cd| {
            let mut ret = vec![0u8; 4];
            ret[0] = bn as u8;
            ret[1] = cd.first().copied().unwrap_or(0);
            Ok((0, ret, false))
        });
        let _ = l1staticcall_run(&create_test_input(105, &[0x11]), expected_gas(1), 0).unwrap();
        let _ = l1staticcall_run(&create_test_input(110, &[0x22]), expected_gas(1), 0).unwrap();
        let served = take_l1_staticcall_rpc_served_calls();
        assert_eq!(served.len(), 2, "both served calls tracked");
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
        set_l1_origin_block_id(100);
        let target = Address::from(TEST_ADDRESS);
        set_l1_staticcall_value(target, 100, &[0x01], 0, vec![0xFF], false);
        let input = create_test_input(100, &[0x01]);
        assert!(l1staticcall_run(&input, expected_gas(1), 0).is_ok(), "find cached value");

        clear_l1_staticcall_cache();
        assert!(
            matches!(&l1staticcall_run(&input, expected_gas(1), 0), Ok(o) if o.is_halt()),
            "fail after cache clear"
        );

        // The origin context survives a cache clear.
        assert!(get_l1_origin_block_id().is_some(), "origin should survive cache clear");

        set_l1_staticcall_rpc_fetcher(|_, _, _, _| Ok((0, vec![0x99], false)));
        let _ = l1staticcall_run(&input, expected_gas(1), 0).unwrap();
        assert_eq!(take_l1_staticcall_rpc_served_calls().len(), 1);
        assert!(take_l1_staticcall_rpc_served_calls().is_empty(), "empty after take");

        set_l1_staticcall_rpc_fetcher(|_, _, _, _| Ok((0, vec![0x88], false)));
        clear_l1_staticcall_cache(); // force cache miss
        let _ = l1staticcall_run(&input, expected_gas(1), 0).unwrap();
        clear_l1_staticcall_rpc_served_calls();
        assert!(
            take_l1_staticcall_rpc_served_calls().is_empty(),
            "explicit clear empties served calls"
        );
    }

    // ── Edge cases ────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_max_return_data_size() {
        reset_all();
        set_l1_origin_block_id(100);
        let target = Address::from(TEST_ADDRESS);
        let input = create_test_input(100, &[0x01]);

        let exact_max = vec![0xAA; MAX_RETURN_DATA_SIZE];
        set_l1_staticcall_value(target, 100, &[0x01], 0, exact_max.clone(), false);
        let result = l1staticcall_run(&input, expected_gas(1), 0);
        assert!(result.is_ok(), "exactly MAX_RETURN_DATA_SIZE should succeed");
        assert_eq!(result.unwrap().bytes.len(), MAX_RETURN_DATA_SIZE);

        // One byte over max via RPC — should fail.
        clear_l1_staticcall_cache();
        let over_max = vec![0xBB; MAX_RETURN_DATA_SIZE + 1];
        set_l1_staticcall_rpc_fetcher(move |_, _, _, _| Ok((0, over_max.clone(), false)));
        let result = l1staticcall_run(&input, expected_gas(1), 0);
        assert!(matches!(&result, Ok(o) if o.is_halt()), "over MAX_RETURN_DATA_SIZE should fail");
        let msg = format!("{:?}", result.unwrap().halt_reason().expect("halt"));
        assert!(msg.contains("return data too large"), "got: {msg}");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_zero_block_number() {
        reset_all();
        set_l1_origin_block_id(100);
        set_l1_staticcall_value(Address::from(TEST_ADDRESS), 0, &[], 0, vec![0x00], false);
        let result = l1staticcall_run(&create_min_input(0), expected_gas(0), 0);
        assert!(result.is_ok(), "block 0 within lookback should succeed");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_same_target_different_calldata() {
        reset_all();
        set_l1_origin_block_id(100);
        let target = Address::from(TEST_ADDRESS);
        let (cd1, cd2) = (vec![0x01, 0x02], vec![0x03, 0x04]);
        let (ret1, ret2) = (vec![0xAA, 0xBB], vec![0xCC, 0xDD]);
        set_l1_staticcall_value(target, 100, &cd1, 0, ret1.clone(), false);
        set_l1_staticcall_value(target, 100, &cd2, 0, ret2.clone(), false);
        let r1 = l1staticcall_run(&create_test_input(100, &cd1), expected_gas(2), 0).unwrap();
        let r2 = l1staticcall_run(&create_test_input(100, &cd2), expected_gas(2), 0).unwrap();
        assert_eq!(r1.bytes.as_ref(), &ret1);
        assert_eq!(r2.bytes.as_ref(), &ret2);
        assert_ne!(r1.bytes, r2.bytes, "different calldata → different values");
    }

    // ── Dynamic gas ───────────────────────────────────────────────────

    #[test]
    #[serial]
    fn test_l1staticcall_charges_dynamic_gas_from_cache() {
        reset_all();
        set_l1_origin_block_id(100);
        let l1_gas = 50_000u64;
        set_l1_staticcall_value(Address::from(TEST_ADDRESS), 100, &[], l1_gas, vec![0x42], false);
        let output = l1staticcall_run(&create_min_input(100), 1_000_000, 0).unwrap();
        assert_eq!(output.gas_used, expected_gas(0) + l1_gas, "total = static + cached L1 gas");
    }

    #[test]
    #[serial]
    fn test_l1staticcall_charges_dynamic_gas_from_rpc() {
        reset_all();
        set_l1_origin_block_id(100);
        let l1_gas = 50_000u64;
        let return_data = vec![0xBE, 0xEF];
        let rd = return_data.clone();
        set_l1_staticcall_rpc_fetcher(move |_, _, _, _| Ok((l1_gas, rd.clone(), false)));
        let output = l1staticcall_run(&create_min_input(100), 1_000_000, 0).unwrap();
        assert_eq!(output.bytes.as_ref(), &return_data);
        assert_eq!(output.gas_used, expected_gas(0) + l1_gas, "total = static + fetched L1 gas");
        let served = take_l1_staticcall_rpc_served_calls();
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].gas_used, l1_gas, "record carries L1 gas_used");
        assert!(!served[0].is_reverted);
    }

    #[test]
    #[serial]
    fn test_l1staticcall_gas_within_limit_succeeds() {
        reset_all();
        set_l1_origin_block_id(100);
        let l1_gas = 25_000u64;
        let return_data = vec![0x12, 0x34];
        let rd = return_data.clone();
        set_l1_staticcall_rpc_fetcher(move |_, _, _, _| Ok((l1_gas, rd.clone(), false)));
        let gas_limit = expected_gas(0) + l1_gas;
        let output = l1staticcall_run(&create_min_input(100), gas_limit, 0).unwrap();
        assert_eq!(output.bytes.as_ref(), &return_data);
        assert_eq!(output.gas_used, gas_limit);
    }

    /// `gas_used > gas_limit` is expected: the precompile reports `static + clamped_l1_gas` and
    /// revm applies the final OOG check.
    #[test]
    #[serial]
    fn test_l1staticcall_l1_gas_clamped_at_gas_cap() {
        reset_all();
        set_l1_origin_block_id(100);
        set_l1_staticcall_rpc_fetcher(move |_, _, _gl, _| {
            Ok((1_000_000_000u64, vec![0x01], false))
        });
        let gas_limit = 100_000u64;
        let output = l1staticcall_run(&create_min_input(100), gas_limit, 0).unwrap();
        let effective_cap = gas_limit.min(L1STATICCALL_GAS_CAP);
        assert_eq!(
            output.gas_used,
            expected_gas(0) + effective_cap,
            "L1 gas clamped to min(gas_limit, 30M)"
        );
    }

    #[test]
    #[serial]
    fn test_l1staticcall_reverted_result_returns_error() {
        reset_all();
        set_l1_origin_block_id(100);
        // NMC tracer contract: reverted calls report gas=0 and empty data. The precompile
        // sanitizes regardless of fetcher input, but matching the contract here keeps the
        // fixture honest.
        set_l1_staticcall_rpc_fetcher(|_, _, _, _| Ok((0, vec![], true)));
        let result = l1staticcall_run(&create_min_input(100), 1_000_000, 0);
        assert!(matches!(&result, Ok(o) if o.is_halt()), "reverted L1 call should halt");
        let msg = format!("{:?}", result.unwrap().halt_reason().expect("halt"));
        assert!(msg.contains("L1 call reverted"), "got: {msg}");
        let served = take_l1_staticcall_rpc_served_calls();
        assert_eq!(served.len(), 1);
        assert!(served[0].is_reverted);
        assert_eq!(served[0].gas_used, 0, "reverted served-call must carry gas=0");
        assert!(served[0].return_data.is_empty(), "reverted served-call must carry empty data");
    }

    // ── Concurrency + panic-safety ────────────────────────────────────

    /// Two threads hitting the precompile while the fetcher sleeps must not deadlock on the
    /// global mutex — the fetcher `Arc` is cloned out of the lock before invocation.
    #[test]
    #[serial]
    fn test_l1staticcall_concurrent_invocation_no_deadlock() {
        use std::{
            sync::{Arc as StdArc, Barrier},
            thread,
            time::Duration,
        };

        reset_all();
        set_l1_origin_block_id(100);
        set_l1_staticcall_rpc_fetcher(|_, _, _, _| {
            thread::sleep(Duration::from_millis(50));
            Ok((10_000, vec![0xAA], false))
        });

        let barrier = StdArc::new(Barrier::new(2));
        let (b1, b2) = (StdArc::clone(&barrier), StdArc::clone(&barrier));
        let h1 = thread::spawn(move || {
            b1.wait();
            l1staticcall_run(&create_test_input(100, &[0x11]), 1_000_000, 0)
                .map(|o| o.bytes.to_vec())
        });
        let h2 = thread::spawn(move || {
            b2.wait();
            l1staticcall_run(&create_test_input(100, &[0x22]), 1_000_000, 0)
                .map(|o| o.bytes.to_vec())
        });
        let r1 = h1.join().expect("thread 1 panicked");
        let r2 = h2.join().expect("thread 2 panicked");
        assert!(r1.is_ok(), "thread 1 did not complete: {:?}", r1.err());
        assert!(r2.is_ok(), "thread 2 did not complete: {:?}", r2.err());
    }

    /// A panicking fetcher must not poison the global mutex.
    #[test]
    #[serial]
    fn test_l1staticcall_fetcher_panic_does_not_poison_mutex() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        reset_all();
        set_l1_origin_block_id(100);
        set_l1_staticcall_rpc_fetcher(|_, _, _, _| {
            panic!("fetcher panic (test): L1 node exploded")
        });

        let input = create_min_input(100);
        let first = catch_unwind(AssertUnwindSafe(|| l1staticcall_run(&input, 1_000_000, 0)));
        assert!(first.is_err(), "first call must propagate the fetcher panic");

        // A well-behaved fetcher must now succeed, proving the mutex was not poisoned.
        clear_l1_staticcall_cache();
        set_l1_staticcall_rpc_fetcher(|_, _, _, _| Ok((0, vec![0xBB], false)));
        let out = l1staticcall_run(&input, 1_000_000, 0).expect("recovered fetcher should succeed");
        assert_eq!(out.bytes.as_ref(), &[0xBB]);
    }

    // ── Setter/gas regression guards ──────────────────────────────────

    /// Oversized return data is dropped at the setter rather than poisoning the cache.
    #[test]
    #[serial]
    fn test_set_l1_staticcall_value_drops_oversized_return_data() {
        reset_all();
        let target = Address::from([0x1Au8; 20]);
        let calldata = vec![0xAA, 0xBB];

        set_l1_staticcall_value(
            target,
            100,
            &calldata,
            0,
            vec![0xCDu8; MAX_RETURN_DATA_SIZE + 1],
            false,
        );
        assert!(
            get_l1_staticcall_value(target, 100, &calldata).is_none(),
            "oversized must NOT enter cache"
        );

        let at_cap = vec![0xEFu8; MAX_RETURN_DATA_SIZE];
        set_l1_staticcall_value(target, 200, &calldata, 0, at_cap, false);
        let stored =
            get_l1_staticcall_value(target, 200, &calldata).expect("at-cap must be stored");
        assert_eq!(stored.0, 0);
        assert_eq!(stored.1.len(), MAX_RETURN_DATA_SIZE);
        assert!(!stored.2);
    }

    /// When `l1_gas` alone exceeds the remaining budget, the precompile still reports its true
    /// `total_gas` (revm OOG-halts the caller) rather than clamping to `gas_limit`.
    #[test]
    #[serial]
    fn test_l1staticcall_l1_gas_alone_oog_when_exceeding_limit() {
        reset_all();
        set_l1_origin_block_id(100);
        let target = Address::from([0xC0u8; 20]);
        let calldata = vec![0x01];
        set_l1_staticcall_value(target, 100, &calldata, 10_000_000, vec![0xAA], false);

        let mut input = Vec::with_capacity(53);
        input.extend_from_slice(target.as_slice());
        input.extend_from_slice(&U256::from(100u64).to_be_bytes::<32>());
        input.extend_from_slice(&calldata);

        let limit: u64 = 100_000;
        let result = l1staticcall_run(&input, limit, 0).expect("precompile returns reported gas");
        assert!(
            result.gas_used > limit,
            "must report true total_gas {} > limit {limit}",
            result.gas_used
        );
        assert_eq!(result.bytes.as_ref(), &[0xAA]);
    }
}
