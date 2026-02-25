/// L1SLOAD precompile implementation (RIP-7728).
pub mod l1sload;

use reth_evm::precompiles::{DynPrecompile, PrecompilesMap};
use reth_revm::precompile::{PrecompileSpecId, Precompiles};

/// Builds a [`PrecompilesMap`] that includes the standard Ethereum precompiles
/// plus the Taiko L1SLOAD precompile at address `0x10001`.
pub fn taiko_precompiles_map(spec_id: PrecompileSpecId) -> PrecompilesMap {
    let mut map = PrecompilesMap::from_static(Precompiles::new(spec_id));
    let l1sload_addr = *l1sload::L1SLOAD.address();
    let l1sload_dyn: DynPrecompile = (l1sload::l1sload_run as fn(&[u8], u64) -> _).into();
    map.extend_precompiles([(l1sload_addr, l1sload_dyn)]);
    map
}
