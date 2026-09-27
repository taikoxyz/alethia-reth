use alethia_reth_block::config::{TaikoEvmConfig, TaikoNextBlockEnvAttributes};
use alethia_reth_chainspec::{hardfork::TaikoHardforks, spec::TaikoChainSpec};
use alethia_reth_primitives::engine::TaikoEngineTypes;
use alloy_consensus::Header;
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::BlockOverrides;
use reth_primitives_traits::SealedHeader;
use reth_rpc_eth_api::helpers::pending_block::{BuildPendingEnv, PendingEnvBuilder};
use reth_rpc_eth_types::EthApiError;

use reth_ethereum::EthPrimitives;
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_node_builder::rpc::{EthApiBuilder, EthApiCtx};
use reth_rpc::{EthApi, eth::core::EthRpcConverterFor};

/// Builds the Taiko `eth` API ([`EthApi`]) for the Taiko node.
#[derive(Debug, Default)]
pub struct TaikoEthApiBuilder;

impl<N> EthApiBuilder<N> for TaikoEthApiBuilder
where
    N: FullNodeComponents<Evm = TaikoEvmConfig>,
    N::Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = TaikoChainSpec,
            Payload = TaikoEngineTypes,
        >,
{
    /// The Ethapi implementation this builder will build.
    type EthApi = EthApi<N, EthRpcConverterFor<N>>;

    /// Builds the [`EthApi`] from the given context.
    async fn build_eth_api(self, ctx: EthApiCtx<'_, N>) -> eyre::Result<Self::EthApi> {
        let pending = TaikoPendingEnvBuilder { evm: ctx.components.evm_config().clone() };
        Ok(ctx.eth_api_builder().with_pending_env_builder(pending).build())
    }
}

/// Supplies simulation-only metadata missing from a genesis parent, without fabricating L1 roots.
struct TaikoPendingEnvBuilder {
    /// Chain configuration used to gate the pending target's TBD fee context.
    evm: TaikoEvmConfig,
}

impl PendingEnvBuilder<TaikoEvmConfig> for TaikoPendingEnvBuilder {
    /// Uses zero-percent sharing only for a TBD pending target over empty-metadata genesis.
    ///
    /// Seven zero bytes are a simulation default, not authoritative metadata for a real block.
    /// Fee-enabled simulation credits base fees to the treasury; absent fee authority would not.
    /// The missing beacon root is preserved, so ordinary local pending builds still fall back.
    fn pending_env_attributes(
        &self,
        parent: &SealedHeader<Header>,
        block_overrides: Option<&BlockOverrides>,
    ) -> Result<TaikoNextBlockEnvAttributes, EthApiError> {
        let mut attributes =
            TaikoNextBlockEnvAttributes::build_pending_env(parent, block_overrides);
        if parent.number == 0 &&
            parent.extra_data.is_empty() &&
            self.evm.chain_spec().is_tbd_active(attributes.timestamp)
        {
            attributes.extra_data = Bytes::from_static(&[0; 7]);
        }
        Ok(attributes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alethia_reth_block::config::InvalidTbdExtraData;
    use alethia_reth_chainspec::{TAIKO_DEVNET, hardfork::TaikoHardfork};
    use alethia_reth_primitives::tbd::MissingTbdBeaconRoot;
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::B256;
    use reth_evm::ConfigureEvm;
    use std::sync::Arc;

    fn pending_builder(activation: u64) -> TaikoPendingEnvBuilder {
        let mut spec = TAIKO_DEVNET.as_ref().clone();
        spec.inner.hardforks.insert(TaikoHardfork::TBD, ForkCondition::Timestamp(activation));
        TaikoPendingEnvBuilder { evm: TaikoEvmConfig::new(Arc::new(spec)) }
    }

    #[test]
    fn pending_genesis_default_is_gated_by_target_fork_number_and_empty_metadata() {
        for (activation, number, timestamp, extra_data, expected) in [
            (12, 0, 0, vec![], vec![0; 7]),
            (13, 0, 0, vec![], vec![]),
            (0, 1, 1, vec![], vec![]),
            (0, 0, 0, vec![1], vec![1]),
            (0, 0, 0, vec![25, 0, 0, 0, 0, 1, 2], vec![25, 0, 0, 0, 0, 1, 2]),
        ] {
            let builder = pending_builder(activation);
            let parent = SealedHeader::new_unhashed(Header {
                number,
                timestamp,
                extra_data: extra_data.into(),
                ..Default::default()
            });
            let attributes = builder.pending_env_attributes(&parent, None).unwrap();
            assert_eq!(attributes.extra_data.as_ref(), expected);
            assert_eq!(attributes.parent_beacon_block_root, None);
            if expected.len() != 7 && activation <= attributes.timestamp {
                assert!(builder.evm.next_evm_env(&parent, &attributes).is_err());
            }
        }
    }

    #[test]
    fn pending_simulation_preserves_real_context_guards_and_missing_root_fallback() {
        let builder = pending_builder(0);
        let genesis = SealedHeader::new_unhashed(Header::default());
        let real = TaikoNextBlockEnvAttributes::build_pending_env(&genesis, None);
        assert!(
            builder
                .evm
                .next_evm_env(&genesis, &real)
                .unwrap_err()
                .as_error()
                .is::<InvalidTbdExtraData>()
        );
        let simulated = builder.pending_env_attributes(&genesis, None).unwrap();
        let env = builder.evm.next_evm_env(&genesis, &simulated).unwrap();
        assert_eq!(env.block_env.base_fee_share_pctg, Some(0));
        assert!(
            builder
                .evm
                .context_for_next_block(&genesis, simulated)
                .unwrap_err()
                .as_error()
                .is::<MissingTbdBeaconRoot>()
        );
        for (activation, timestamp) in [(100, 99), (0, 1)] {
            let builder = pending_builder(activation);
            let parent = SealedHeader::new_unhashed(Header {
                number: 1,
                timestamp,
                extra_data: Bytes::from_static(&[25, 0, 0, 0, 0, 0, 1]),
                ..Default::default()
            });
            let attributes = builder.pending_env_attributes(&parent, None).unwrap();
            assert_eq!(
                builder
                    .evm
                    .next_evm_env(&parent, &attributes)
                    .unwrap()
                    .block_env
                    .base_fee_share_pctg,
                Some(25)
            );
            assert_eq!(attributes.parent_beacon_block_root, None);
            assert!(
                builder
                    .evm
                    .context_for_next_block(&parent, attributes)
                    .unwrap_err()
                    .as_error()
                    .is::<MissingTbdBeaconRoot>()
            );
            let overrides =
                BlockOverrides { beacon_root: Some(B256::with_last_byte(7)), ..Default::default() };
            assert_eq!(
                builder
                    .pending_env_attributes(&parent, Some(&overrides))
                    .unwrap()
                    .parent_beacon_block_root,
                overrides.beacon_root
            );
        }
    }
}
