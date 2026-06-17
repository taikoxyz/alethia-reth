//! Input-scaled precompile reprice via [`PrecompilesMap`] overrides.
//!
//! Wraps an existing precompile so each call pays an additive surcharge on top of the
//! precompile's own gas. The surcharge is `base + per_word * ceil(len / 32)`, i.e. a fixed
//! component plus a component that scales with the 32-byte word count of the input. This
//! mirrors how the underlying precompiles already meter input (per-word), so proving cost
//! that grows with input size can be charged without re-deriving each precompile's formula.
//!
//! The wrapper preserves the original output bytes and status; it only inflates `gas_used`.
//! Saturating arithmetic guards against overflow on pathological inputs.

use alloy_primitives::Address;
use reth_evm::precompiles::{DynPrecompile, Precompile, PrecompilesMap};

/// Additive, input-scaled gas surcharge for a single precompile.
///
/// Charged as `base + per_word * ceil(len / 32)`. NON-CONSENSUS placeholder values are
/// supplied by tests/calibration; this type only carries them.
#[derive(Clone, Copy, Debug)]
pub struct PrecompileSurcharge {
    /// Fixed gas added to every call regardless of input length.
    pub base: u64,
    /// Gas added per 32-byte input word (ceil), on top of [`base`](Self::base).
    pub per_word: u64,
}

/// Number of 32-byte words spanning `len` bytes, rounding up (`ceil(len / 32)`).
const fn words(len: usize) -> u64 {
    (len as u64).div_ceil(32)
}

/// Wraps the precompile at `address` so each call pays `surcharge` on top of its own gas.
///
/// No-op if no precompile is registered at `address` (`map_precompile` only transforms an
/// existing entry). The wrapper clones the original's `PrecompileId` and forwards every
/// other field of the output unchanged, adjusting only `gas_used` (saturating).
pub fn apply_precompile_surcharge(
    precompiles: &mut PrecompilesMap,
    address: Address,
    surcharge: PrecompileSurcharge,
) {
    precompiles.map_precompile(&address, move |original| {
        let id = original.precompile_id().clone();
        DynPrecompile::new(id, move |input| {
            let extra = surcharge
                .base
                .saturating_add(surcharge.per_word.saturating_mul(words(input.data.len())));
            let mut out = original.call(input)?;
            out.gas_used = out.gas_used.saturating_add(extra);
            Ok(out)
        })
    });
}

#[cfg(test)]
mod test_support {
    use alloy_primitives::Address;
    use reth_evm::precompiles::{Precompile, PrecompilesMap};
    use reth_revm::{
        MainContext,
        context::Context,
        db::EmptyDB,
        precompile::{PrecompileSpecId, Precompiles},
        primitives::hardfork::SpecId,
    };

    /// Builds an Osaka [`PrecompilesMap`], mirroring the precompile-set construction in
    /// `factory.rs`.
    pub(super) fn osaka_precompiles() -> PrecompilesMap {
        PrecompilesMap::from_static(Precompiles::new(PrecompileSpecId::from_spec_id(SpecId::OSAKA)))
    }

    /// Resolves the precompile registered at `addr`.
    ///
    /// The returned value borrows `map` and exposes `.call(PrecompileInput) -> PrecompileResult`
    /// via the [`Precompile`] trait. Drop it before mutating `map` (e.g. before applying a
    /// surcharge), since the borrow is immutable.
    pub(super) fn get_precompile<'a>(
        map: &'a PrecompilesMap,
        addr: &Address,
    ) -> impl Precompile + 'a {
        map.get(addr).expect("precompile registered at address")
    }

    /// A throwaway EVM context whose journal/db back the precompile call's `EvmInternals`.
    ///
    /// `EmptyDB` is sufficient because the surcharge wrapper and the identity precompile never
    /// touch state; the context only satisfies `PrecompileInput`'s required fields.
    pub(super) fn empty_context() -> Context<
        reth_revm::context::BlockEnv,
        reth_revm::context::TxEnv,
        reth_revm::context::CfgEnv,
        EmptyDB,
    > {
        Context::mainnet().with_db(EmptyDB::default())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        test_support::{empty_context, get_precompile, osaka_precompiles},
        *,
    };
    use alloy_primitives::{Address, Bytes, U256};
    use reth_evm::{
        EvmInternals,
        precompiles::{Precompile, PrecompileInput},
    };

    /// Identity precompile address (`0x..04`).
    const IDENTITY: Address = Address::with_last_byte(0x04);

    /// Calls `precompile` with `input_data` and `gas`, supplying the remaining
    /// `PrecompileInput` fields from a throwaway context. Returns `gas_used` on success.
    fn call_gas_used(precompile: &impl Precompile, input_data: &[u8], gas: u64) -> u64 {
        let mut ctx = empty_context();
        let data = Bytes::copy_from_slice(input_data);
        precompile
            .call(PrecompileInput {
                data: &data,
                gas,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                is_static: false,
                internals: EvmInternals::from_context(&mut ctx),
                target_address: IDENTITY,
                bytecode_address: IDENTITY,
            })
            .expect("identity precompile call succeeds")
            .gas_used
    }

    #[test]
    fn words_rounds_up() {
        assert_eq!(words(0), 0);
        assert_eq!(words(1), 1);
        assert_eq!(words(32), 1);
        assert_eq!(words(33), 2);
    }

    #[test]
    fn wrapped_call_adds_input_scaled_surcharge() {
        // 64-byte input spans exactly two 32-byte words.
        let input = [0u8; 64];

        // Baseline: identity precompile gas for this input, before any surcharge.
        let mut map = osaka_precompiles();
        let unwrapped = {
            let identity = get_precompile(&map, &IDENTITY);
            call_gas_used(&identity, &input, 1_000_000)
        };

        // Apply base = 100, per_word = 7. For two words: 100 + 2 * 7 = 114.
        apply_precompile_surcharge(
            &mut map,
            IDENTITY,
            PrecompileSurcharge { base: 100, per_word: 7 },
        );

        let wrapped = {
            let identity = get_precompile(&map, &IDENTITY);
            call_gas_used(&identity, &input, 1_000_000)
        };

        assert_eq!(
            wrapped,
            unwrapped + 114,
            "wrapped gas must be base precompile gas plus the input-scaled surcharge (100 + 2*7)"
        );
    }
}
