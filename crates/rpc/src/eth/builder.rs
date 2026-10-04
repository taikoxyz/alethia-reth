use alethia_reth_block::config::{TaikoEvmConfig, TaikoNextBlockEnvAttributes};
use alethia_reth_chainspec::{hardfork::TaikoHardforks, spec::TaikoChainSpec};
use alethia_reth_primitives::{
    ETNA_EXTRA_DATA_LEN, SHASTA_EXTRA_DATA_LEN, engine::TaikoEngineTypes,
};
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

/// Supplies simulation-only Etna metadata when a pending target's parent lacks the Etna layout,
/// without fabricating L1 roots.
struct TaikoPendingEnvBuilder {
    /// Chain configuration used to gate the pending target's Etna fee context.
    evm: TaikoEvmConfig,
}

/// Returns simulation-only Etna fee metadata for a child of `parent`.
///
/// A 13-byte parent is copied, which models an inherited anchor. Any 7-byte parent (in practice the
/// last Shasta/Unzen block) keeps its fee share and proposal ID with a zero anchor number, and an
/// empty genesis uses zeros. The anchor number takes part in no execution, so these defaults cannot
/// change a simulation result. Other lengths are returned unchanged: block execution rejects them,
/// and call-style simulations run without a fee share.
pub(crate) fn etna_simulation_extra_data(parent: &Header) -> Bytes {
    match parent.extra_data.len() {
        SHASTA_EXTRA_DATA_LEN => {
            let mut extra_data = parent.extra_data.to_vec();
            extra_data.resize(ETNA_EXTRA_DATA_LEN, 0);
            extra_data.into()
        }
        0 if parent.number == 0 => Bytes::from_static(&[0; ETNA_EXTRA_DATA_LEN]),
        _ => parent.extra_data.clone(),
    }
}

impl PendingEnvBuilder<TaikoEvmConfig> for TaikoPendingEnvBuilder {
    /// Uses simulation-only Etna metadata when the pending target's parent lacks the Etna layout.
    ///
    /// Fee-enabled simulation credits base fees to the treasury; absent fee authority would not.
    /// The missing beacon root is preserved, so ordinary local pending builds still fall back.
    fn pending_env_attributes(
        &self,
        parent: &SealedHeader<Header>,
        block_overrides: Option<&BlockOverrides>,
    ) -> Result<TaikoNextBlockEnvAttributes, EthApiError> {
        let mut attributes =
            TaikoNextBlockEnvAttributes::build_pending_env(parent, block_overrides);
        if self.evm.chain_spec().is_etna_active(attributes.timestamp) {
            attributes.extra_data = etna_simulation_extra_data(parent);
        }
        Ok(attributes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alethia_reth_chainspec::{TAIKO_DEVNET, hardfork::TaikoHardfork};
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::B256;
    use reth_evm::ConfigureEvm;
    use std::sync::Arc;

    fn pending_builder(activation: u64) -> TaikoPendingEnvBuilder {
        let mut spec = TAIKO_DEVNET.as_ref().clone();
        spec.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(activation));
        TaikoPendingEnvBuilder { evm: TaikoEvmConfig::new(Arc::new(spec)) }
    }

    #[test]
    fn pending_genesis_default_is_gated_by_target_fork_number_and_empty_metadata() {
        for (activation, number, timestamp, extra_data, expected, percentage) in [
            (12, 0, 0, vec![], vec![0; 13], Some(0)),
            (13, 0, 0, vec![], vec![], None),
            (
                0,
                0,
                0,
                vec![25, 0, 0, 0, 0, 1, 2],
                vec![25, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0, 0, 0],
                Some(25),
            ),
            (0, 1, 1, vec![9; 13], vec![9; 13], Some(9)),
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
            // The pending environment takes its fee share from the simulated attributes.
            let env = builder.evm.next_evm_env(&parent, &attributes).unwrap();
            assert_eq!(env.block_env.base_fee_share_pctg, percentage);
        }
        // `eth_simulateV1` supplies an Etna root through block overrides; it must pass through.
        let overrides =
            BlockOverrides { beacon_root: Some(B256::with_last_byte(7)), ..Default::default() };
        let parent = SealedHeader::new_unhashed(Header::default());
        let attributes =
            pending_builder(0).pending_env_attributes(&parent, Some(&overrides)).unwrap();
        assert_eq!(attributes.parent_beacon_block_root, overrides.beacon_root);
    }
}
