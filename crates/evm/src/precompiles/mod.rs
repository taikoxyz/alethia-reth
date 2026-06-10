/// Shared L1 origin context for the L1 precompiles.
pub mod context;
/// L1SLOAD precompile implementation (RIP-7728).
pub mod l1sload;
/// L1STATICCALL precompile implementation.
pub mod l1staticcall;

// Deferred refactors documented in `notes/2026/05-21-l1sload-l1staticcall-upstreaming/code-review-2026-06-04.md`:
//
//  * D2 — thread-local-only L1 origin (drop `TaikoBlockExecutionCtx.l1_origin_block_number`).
//    Rejected per session-log-2026-06-01.md §5: thread-locals don't survive `tokio::spawn`,
//    so the build path would lose the override across runtime boundaries. Revisit only if
//    the payload builder is proven single-thread end-to-end.
//
//  * D4 — `PrecompileGlobals<K, V>` generic to consolidate the per-precompile global
//    skeleton (cache + fetcher + served-calls). ~250 LoC dedup but invasive; defer until
//    post-devnet so the structural change doesn't churn against in-flight ZK fixtures.

use reth_evm::precompiles::{DynPrecompile, PrecompilesMap};
use reth_revm::precompile::{PrecompileSpecId, Precompiles, u64_to_address};

use crate::spec::TaikoSpecId;

/// Address of the L1SLOAD precompile (RIP-7728).
pub const L1SLOAD_PRECOMPILE_ADDRESS: u64 = 0x10001;
/// Address of the L1STATICCALL precompile.
pub const L1STATICCALL_PRECOMPILE_ADDRESS: u64 = 0x10002;

/// Builds a [`PrecompilesMap`] that includes the standard Ethereum precompiles plus, where the
/// active Taiko hardfork enables them, the L1SLOAD precompile at `0x10001` and the L1STATICCALL
/// precompile at `0x10002`.
///
/// **Hardfork gating.** Both L1SLOAD (RIP-7728) and L1STATICCALL register from
/// [`TaikoSpecId::UNZEN`] onward. On chains where Unzen is `ForkCondition::Never` (Taiko Mainnet
/// at the time of writing — see `crates/chainspec/src/hardfork.rs::TAIKO_MAINNET_HARDFORKS`),
/// the addresses are left unregistered, so a call to `0x10001` or `0x10002` lands on the
/// standard EVM "no code at address" path instead of returning an internal precompile error.
pub fn taiko_precompiles_map(spec_id: PrecompileSpecId, taiko_spec: TaikoSpecId) -> PrecompilesMap {
    let mut map = PrecompilesMap::from_static(Precompiles::new(spec_id));
    if taiko_spec.is_enabled_in(TaikoSpecId::UNZEN) {
        let l1sload_addr = u64_to_address(L1SLOAD_PRECOMPILE_ADDRESS);
        let l1sload_dyn: DynPrecompile = (l1sload::l1sload_run as fn(&[u8], u64, u64) -> _).into();
        map.extend_precompiles([(l1sload_addr, l1sload_dyn)]);

        let l1staticcall_addr = u64_to_address(L1STATICCALL_PRECOMPILE_ADDRESS);
        let l1staticcall_dyn: DynPrecompile =
            (l1staticcall::l1staticcall_run as fn(&[u8], u64, u64) -> _).into();
        map.extend_precompiles([(l1staticcall_addr, l1staticcall_dyn)]);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    fn precompile_spec_id() -> PrecompileSpecId {
        PrecompileSpecId::from_spec_id(TaikoSpecId::UNZEN.into())
    }

    fn map_contains(map: &PrecompilesMap, addr_u64: u64) -> bool {
        let needle = u64_to_address(addr_u64);
        map.addresses().any(|a: &Address| *a == needle)
    }

    #[test]
    fn pre_unzen_specs_skip_both_precompiles() {
        for spec in
            [TaikoSpecId::GENESIS, TaikoSpecId::ONTAKE, TaikoSpecId::PACAYA, TaikoSpecId::SHASTA]
        {
            let map = taiko_precompiles_map(precompile_spec_id(), spec);
            assert!(
                !map_contains(&map, L1SLOAD_PRECOMPILE_ADDRESS),
                "L1SLOAD must not be registered at {spec:?}"
            );
            assert!(
                !map_contains(&map, L1STATICCALL_PRECOMPILE_ADDRESS),
                "L1STATICCALL must not be registered at {spec:?}"
            );
        }
    }

    #[test]
    fn unzen_registers_both_l1_precompiles() {
        let map = taiko_precompiles_map(precompile_spec_id(), TaikoSpecId::UNZEN);
        assert!(
            map_contains(&map, L1SLOAD_PRECOMPILE_ADDRESS),
            "L1SLOAD must be registered from Unzen onward"
        );
        assert!(
            map_contains(&map, L1STATICCALL_PRECOMPILE_ADDRESS),
            "L1STATICCALL must be registered from Unzen onward"
        );
    }
}
