//! Taiko precompiles implementation

use alloy_primitives::Address;
use reth_evm::precompiles::PrecompilesMap;
use reth_revm::{
    context::Cfg,
    context_interface::ContextTr,
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{CallInputs, InterpreterResult},
    precompile::Precompiles,
    primitives::hardfork::SpecId,
};
use std::{boxed::Box, sync::OnceLock};

use crate::spec::TaikoSpecId;

pub mod l1sload;

/// Taiko precompile provider
#[derive(Debug, Clone)]
pub struct TaikoPrecompiles {
    /// Inner precompile provider is same as Ethereum's.
    inner: EthPrecompiles,
    /// Spec id of the precompile provider.
    spec: TaikoSpecId,
}

impl TaikoPrecompiles {
    /// Create a new precompile provider with the given TaikoSpecId.
    #[inline]
    pub fn new_with_spec(spec: TaikoSpecId) -> Self {
        let precompiles = match spec {
            TaikoSpecId::GENESIS => Precompiles::new(spec.into_eth_spec().into()),
            TaikoSpecId::ONTAKE => ontake(),
            TaikoSpecId::PACAYA => pacaya(),
            TaikoSpecId::SHASTA => shasta(),
        };

        Self { inner: EthPrecompiles { precompiles, spec: SpecId::default() }, spec }
    }

    /// Precompiles getter.
    #[inline]
    pub fn precompiles(&self) -> &'static Precompiles {
        self.inner.precompiles
    }
}

/// Returns precompiles for Ontake spec.
pub fn ontake() -> &'static Precompiles {
    static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
    INSTANCE.get_or_init(|| Precompiles::berlin().clone())
}

/// Returns precompiles for Pacaya spec.
pub fn pacaya() -> &'static Precompiles {
    static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = ontake().clone();
        precompiles.extend([l1sload::L1SLOAD]);
        precompiles
    })
}

/// Returns precompiles for Shasta spec.
pub fn shasta() -> &'static Precompiles {
    static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
    INSTANCE.get_or_init(|| pacaya().clone())
}

impl<CTX> PrecompileProvider<CTX> for TaikoPrecompiles
where
    CTX: ContextTr<Cfg: Cfg<Spec = TaikoSpecId>>,
{
    type Output = InterpreterResult;

    #[inline]
    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool {
        if spec == self.spec {
            return false;
        }
        *self = Self::new_with_spec(spec);
        true
    }

    #[inline]
    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        self.inner.run(context, inputs)
    }

    #[inline]
    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        self.inner.warm_addresses()
    }

    #[inline]
    fn contains(&self, address: &Address) -> bool {
        self.inner.contains(address)
    }
}

impl Default for TaikoPrecompiles {
    fn default() -> Self {
        Self::new_with_spec(TaikoSpecId::default())
    }
}

impl From<TaikoPrecompiles> for PrecompilesMap {
    fn from(val: TaikoPrecompiles) -> Self {
        PrecompilesMap::from_static(val.inner.precompiles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_taiko_precompiles_creation() {
        let specs = vec![
            TaikoSpecId::GENESIS,
            TaikoSpecId::ONTAKE,
            TaikoSpecId::PACAYA,
            TaikoSpecId::SHASTA,
        ];

        for spec in specs {
            let precompiles = TaikoPrecompiles::new_with_spec(spec);
            assert_eq!(precompiles.spec, spec);

            let addresses: Vec<_> = precompiles.precompiles().addresses().collect();
            assert!(!addresses.is_empty(), "Precompiles should not be empty for spec {:?}", spec);

            let ecrecover_addr = address!("0000000000000000000000000000000000000001");
            assert!(
                addresses.contains(&&ecrecover_addr),
                "ECRECOVER should be present for spec {:?}",
                spec
            );
        }
    }

    #[test]
    fn test_spec_specific_precompiles() {
        let ontake_p = TaikoPrecompiles::new_with_spec(TaikoSpecId::ONTAKE);
        let pacaya_p = TaikoPrecompiles::new_with_spec(TaikoSpecId::PACAYA);
        let shasta_p = TaikoPrecompiles::new_with_spec(TaikoSpecId::SHASTA);

        let ontake_count = ontake_p.precompiles().addresses().count();
        let pacaya_count = pacaya_p.precompiles().addresses().count();
        let shasta_count = shasta_p.precompiles().addresses().count();

        assert_eq!(
            pacaya_count,
            ontake_count + 1,
            "Pacaya should have L1SLOAD precompile added to Ontake"
        );
        assert_eq!(shasta_count, pacaya_count, "Shasta inherits from Pacaya");

        let l1sload_addr = address!("0000000000000000000000000000000000010001");
        let pacaya_addrs: Vec<_> = pacaya_p.precompiles().addresses().collect();
        let shasta_addrs: Vec<_> = shasta_p.precompiles().addresses().collect();
        let ontake_addrs: Vec<_> = ontake_p.precompiles().addresses().collect();

        assert!(pacaya_addrs.contains(&&l1sload_addr), "Pacaya should contain L1SLOAD");
        assert!(shasta_addrs.contains(&&l1sload_addr), "Shasta should contain L1SLOAD");
        assert!(!ontake_addrs.contains(&&l1sload_addr), "Ontake should NOT contain L1SLOAD");
    }

    #[test]
    fn test_default_implementation() {
        let default_precompiles = TaikoPrecompiles::default();
        let expected_spec = TaikoSpecId::default();

        assert_eq!(default_precompiles.spec, expected_spec);
        assert!(!default_precompiles.precompiles().addresses().collect::<Vec<_>>().is_empty());
    }
}
