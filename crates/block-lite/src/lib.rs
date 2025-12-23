pub mod config {
    use std::sync::Arc;

    use alethia_reth_chainspec_lite::spec::TaikoChainSpec;

    /// Minimal Taiko EVM configuration for guest builds.
    #[derive(Debug, Clone)]
    pub struct TaikoEvmConfig {
        chain_spec: Arc<TaikoChainSpec>,
    }

    impl TaikoEvmConfig {
        /// Creates a new Taiko EVM configuration with the given chain spec.
        pub fn new(chain_spec: Arc<TaikoChainSpec>) -> Self {
            Self { chain_spec }
        }

        /// Returns the chain spec associated with this configuration.
        pub const fn chain_spec(&self) -> &Arc<TaikoChainSpec> {
            &self.chain_spec
        }
    }
}
