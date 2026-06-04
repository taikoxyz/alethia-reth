//! Shared L1 precompile context: the `l1_origin_block_id` that bounds the trusted
//! `[origin − 256, origin]` lookback window for both [`super::l1sload`] and
//! [`super::l1staticcall`].
//!
//! `origin` is `Proposal.originBlockNumber` — the L1 tip the Shasta proposal committed to,
//! whose hash is the on-chain trust root `originBlockHash = blockhash(originBlockNumber)`.
//! The host (raiko / a re-executing RPC) and the live block-import path each set it once
//! per block, before any L1 precompile call. Single-threaded contract: drive these
//! getters/setters with one value at a time.

use std::{
    cell::Cell,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Maximum L1-block lookback enforced by both L1Sload and L1Staticcall. Matches the EVM
/// `BLOCKHASH` opcode's 256-block window: any block older than `origin − 256` is rejected
/// before the precompile attempts a cache lookup or RPC fetch.
pub const L1_PRECOMPILE_MAX_LOOKBACK: u64 = 256;

/// Outcome of [`validate_l1_block_in_window`]. The discriminants double as halt-reason
/// strings for the precompile entry points, so the variant names track exactly the
/// messages users see in halt outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowError {
    /// Requested block is strictly greater than the L1 origin block.
    BlockAfterOrigin,
    /// Requested block is older than `origin − [`L1_PRECOMPILE_MAX_LOOKBACK`]`.
    BlockBeyondLookback,
}

/// Validate `block ∈ [origin − L1_PRECOMPILE_MAX_LOOKBACK, origin]`. Single source of truth
/// shared by both precompiles' live paths and the guest verifier so a future change to the
/// constant or the saturating semantics lands in one place.
pub fn validate_l1_block_in_window(block: u64, origin: u64) -> Result<(), WindowError> {
    if block > origin {
        return Err(WindowError::BlockAfterOrigin);
    }
    if origin - block > L1_PRECOMPILE_MAX_LOOKBACK {
        return Err(WindowError::BlockBeyondLookback);
    }
    Ok(())
}

/// Current L1 origin block number — the upper bound of the `[origin − 256, origin]`
/// lookback window.
static CURRENT_L1_ORIGIN_BLOCK_ID: LazyLock<Mutex<Option<u64>>> =
    LazyLock::new(|| Mutex::new(None));

/// Set the L1 origin block ID for the block about to execute.
///
/// **Caller contract**: must be invoked from a serialized execution context — either inside
/// the block executor's `apply_pre_execution_changes` (single-threaded per block import / build)
/// or under the prover's `L1_PRECOMPILE_EXECUTION_LOCK`. Concurrent callers will race on the
/// process-global and the precompile may observe a value from a different block, silently
/// widening or narrowing its lookback window.
pub fn set_l1_origin_block_id(origin_block_id: u64) {
    *CURRENT_L1_ORIGIN_BLOCK_ID.lock().expect("CURRENT_L1_ORIGIN_BLOCK_ID mutex poisoned") =
        Some(origin_block_id);
}

/// Read the current L1 origin block ID.
pub fn get_l1_origin_block_id() -> Option<u64> {
    *CURRENT_L1_ORIGIN_BLOCK_ID.lock().expect("CURRENT_L1_ORIGIN_BLOCK_ID mutex poisoned")
}

/// Clear the L1 origin context — used by `clear_l1_storage`.
pub fn clear_l1_origin_context() {
    *CURRENT_L1_ORIGIN_BLOCK_ID.lock().expect("CURRENT_L1_ORIGIN_BLOCK_ID mutex poisoned") = None;
}

/// Whether the precompiles record RPC-served calls into the served-call lists. Defaults to `true`
/// (the prover preflight needs the records to fetch proofs). The live node binary turns it off when
/// it installs live fetchers, so a sequencer / follower never accumulates served-call records.
static RECORD_L1_SERVED_CALLS: AtomicBool = AtomicBool::new(true);

/// Enable or disable recording of RPC-served L1 calls.
pub fn set_record_l1_served_calls(enabled: bool) {
    RECORD_L1_SERVED_CALLS.store(enabled, Ordering::Relaxed);
}

/// Whether RPC-served L1 calls should be recorded (see [`set_record_l1_served_calls`]).
pub fn should_record_l1_served_calls() -> bool {
    RECORD_L1_SERVED_CALLS.load(Ordering::Relaxed)
}

thread_local! {
    /// Per-thread origin override for re-execution RPC handlers (`debug_executionWitness`,
    /// `proof_call`) that look up `StoredL1OriginTable` from the db and inject the result
    /// here before invoking the executor — whose `TaikoEvmConfig` has no db handle. The
    /// executor hook prefers this over the ctx field. Per-thread so concurrent handlers
    /// don't interfere.
    static L1_ORIGIN_OVERRIDE: Cell<Option<u64>> = const { Cell::new(None) };
}

/// RAII guard that restores the previous [`L1_ORIGIN_OVERRIDE`] on drop (supports nesting).
#[must_use = "bind the guard to a variable that outlives the executor invocation"]
pub struct L1OriginOverride {
    prev: Option<u64>,
}

impl L1OriginOverride {
    /// Install `origin` as the current thread's override; restored when the returned guard drops.
    pub fn install(origin: u64) -> Self {
        let prev = L1_ORIGIN_OVERRIDE.with(|cell| cell.replace(Some(origin)));
        Self { prev }
    }
}

impl Drop for L1OriginOverride {
    fn drop(&mut self) {
        L1_ORIGIN_OVERRIDE.with(|cell| cell.set(self.prev));
    }
}

/// Read the current thread's origin override, if any.
pub fn current_l1_origin_override() -> Option<u64> {
    L1_ORIGIN_OVERRIDE.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn set_get_clear_origin() {
        clear_l1_origin_context();
        assert_eq!(get_l1_origin_block_id(), None);
        set_l1_origin_block_id(1000);
        assert_eq!(get_l1_origin_block_id(), Some(1000));
        clear_l1_origin_context();
        assert_eq!(get_l1_origin_block_id(), None);
    }

    #[test]
    fn override_install_read_and_nested_restore() {
        assert_eq!(current_l1_origin_override(), None);
        let outer = L1OriginOverride::install(1000);
        assert_eq!(current_l1_origin_override(), Some(1000));
        {
            let _inner = L1OriginOverride::install(2000);
            assert_eq!(current_l1_origin_override(), Some(2000));
        }
        // Inner dropped → outer's value restored, not None.
        assert_eq!(current_l1_origin_override(), Some(1000));
        drop(outer);
        assert_eq!(current_l1_origin_override(), None);
    }

    // T14: guard must clear the thread-local even on panic-unwind. Defends against a future
    // refactor that drops the Drop impl or moves the restore out of it.
    #[test]
    fn override_guard_clears_on_panic_unwind() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        assert_eq!(current_l1_origin_override(), None);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = L1OriginOverride::install(4242);
            assert_eq!(current_l1_origin_override(), Some(4242));
            panic!("simulated panic while override is installed");
        }));
        assert!(result.is_err(), "panic must propagate to catch_unwind");
        assert_eq!(
            current_l1_origin_override(),
            None,
            "override must be cleared via Drop on panic unwind"
        );
    }

    #[test]
    fn validate_l1_block_in_window_accepts_inside_window() {
        assert!(validate_l1_block_in_window(1000, 1000).is_ok(), "block == origin");
        assert!(validate_l1_block_in_window(744, 1000).is_ok(), "exact lookback boundary (origin - 256)");
        assert!(validate_l1_block_in_window(900, 1000).is_ok(), "well inside the window");
    }

    #[test]
    fn validate_l1_block_in_window_rejects_after_origin() {
        assert_eq!(
            validate_l1_block_in_window(1001, 1000),
            Err(WindowError::BlockAfterOrigin),
        );
    }

    #[test]
    fn validate_l1_block_in_window_rejects_beyond_lookback() {
        assert_eq!(
            validate_l1_block_in_window(743, 1000), // distance 257 > 256
            Err(WindowError::BlockBeyondLookback),
        );
    }

    #[test]
    fn validate_l1_block_in_window_saturates_when_origin_is_small() {
        // origin == 0 → only block 0 is in window; everything else is BlockAfterOrigin.
        assert!(validate_l1_block_in_window(0, 0).is_ok());
        assert_eq!(validate_l1_block_in_window(1, 0), Err(WindowError::BlockAfterOrigin));
    }
}
