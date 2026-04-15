/// L1SLOAD precompile implementation (RIP-7728).
pub mod l1sload;
/// L1STATICCALL precompile implementation.
pub mod l1staticcall;

use reth_evm::precompiles::{DynPrecompile, PrecompilesMap};
use reth_revm::precompile::{PrecompileSpecId, Precompiles, u64_to_address};

/// Builds a [`PrecompilesMap`] that includes the standard Ethereum precompiles
/// plus the Taiko L1SLOAD precompile at address `0x10001` and L1STATICCALL at `0x10002`.
pub fn taiko_precompiles_map(spec_id: PrecompileSpecId) -> PrecompilesMap {
    let mut map = PrecompilesMap::from_static(Precompiles::new(spec_id));
    let l1sload_addr = u64_to_address(0x10001);
    let l1sload_dyn: DynPrecompile = (l1sload::l1sload_run as fn(&[u8], u64) -> _).into();
    let l1staticcall_addr = u64_to_address(0x10002);
    let l1staticcall_dyn: DynPrecompile =
        (l1staticcall::l1staticcall_run as fn(&[u8], u64) -> _).into();
    map.extend_precompiles([(l1sload_addr, l1sload_dyn), (l1staticcall_addr, l1staticcall_dyn)]);
    map
}
