/// Default zk gas multiplier used when an opcode has no special override.
pub const DEFAULT_ZK_GAS_MULTIPLIER: u32 = 1;

/// Default per-transaction zk gas limit shared across all Taiko chains.
pub const DEFAULT_TX_ZK_GAS_LIMIT: u64 = 30_000_000;

/// Default per-block zk gas limit shared across all Taiko chains.
pub const DEFAULT_BLOCK_ZK_GAS_LIMIT: u64 = 30_000_000;

/// Shared zk gas configuration for all Taiko chains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZkGasConfig {
    /// Per-opcode multipliers indexed by opcode byte.
    pub multipliers: [u32; 256],
    /// Maximum zk gas allowed per transaction.
    pub tx_limit: u64,
    /// Maximum zk gas allowed per block.
    pub block_limit: u64,
}

/// Single shared zk gas configuration instance for Taiko chains.
pub const DEFAULT_ZK_GAS_CONFIG: ZkGasConfig = ZkGasConfig {
    multipliers: [DEFAULT_ZK_GAS_MULTIPLIER; 256],
    tx_limit: DEFAULT_TX_ZK_GAS_LIMIT,
    block_limit: DEFAULT_BLOCK_ZK_GAS_LIMIT,
};

impl Default for ZkGasConfig {
    /// Returns the shared zk gas configuration for all Taiko chains.
    fn default() -> Self {
        DEFAULT_ZK_GAS_CONFIG
    }
}

#[cfg(test)]
mod tests {
    use super::ZkGasConfig;

    /// Verifies the default zk gas config shape and non-zero limits.
    #[test]
    fn zk_gas_defaults_are_well_formed() {
        let cfg = ZkGasConfig::default();
        assert_eq!(cfg.multipliers.len(), 256);
        assert!(cfg.tx_limit > 0);
        assert!(cfg.block_limit > 0);
        assert!(cfg.multipliers.iter().all(|value| *value >= 1));
    }
}
