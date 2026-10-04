//! Taiko block and EVM configuration used by node and payload services.
use std::{borrow::Cow, sync::Arc};

use alloy_consensus::{BlockHeader, Header};
#[cfg(feature = "net")]
use alloy_eips::Decodable2718;
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::Withdrawals;
use reth_chainspec::EthChainSpec;
use reth_ethereum_forks::Hardforks;
use reth_ethereum_primitives::EthPrimitives;
#[cfg(feature = "net")]
use reth_evm::ConfigureEngineEvm;
use reth_evm::{ConfigureEvm, EvmEnv, EvmEnvFor};
#[cfg(feature = "net")]
use reth_evm::{ExecutableTxIterator, ExecutionCtxFor};
use reth_evm_ethereum::RethReceiptBuilder;
#[cfg(feature = "net")]
use reth_payload_primitives::ExecutionPayload;
use reth_primitives_traits::{
    BlockTy, SealedBlock, SealedHeader, constants::MAX_TX_GAS_LIMIT_OSAKA,
};
#[cfg(feature = "net")]
use reth_primitives_traits::{SignedTransaction, TxTy};
use reth_revm::{
    context::{BlockEnv, CfgEnv},
    context_interface::block::BlobExcessGasAndPrice,
    primitives::{Address, B256, U256, hardfork::SpecId},
};
#[cfg(feature = "net")]
use reth_rpc_eth_api::helpers::pending_block::BuildPendingEnv;
use reth_storage_errors::any::AnyError;

use crate::{
    assembler::TaikoBlockAssembler,
    factory::{TaikoBlockExecutionCtx, TaikoBlockExecutorFactory},
};
use alethia_reth_chainspec::{
    hardfork::{TaikoHardfork, TaikoHardforks},
    spec::TaikoChainSpec,
};
use alethia_reth_evm::{env::TaikoBlockEnv, factory::TaikoEvmFactory, spec::TaikoSpecId};
#[cfg(feature = "net")]
use alethia_reth_primitives::engine::types::TaikoExecutionData;
use alethia_reth_primitives::{
    ETNA_EXTRA_DATA_LEN, decode_shasta_basefee_sharing_pctg,
    etna::{MissingEtnaBeaconRoot, validate_etna_extra_data, validate_etna_root},
};

/// Error when base fee is missing from a block header.
#[derive(Debug)]
pub struct MissingBaseFee {
    /// The block number where base fee was missing.
    pub block_number: u64,
}

impl std::fmt::Display for MissingBaseFee {
    /// Formats the missing-base-fee error with the affected block number.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "missing base_fee_per_gas in block {}", self.block_number)
    }
}

impl std::error::Error for MissingBaseFee {}

/// Error when an Unzen payload sidecar is missing the hash-relevant header difficulty.
#[derive(Debug)]
pub struct MissingUnzenHeaderDifficulty {
    /// The block number whose payload sidecar lacked the header difficulty.
    pub block_number: u64,
}

impl std::fmt::Display for MissingUnzenHeaderDifficulty {
    /// Formats the missing-header-difficulty error with the affected block number.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "missing header difficulty for Unzen payload at block {}", self.block_number)
    }
}

impl std::error::Error for MissingUnzenHeaderDifficulty {}

/// Carries header fee authority into Etna replay environments, rejecting malformed target blocks.
/// Genesis may lack the Etna layout; it then retains no authoritative fee percentage.
fn with_taiko_fee_context(
    block_env: BlockEnv,
    spec: TaikoSpecId,
    extra_data: &[u8],
) -> Result<TaikoBlockEnv, AnyError> {
    let mut block_env = TaikoBlockEnv::from(block_env);
    if spec.is_enabled_in(TaikoSpecId::ETNA) {
        validate_etna_extra_data(true, block_env.number.to(), extra_data).map_err(AnyError::new)?;
        if extra_data.len() == ETNA_EXTRA_DATA_LEN {
            block_env = block_env.with_base_fee_share_pctg(u64::from(
                decode_shasta_basefee_sharing_pctg(extra_data),
            ));
        }
    }
    Ok(block_env)
}

/// A complete configuration of EVM for Taiko network.
#[derive(Debug, Clone)]
pub struct TaikoEvmConfig {
    /// Block executor factory configured for Taiko execution rules.
    pub executor_factory: TaikoBlockExecutorFactory,
    /// Block assembler used to construct finalized block objects.
    pub block_assembler: TaikoBlockAssembler,
}

impl TaikoEvmConfig {
    /// Creates a new Taiko EVM configuration with the given chain spec and extra context.
    pub fn new(chain_spec: Arc<TaikoChainSpec>) -> Self {
        Self::new_with_evm_factory(chain_spec, TaikoEvmFactory)
    }

    /// Creates a new Taiko EVM configuration with the given chain spec and EVM factory.
    pub fn new_with_evm_factory(
        chain_spec: Arc<TaikoChainSpec>,
        evm_factory: TaikoEvmFactory,
    ) -> Self {
        Self {
            block_assembler: TaikoBlockAssembler,
            executor_factory: TaikoBlockExecutorFactory::new(
                RethReceiptBuilder::default(),
                chain_spec,
                evm_factory,
            ),
        }
    }

    /// Returns the chain spec associated with this configuration.
    pub const fn chain_spec(&self) -> &Arc<TaikoChainSpec> {
        self.executor_factory.spec()
    }

    /// Returns the EVM factory backing this configuration.
    pub const fn evm_factory(&self) -> &TaikoEvmFactory {
        self.executor_factory.evm_factory()
    }
}

/// Returns the zero blob-gas environment used for Cancun-or-later RPC execution.
fn taiko_blob_excess_gas_and_price(spec: TaikoSpecId) -> Option<BlobExcessGasAndPrice> {
    spec.into_eth_spec()
        .is_enabled_in(SpecId::CANCUN)
        .then_some(BlobExcessGasAndPrice { excess_blob_gas: 0, blob_gasprice: 1 })
}

/// Applies the Unzen zero-root fallback; earlier forks carry no beacon root.
fn legacy_beacon_root_fallback(is_post_unzen: bool, root: Option<B256>) -> Option<B256> {
    if is_post_unzen { root.or(Some(B256::ZERO)) } else { None }
}

/// Validates Etna roots before applying the legacy Unzen zero-root fallback.
/// Genesis remains exempt from the nonzero-root requirement.
fn normalize_parent_beacon_block_root(
    is_unzen_active: bool,
    is_etna_active: bool,
    block_number: u64,
    parent_beacon_block_root: Option<B256>,
) -> Result<Option<B256>, MissingEtnaBeaconRoot> {
    validate_etna_root(is_etna_active, block_number, parent_beacon_block_root)?;
    Ok(legacy_beacon_root_fallback(is_unzen_active || is_etna_active, parent_beacon_block_root))
}

/// Builds the execution context for the block after `parent` without the Etna beacon-root check.
///
/// Only simulations that never apply pre-execution system calls may use this directly, such as
/// tx-pool preselection. [`ConfigureEvm::context_for_next_block`] validates the root first.
pub fn simulation_context_for_next_block<'a>(
    chain_spec: &TaikoChainSpec,
    parent: &SealedHeader,
    attributes: TaikoNextBlockEnvAttributes,
) -> TaikoBlockExecutionCtx<'a> {
    let is_unzen_active = chain_spec.is_unzen_active(attributes.timestamp);
    let is_etna_active = chain_spec.is_etna_active(attributes.timestamp);
    TaikoBlockExecutionCtx {
        parent_hash: parent.hash(),
        parent_beacon_block_root: legacy_beacon_root_fallback(
            is_unzen_active || is_etna_active,
            attributes.parent_beacon_block_root,
        ),
        ommers: &[],
        withdrawals: Some(Cow::Owned(Withdrawals::new(vec![]))),
        basefee_per_gas: attributes.base_fee_per_gas,
        extra_data: attributes.extra_data,
        is_unzen_active,
        expected_difficulty: None,
        finalized_block_zk_gas: Default::default(),
    }
}

impl ConfigureEvm for TaikoEvmConfig {
    /// The primitives type used by the EVM.
    type Primitives = EthPrimitives;
    /// The error type that is returned by [`Self::next_evm_env`].
    type Error = AnyError;
    /// Context required for configuring next block environment.
    ///
    /// Contains values that can't be derived from the parent block.
    type NextBlockEnvCtx = TaikoNextBlockEnvAttributes;
    /// Configured [`BlockExecutorFactory`], contains [`EvmFactory`] internally.
    type BlockExecutorFactory =
        TaikoBlockExecutorFactory<RethReceiptBuilder, Arc<TaikoChainSpec>, TaikoEvmFactory>;
    /// The assembler to build a Taiko block.
    type BlockAssembler = TaikoBlockAssembler;

    /// Returns reference to the configured [`BlockExecutorFactory`].
    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    /// Returns reference to the configured [`BlockAssembler`].
    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.block_assembler
    }

    /// Creates a new [`EvmEnv`] for the given header.
    fn evm_env(&self, header: &Header) -> Result<EvmEnvFor<Self>, Self::Error> {
        let spec = taiko_revm_spec(&self.chain_spec().inner, header);
        let mut cfg_env = CfgEnv::new()
            .with_chain_id(self.chain_spec().inner.chain().id())
            .with_spec_and_mainnet_gas_params(spec);

        if self.chain_spec().inner.is_osaka_active_at_timestamp(header.timestamp()) {
            cfg_env.tx_gas_limit_cap = Some(MAX_TX_GAS_LIMIT_OSAKA);
        }

        let basefee: u64 = header
            .base_fee_per_gas()
            .ok_or_else(|| AnyError::new(MissingBaseFee { block_number: header.number() }))?;
        let block_env = BlockEnv {
            number: U256::from(header.number()),
            beneficiary: header.beneficiary(),
            timestamp: U256::from(header.timestamp()),
            difficulty: if self.chain_spec().is_unzen_active(header.timestamp()) {
                header.difficulty()
            } else {
                U256::ZERO
            },
            prevrandao: header.mix_hash(),
            gas_limit: header.gas_limit(),
            basefee,
            blob_excess_gas_and_price: taiko_blob_excess_gas_and_price(spec),
            slot_num: 0,
        };

        let block_env = with_taiko_fee_context(block_env, spec, &header.extra_data)?;
        Ok(EvmEnv { cfg_env, block_env })
    }

    /// Returns the configured [`EvmEnv`] for `parent + 1` block.
    ///
    /// This is intended for usage in block building after the merge and requires additional
    /// attributes that can't be derived from the parent block: attributes that are determined by
    /// the CL, such as the timestamp, suggested fee recipient, and randomness value.
    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        let spec = taiko_spec_by_timestamp_and_block_number(
            &self.chain_spec().inner,
            attributes.timestamp,
            parent.number + 1,
        );
        let mut cfg = CfgEnv::new()
            .with_chain_id(self.chain_spec().inner.chain().id())
            .with_spec_and_mainnet_gas_params(spec);

        if self.chain_spec().inner.is_osaka_active_at_timestamp(attributes.timestamp) {
            cfg.tx_gas_limit_cap = Some(MAX_TX_GAS_LIMIT_OSAKA);
        }

        let block_env: BlockEnv = BlockEnv {
            number: U256::from(parent.number + 1),
            beneficiary: attributes.suggested_fee_recipient,
            timestamp: U256::from(attributes.timestamp),
            difficulty: U256::ZERO,
            prevrandao: Some(attributes.prev_randao),
            gas_limit: attributes.gas_limit,
            basefee: attributes.base_fee_per_gas,
            blob_excess_gas_and_price: taiko_blob_excess_gas_and_price(spec),
            slot_num: 0,
        };

        let block_env = with_taiko_fee_context(block_env, spec, &attributes.extra_data)?;
        Ok((cfg, block_env).into())
    }

    /// Returns the configured [`BlockExecutorFactory::ExecutionCtx`] for a given block.
    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<BlockTy<Self::Primitives>>,
    ) -> Result<reth_evm::ExecutionCtxFor<'a, Self>, Self::Error> {
        let is_unzen_active = self.chain_spec().is_unzen_active(block.header().timestamp);
        let basefee_per_gas = block
            .header()
            .base_fee_per_gas
            .ok_or_else(|| AnyError::new(MissingBaseFee { block_number: block.header().number }))?;
        validate_etna_root(
            self.chain_spec().is_etna_active(block.header().timestamp),
            block.header().number,
            block.header().parent_beacon_block_root,
        )
        .map_err(AnyError::new)?;
        Ok(TaikoBlockExecutionCtx {
            parent_hash: block.header().parent_hash,
            parent_beacon_block_root: block.header().parent_beacon_block_root,
            ommers: &[],
            withdrawals: Some(Cow::Owned(Withdrawals::new(vec![]))),
            basefee_per_gas,
            extra_data: block.header().extra_data.clone(),
            is_unzen_active,
            expected_difficulty: is_unzen_active.then_some(block.header().difficulty),
            finalized_block_zk_gas: Default::default(),
        })
    }

    /// Returns the configured [`BlockExecutorFactory::ExecutionCtx`] for `parent + 1`
    /// block.
    fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        ctx: Self::NextBlockEnvCtx,
    ) -> Result<reth_evm::ExecutionCtxFor<'_, Self>, Self::Error> {
        validate_etna_root(
            self.chain_spec().is_etna_active(ctx.timestamp),
            parent.number + 1,
            ctx.parent_beacon_block_root,
        )
        .map_err(AnyError::new)?;
        Ok(simulation_context_for_next_block(self.chain_spec(), parent, ctx))
    }
}

#[cfg(feature = "net")]
impl ConfigureEngineEvm<TaikoExecutionData> for TaikoEvmConfig {
    /// Returns an [`EvmEnvFor`] for the given payload.
    fn evm_env_for_payload(
        &self,
        payload: &TaikoExecutionData,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        let timestamp = payload.timestamp();
        let block_number = payload.block_number();

        let blob_params = self.chain_spec().blob_params_at_timestamp(timestamp);
        let spec =
            taiko_spec_by_timestamp_and_block_number(self.chain_spec(), timestamp, block_number);

        // configure evm env based on parent block
        let mut cfg_env = CfgEnv::new()
            .with_chain_id(self.chain_spec().chain().id())
            .with_spec_and_mainnet_gas_params(spec);

        if let Some(blob_params) = &blob_params {
            cfg_env.set_max_blobs_per_tx(blob_params.max_blobs_per_tx);
        }

        if self.chain_spec().is_osaka_active_at_timestamp(timestamp) {
            cfg_env.tx_gas_limit_cap = Some(MAX_TX_GAS_LIMIT_OSAKA);
        }

        let block_env = BlockEnv {
            number: U256::from(block_number),
            beneficiary: payload.execution_payload.fee_recipient,
            timestamp: U256::from(timestamp),
            difficulty: U256::ZERO,
            prevrandao: Some(payload.execution_payload.prev_randao),
            gas_limit: payload.execution_payload.gas_limit,
            basefee: payload.execution_payload.base_fee_per_gas.saturating_to(),
            blob_excess_gas_and_price: taiko_blob_excess_gas_and_price(spec),
            slot_num: 0,
        };

        let block_env =
            with_taiko_fee_context(block_env, spec, &payload.execution_payload.extra_data)?;
        Ok((cfg_env, block_env).into())
    }

    /// Returns an [`ExecutionCtxFor`] for the given payload.
    fn context_for_payload<'a>(
        &self,
        payload: &'a TaikoExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        let is_unzen_active = self.chain_spec().is_unzen_active(payload.timestamp());
        let is_etna_active = self.chain_spec().is_etna_active(payload.timestamp());
        // Unzen commits the finalized block zk gas into the header difficulty, so payload
        // execution must validate it against the sidecar value the same way block re-execution
        // does. Requiring the sidecar value keeps `engine_newPayload` fail-closed instead of
        // silently skipping the check.
        let expected_difficulty = if is_unzen_active {
            Some(payload.taiko_sidecar.header_difficulty.ok_or_else(|| {
                AnyError::new(MissingUnzenHeaderDifficulty { block_number: payload.block_number() })
            })?)
        } else {
            None
        };
        Ok(TaikoBlockExecutionCtx {
            parent_hash: payload.parent_hash(),
            parent_beacon_block_root: normalize_parent_beacon_block_root(
                is_unzen_active,
                is_etna_active,
                payload.block_number(),
                // Legacy conversion commits the Unzen zero root, independently of sidecar data.
                if is_etna_active { payload.parent_beacon_block_root() } else { None },
            )
            .map_err(AnyError::new)?,
            ommers: &[],
            withdrawals: payload.withdrawals().map(|w| Cow::Owned(w.clone().into())),
            basefee_per_gas: payload.execution_payload.base_fee_per_gas.saturating_to(),
            extra_data: payload.execution_payload.extra_data.clone(),
            is_unzen_active,
            expected_difficulty,
            finalized_block_zk_gas: Default::default(),
        })
    }

    /// Returns an [`ExecutableTxIterator`] for the given payload.
    fn tx_iterator_for_payload(
        &self,
        payload: &TaikoExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        let txs = payload.execution_payload.transactions.clone().unwrap_or_default();
        let convert = |tx: Bytes| {
            let tx =
                TxTy::<Self::Primitives>::decode_2718_exact(tx.as_ref()).map_err(AnyError::new)?;
            let signer = tx.try_recover().map_err(AnyError::new)?;
            Ok::<_, AnyError>(tx.with_signer(signer))
        };

        Ok((txs, convert))
    }
}

/// Context relevant for execution of a next block w.r.t Taiko.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaikoNextBlockEnvAttributes {
    /// The timestamp of the next block.
    pub timestamp: u64,
    /// The suggested fee recipient for the next block.
    pub suggested_fee_recipient: Address,
    /// The randomness value for the next block.
    pub prev_randao: B256,
    /// Block gas limit.
    pub gas_limit: u64,
    /// Encoded base fee share pctg parameters to include into block's `extra_data` field.
    pub extra_data: Bytes,
    /// The base fee per gas for the next block.
    pub base_fee_per_gas: u64,
    /// Parent beacon block root used by the EIP-4788 system call.
    pub parent_beacon_block_root: Option<B256>,
}

/// Map the latest active hardfork at the given header to a [`TaikoSpecId`].
pub fn taiko_revm_spec<C>(chain_spec: &C, header: &Header) -> TaikoSpecId
where
    C: EthereumHardforks + EthChainSpec + Hardforks,
{
    taiko_spec_by_timestamp_and_block_number(chain_spec, header.timestamp, header.number)
}

/// Map the latest active hardfork at the given timestamp or block number to a [`TaikoSpecId`].
pub fn taiko_spec_by_timestamp_and_block_number<C>(
    chain_spec: &C,
    timestamp: u64,
    block_number: u64,
) -> TaikoSpecId
where
    C: EthereumHardforks + EthChainSpec + Hardforks,
{
    if chain_spec.fork(TaikoHardfork::Etna).active_at_timestamp(timestamp) {
        TaikoSpecId::ETNA
    } else if chain_spec.fork(TaikoHardfork::Unzen).active_at_timestamp(timestamp) {
        TaikoSpecId::UNZEN
    } else if chain_spec.fork(TaikoHardfork::Shasta).active_at_timestamp(timestamp) {
        // London is on from genesis for Taiko, so Shasta reduces to the timestamp activation.
        TaikoSpecId::SHASTA
    } else if chain_spec
        .fork(TaikoHardfork::Pacaya)
        .active_at_timestamp_or_number(timestamp, block_number)
    {
        TaikoSpecId::PACAYA
    } else if chain_spec
        .fork(TaikoHardfork::Ontake)
        .active_at_timestamp_or_number(timestamp, block_number)
    {
        TaikoSpecId::ONTAKE
    } else {
        TaikoSpecId::GENESIS
    }
}

#[cfg(feature = "net")]
impl BuildPendingEnv<Header> for TaikoNextBlockEnvAttributes {
    /// Builds a [`ConfigureEvm::NextBlockEnvCtx`] for pending block.
    fn build_pending_env(
        parent: &SealedHeader<Header>,
        block_overrides: Option<&alloy_rpc_types_eth::BlockOverrides>,
    ) -> Self {
        Self {
            timestamp: parent.timestamp.saturating_add(12),
            suggested_fee_recipient: parent.beneficiary,
            prev_randao: B256::random(),
            gas_limit: parent.gas_limit,
            extra_data: parent.extra_data.clone(),
            base_fee_per_gas: parent.base_fee_per_gas.unwrap_or_default(),
            parent_beacon_block_root: block_overrides.and_then(|overrides| overrides.beacon_root),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alethia_reth_chainspec::{TAIKO_DEVNET, hardfork::TaikoHardfork};
    use alloy_hardforks::ForkCondition;
    use std::sync::Arc;

    fn config_with_unzen_at(timestamp: u64) -> TaikoEvmConfig {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec
            .inner
            .hardforks
            .insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(timestamp));
        TaikoEvmConfig::new(Arc::new(chain_spec))
    }

    fn config_with_etna_at(timestamp: u64) -> TaikoEvmConfig {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(timestamp));
        TaikoEvmConfig::new(Arc::new(chain_spec))
    }

    fn etna_header(percentage: u8) -> Header {
        Header {
            number: 1,
            timestamp: 1,
            gas_limit: 30_000_000,
            beneficiary: Address::with_last_byte(0xBB),
            base_fee_per_gas: Some(10_000_000),
            extra_data: vec![percentage, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1].into(),
            ..Default::default()
        }
    }

    #[test]
    fn etna_block_and_next_context_require_nonzero_root() {
        use reth_ethereum_primitives::{Block, BlockBody};
        let config = config_with_etna_at(0);
        let parent = SealedHeader::seal_slow(etna_header(0));
        for root in [None, Some(B256::ZERO), Some(B256::with_last_byte(7))] {
            let mut header = etna_header(0);
            header.parent_beacon_block_root = root;
            let block = SealedBlock::seal_slow(Block {
                header: header.clone(),
                body: BlockBody::default(),
            });
            let result = config.context_for_block(&block);
            assert_eq!(result.is_ok(), root.is_some_and(|r| !r.is_zero()), "block root {root:?}");
            if let Ok(ctx) = result {
                assert_eq!(ctx.parent_beacon_block_root, root);
            }
            let attrs = TaikoNextBlockEnvAttributes {
                timestamp: 2,
                suggested_fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                gas_limit: 30_000_000,
                extra_data: vec![0; 13].into(),
                base_fee_per_gas: 1,
                parent_beacon_block_root: root,
            };
            let simulated =
                simulation_context_for_next_block(config.chain_spec(), &parent, attrs.clone());
            assert_eq!(simulated.parent_beacon_block_root, Some(root.unwrap_or_default()));
            let result = config.context_for_next_block(&parent, attrs);
            assert_eq!(result.is_ok(), root.is_some_and(|r| !r.is_zero()), "next root {root:?}");
            if let Ok(ctx) = result {
                assert_eq!(ctx.parent_beacon_block_root, root);
            }
            header.number = 0;
            let genesis = SealedBlock::seal_slow(Block { header, body: BlockBody::default() });
            assert!(config.context_for_block(&genesis).is_ok());
        }
    }

    #[test]
    fn etna_header_environment_validates_extra_data_and_preserves_genesis() {
        let config = config_with_etna_at(0);
        let mut header = etna_header(25);
        assert_eq!(config.evm_env(&header).unwrap().block_env.base_fee_share_pctg, Some(25));
        header.extra_data = vec![0; 13].into();
        assert_eq!(config.evm_env(&header).unwrap().block_env.base_fee_share_pctg, Some(0));
        for len in [0, 1, 7, 12, 14] {
            header.extra_data = vec![0; len].into();
            assert!(config.evm_env(&header).is_err(), "non-genesis extraData length {len}");
            assert!(
                config_with_etna_at(100)
                    .evm_env(&header)
                    .unwrap()
                    .block_env
                    .base_fee_share_pctg
                    .is_none()
            );
        }
        let genesis_env = config.evm_env(config.chain_spec().genesis_header()).unwrap();
        assert_eq!(genesis_env.cfg_env.spec, TaikoSpecId::ETNA);
        assert_eq!(genesis_env.block_env.base_fee_share_pctg, None);
    }

    #[test]
    fn etna_next_environment_uses_attribute_fee_percentage() {
        let config = config_with_etna_at(0);
        let parent = etna_header(80);
        let mut attrs = TaikoNextBlockEnvAttributes {
            timestamp: 2,
            suggested_fee_recipient: Address::ZERO,
            prev_randao: B256::ZERO,
            gas_limit: 30_000_000,
            extra_data: vec![25, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 1].into(),
            base_fee_per_gas: 1,
            parent_beacon_block_root: Some(B256::ZERO),
        };
        assert_eq!(
            config.next_evm_env(&parent, &attrs).unwrap().block_env.base_fee_share_pctg,
            Some(25)
        );
        attrs.extra_data = vec![25, 0, 0, 0, 0, 0, 2].into();
        assert!(config.next_evm_env(&parent, &attrs).is_err());
    }

    #[test]
    fn etna_recreated_inspected_environment_preserves_fee_authority() {
        use alethia_reth_evm::{alloy::TaikoAnchorEvm, handler::get_treasury_address};
        use alloy_evm::{Evm, EvmFactory};
        use reth_revm::{
            context::TxEnv, db::InMemoryDB, inspector::NoOpInspector, state::AccountInfo,
        };

        let config = config_with_etna_at(0);
        let caller = Address::with_last_byte(0xA1);
        let balance = U256::from(100_000_000_000_000u64);
        for (percentage, treasury_fee, beneficiary_fee) in [
            (Some(25), 157_500_000_000u64, 52_500_000_000u64),
            (Some(0), 210_000_000_000u64, 0),
            (None, 0, 0),
        ] {
            let header = etna_header(percentage.unwrap_or_default());
            let mut env = config.evm_env(&header).unwrap();
            if percentage.is_none() {
                // Raw standalone environments have no authoritative header fee data.
                env.block_env = env.block_env.inner.clone().into();
            }
            let treasury = get_treasury_address(env.cfg_env.chain_id);
            let mut db = InMemoryDB::default();
            db.insert_account_info(caller, AccountInfo { balance, ..Default::default() });
            let mut direct_env = env.clone();
            direct_env.block_env.base_fee_share_pctg = None;
            let mut direct = TaikoEvmFactory.create_evm(db.clone(), direct_env);
            if let Some(percentage) = percentage {
                direct.set_block_fee_context(u64::from(percentage));
            }
            // Mirror the trace topology: pre-execution initializes one EVM, then replay
            // builds an inspected EVM from an earlier clone of the block environment.
            let replay_env = env.clone();
            let mut pre_execution = TaikoEvmFactory.create_evm(db, env);
            if let Some(percentage) = percentage {
                pre_execution.set_block_fee_context(u64::from(percentage));
            }
            let (db, _) = pre_execution.finish();
            let mut replay =
                TaikoEvmFactory.create_evm_with_inspector(db, replay_env, NoOpInspector {});
            let tx = TxEnv::builder()
                .caller(caller)
                .to(Address::with_last_byte(0xB0))
                .gas_limit(21_000)
                .gas_price(10_000_000)
                .chain_id(None)
                .build()
                .unwrap();
            let expected = direct.transact(tx.clone()).unwrap();
            let actual = replay.transact(tx).unwrap();
            assert_eq!(actual, expected);
            assert!(actual.result.is_success());
            assert_eq!(actual.result.tx_gas_used(), 21_000);
            assert_eq!(
                actual.state[&caller].info.balance,
                balance - U256::from(210_000_000_000u64)
            );
            assert_eq!(
                actual.state.get(&treasury).map_or(U256::ZERO, |a| a.info.balance),
                U256::from(treasury_fee)
            );
            assert_eq!(actual.state[&header.beneficiary].info.balance, U256::from(beneficiary_fee));
        }
    }

    #[test]
    fn unzen_takes_precedence_over_shasta() {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Shasta, ForkCondition::Timestamp(0));
        chain_spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(0));

        let selected = taiko_spec_by_timestamp_and_block_number(&chain_spec, 0, 1);
        assert_eq!(selected, TaikoSpecId::UNZEN);
    }

    #[test]
    fn etna_takes_precedence_over_unzen() {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(0));
        chain_spec.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(10));

        assert_eq!(taiko_spec_by_timestamp_and_block_number(&chain_spec, 9, 1), TaikoSpecId::UNZEN);
        assert_eq!(taiko_spec_by_timestamp_and_block_number(&chain_spec, 10, 1), TaikoSpecId::ETNA);
    }

    #[test]
    fn shasta_remains_active_before_unzen_timestamp() {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Shasta, ForkCondition::Timestamp(0));
        chain_spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(10));

        let selected = taiko_spec_by_timestamp_and_block_number(&chain_spec, 0, 1);
        assert_eq!(selected, TaikoSpecId::SHASTA);
    }

    #[test]
    fn unzen_evm_env_sets_zero_blob_excess_gas_and_price() {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Shasta, ForkCondition::Timestamp(0));
        chain_spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(0));

        let config = TaikoEvmConfig::new(Arc::new(chain_spec));
        let header =
            Header { number: 1, timestamp: 0, base_fee_per_gas: Some(1), ..Header::default() };

        let env = config.evm_env(&header).expect("unzen env should build");
        let blob_env = env
            .block_env
            .blob_excess_gas_and_price
            .expect("unzen historical env should define blob gas pricing");

        assert_eq!(blob_env.excess_blob_gas, 0);
        assert_eq!(blob_env.blob_gasprice, 1);
    }

    #[test]
    fn pre_unzen_normalization_discards_supplied_parent_beacon_block_root() {
        assert_eq!(
            normalize_parent_beacon_block_root(false, false, 1, Some(B256::repeat_byte(0x11)))
                .unwrap(),
            None
        );
    }

    #[test]
    fn unzen_normalization_preserves_supplied_parent_beacon_block_root() {
        let root = B256::repeat_byte(0x22);
        assert_eq!(
            normalize_parent_beacon_block_root(true, false, 1, Some(root)).unwrap(),
            Some(root)
        );
    }

    #[test]
    fn unzen_normalization_falls_back_to_zero_root_when_missing() {
        assert_eq!(
            normalize_parent_beacon_block_root(true, false, 1, None).unwrap(),
            Some(B256::ZERO)
        );
    }

    #[cfg(feature = "net")]
    #[test]
    fn pending_env_preserves_beacon_root_override_for_unzen_context() {
        let root = B256::repeat_byte(0x33);
        let parent = SealedHeader::seal_slow(Header {
            number: 1,
            timestamp: 1,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1),
            ..Header::default()
        });
        let overrides =
            alloy_rpc_types_eth::BlockOverrides { beacon_root: Some(root), ..Default::default() };
        let attributes = TaikoNextBlockEnvAttributes::build_pending_env(&parent, Some(&overrides));
        let config = config_with_unzen_at(0);

        let ctx = config
            .context_for_next_block(&parent, attributes)
            .expect("pending Unzen context should build");

        assert_eq!(ctx.parent_beacon_block_root, Some(root));
    }

    #[cfg(feature = "net")]
    mod payload_ctx {
        use super::*;
        use alethia_reth_primitives::engine::types::{
            TaikoExecutionDataSidecar, TaikoExecutionPayloadV1,
        };
        use alloy_primitives::Bloom;

        fn sample_payload(header_difficulty: Option<U256>) -> TaikoExecutionData {
            TaikoExecutionData {
                execution_payload: TaikoExecutionPayloadV1 {
                    parent_hash: B256::ZERO,
                    fee_recipient: Address::ZERO,
                    state_root: B256::ZERO,
                    receipts_root: B256::ZERO,
                    logs_bloom: Bloom::ZERO,
                    prev_randao: B256::ZERO,
                    block_number: 1,
                    gas_limit: 30_000_000,
                    gas_used: 0,
                    timestamp: 1,
                    extra_data: Bytes::new(),
                    base_fee_per_gas: U256::from(1_u64),
                    block_hash: B256::ZERO,
                    transactions: Some(vec![]),
                },
                taiko_sidecar: TaikoExecutionDataSidecar {
                    tx_hash: B256::ZERO,
                    withdrawals_hash: None,
                    header_difficulty,
                    taiko_block: Some(true),
                    block_access_list: None,
                    slot_number: None,
                    osaka: None,
                },
            }
        }

        #[test]
        fn payload_root_follows_etna_activation_without_changing_legacy_execution() {
            use alethia_reth_primitives::engine::types::TaikoOsakaPayloadFields;
            let mut spec = (*TAIKO_DEVNET).as_ref().clone();
            spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(50));
            spec.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(100));
            let config = TaikoEvmConfig::new(Arc::new(spec));
            let root = B256::with_last_byte(7);
            let mut payload = sample_payload(Some(U256::ZERO));
            payload.taiko_sidecar.osaka = Some(TaikoOsakaPayloadFields {
                parent_beacon_block_root: root,
                withdrawals: vec![],
                blob_gas_used: 0,
                excess_blob_gas: 0,
                expected_blob_versioned_hashes: vec![],
                execution_requests: vec![],
            });
            for (timestamp, expected) in
                [(49, None), (50, Some(B256::ZERO)), (99, Some(B256::ZERO)), (100, Some(root))]
            {
                payload.execution_payload.timestamp = timestamp;
                let context = config.context_for_payload(&payload).unwrap();
                assert_eq!(context.parent_beacon_block_root, expected, "timestamp {timestamp}");
            }
        }

        #[test]
        fn etna_payload_context_requires_nonzero_root() {
            use alethia_reth_primitives::engine::types::TaikoOsakaPayloadFields;
            let config = config_with_etna_at(0);
            for root in [None, Some(B256::ZERO), Some(B256::with_last_byte(7))] {
                let mut payload = sample_payload(Some(U256::ZERO));
                payload.execution_payload.extra_data = vec![0; 13].into();
                payload.taiko_sidecar.osaka = root.map(|root| TaikoOsakaPayloadFields {
                    parent_beacon_block_root: root,
                    withdrawals: vec![],
                    blob_gas_used: 0,
                    excess_blob_gas: 0,
                    expected_blob_versioned_hashes: vec![],
                    execution_requests: vec![],
                });
                let result = config.context_for_payload(&payload);
                assert_eq!(
                    result.is_ok(),
                    root.is_some_and(|r| !r.is_zero()),
                    "payload root {root:?}"
                );
                if let Ok(ctx) = result {
                    assert_eq!(ctx.parent_beacon_block_root, root);
                }
                payload.execution_payload.block_number = 0;
                assert!(config.context_for_payload(&payload).is_ok());
            }
        }

        #[test]
        fn etna_payload_environment_uses_payload_fee_percentage() {
            let config = config_with_etna_at(0);
            let mut payload = sample_payload(Some(U256::ZERO));
            payload.execution_payload.extra_data =
                vec![25, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1].into();
            assert_eq!(
                config.evm_env_for_payload(&payload).unwrap().block_env.base_fee_share_pctg,
                Some(25)
            );
            payload.execution_payload.extra_data = vec![25, 0, 0, 0, 0, 0, 1].into();
            assert!(config.evm_env_for_payload(&payload).is_err());
        }

        #[test]
        fn unzen_payload_ctx_expects_sidecar_header_difficulty() {
            let config = config_with_unzen_at(0);
            let payload = sample_payload(Some(U256::from(42_u64)));

            let ctx = config.context_for_payload(&payload).expect("payload ctx should build");

            assert_eq!(ctx.expected_difficulty, Some(U256::from(42_u64)));
        }

        #[test]
        fn unzen_payload_ctx_rejects_missing_header_difficulty() {
            let config = config_with_unzen_at(0);
            let payload = sample_payload(None);

            let err = config
                .context_for_payload(&payload)
                .expect_err("Unzen payload ctx must require the sidecar header difficulty");

            assert!(err.to_string().contains("missing header difficulty"));
        }

        #[test]
        fn pre_unzen_payload_ctx_has_no_expected_difficulty() {
            let config = config_with_unzen_at(100);
            let payload = sample_payload(Some(U256::from(42_u64)));

            let ctx = config.context_for_payload(&payload).expect("payload ctx should build");

            assert_eq!(ctx.expected_difficulty, None);
        }
    }
}
