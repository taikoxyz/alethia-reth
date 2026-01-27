use reth::revm::inspector::NoOpInspector;

/// Experimental zk gas value for a single `JUMPDEST` execution.
pub const JUMPDEST_ZK_GAS: u64 = 26;

/// Provides zk gas usage for the current block.
pub trait ZkGasTracker {
    /// Returns total zk gas used so far.
    fn zk_gas_used(&self) -> u64;
}

impl ZkGasTracker for NoOpInspector {
    fn zk_gas_used(&self) -> u64 {
        0
    }
}
