//! Per-transaction intrinsic gas surcharge for the repriced fork.

use reth_revm::{context::result::InvalidTransaction, interpreter::InitialAndFloorGas};

/// Adds `surcharge` to `gas.initial_total_gas` and re-checks it against `gas_limit`.
///
/// The surcharge is a per-transaction intrinsic proving cost, so it lifts only
/// `initial_total_gas`. `floor_gas` (the EIP-7623 calldata floor) is deliberately left
/// unchanged — it is a separate lower bound, and because the surcharge is already part of
/// `initial_total_gas` it is charged regardless of which bound dominates.
///
/// This re-runs only the same `initial_total_gas <= gas_limit` check revm performs (raising
/// the identical `CallGasCostMoreThanGasLimit`); revm's separate `floor_gas` check is
/// unaffected and still applies wherever this result is fed back into validation.
pub fn add_tx_intrinsic_surcharge(
    mut gas: InitialAndFloorGas,
    surcharge: u64,
    gas_limit: u64,
) -> Result<InitialAndFloorGas, InvalidTransaction> {
    gas.initial_total_gas = gas.initial_total_gas.saturating_add(surcharge);
    if gas.initial_total_gas > gas_limit {
        return Err(InvalidTransaction::CallGasCostMoreThanGasLimit {
            gas_limit,
            initial_gas: gas.initial_total_gas,
        });
    }
    Ok(gas)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surcharge_adds_to_initial_total_gas() {
        let gas = InitialAndFloorGas::new(21_000, 0);
        let out = add_tx_intrinsic_surcharge(gas, 243_000, 1_000_000).expect("fits");
        assert_eq!(out.initial_total_gas, 264_000);
    }

    #[test]
    fn surcharge_exceeding_limit_is_rejected() {
        let gas = InitialAndFloorGas::new(21_000, 0);
        let err = add_tx_intrinsic_surcharge(gas, 243_000, 100_000).expect_err("over limit");
        assert!(matches!(
            err,
            InvalidTransaction::CallGasCostMoreThanGasLimit {
                gas_limit: 100_000,
                initial_gas: 264_000
            }
        ));
    }
}
