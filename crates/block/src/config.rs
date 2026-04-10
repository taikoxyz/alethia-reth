//! Taiko block and EVM configuration used by node and payload services.
use std::{borrow::Cow, sync::Arc};

use alloy_consensus::{BlockHeader, Header};
use alloy_eips::Decodable2718;
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::Withdrawals;
use reth_chainspec::EthChainSpec;
use reth_ethereum_forks::Hardforks;
use reth_ethereum_primitives::EthPrimitives;
#[cfg(feature = "net")]
use reth_evm::ConfigureEngineEvm;
use reth_evm::{ConfigureEvm, EvmEnv, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor};
use reth_evm_ethereum::RethReceiptBuilder;
#[cfg(feature = "net")]
use reth_payload_primitives::ExecutionPayload;
use reth_primitives_traits::{
    BlockTy, SealedBlock, SealedHeader, SignedTransaction, TxTy, constants::MAX_TX_GAS_LIMIT_OSAKA,
};
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
use alethia_reth_evm::{factory::TaikoEvmFactory, spec::TaikoSpecId};
#[cfg(feature = "net")]
use alethia_reth_primitives::engine::types::TaikoExecutionData;

/// Error when base fee is missing from a block header.
#[derive(Debug)]
pub struct MissingBaseFee {
    /// The block number where base fee was missing.
    pub block_number: u64,
}

impl std::fmt::Display for MissingBaseFee {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "missing base_fee_per_gas in block {}", self.block_number)
    }
}

impl std::error::Error for MissingBaseFee {}

/// A complete configuration of EVM for Taiko network.
#[derive(Debug, Clone)]
pub struct TaikoEvmConfig {
    /// Block executor factory configured for Taiko execution rules.
    pub executor_factory: TaikoBlockExecutorFactory,
    /// Block assembler used to construct finalized block objects.
    pub block_assembler: TaikoBlockAssembler,
    /// EVM factory used to instantiate Taiko EVM instances.
    pub evm_factory: TaikoEvmFactory,
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
            block_assembler: TaikoBlockAssembler::new(chain_spec.clone()),
            executor_factory: TaikoBlockExecutorFactory::new(
                RethReceiptBuilder::default(),
                chain_spec,
                evm_factory,
            ),
            evm_factory,
        }
    }

    /// Returns the chain spec associated with this configuration.
    pub const fn chain_spec(&self) -> &Arc<TaikoChainSpec> {
        self.executor_factory.spec()
    }
}

/// Returns the zero blob-gas environment used for Cancun-or-later RPC execution.
fn taiko_blob_excess_gas_and_price(spec: TaikoSpecId) -> Option<BlobExcessGasAndPrice> {
    spec.into_eth_spec()
        .is_enabled_in(SpecId::CANCUN)
        .then_some(BlobExcessGasAndPrice { excess_blob_gas: 0, blob_gasprice: 1 })
}

/// Normalizes the parent beacon block root for Uzen/Cancun execution contexts.
///
/// Uzen activates Cancun semantics, which require a parent beacon block root for block execution.
/// Imported payloads can supply an explicit value, but local next-block building falls back to the
/// zero root when Uzen is active.
fn normalize_parent_beacon_block_root(
    is_uzen_active: bool,
    parent_beacon_block_root: Option<B256>,
) -> Option<B256> {
    if is_uzen_active { parent_beacon_block_root.or(Some(B256::ZERO)) } else { None }
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
            difficulty: if self.chain_spec().is_uzen_active(header.timestamp()) {
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

        Ok((cfg, block_env).into())
    }

    /// Returns the configured [`BlockExecutorFactory::ExecutionCtx`] for a given block.
    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<BlockTy<Self::Primitives>>,
    ) -> Result<reth_evm::ExecutionCtxFor<'a, Self>, Self::Error> {
        let is_uzen_active = self.chain_spec().is_uzen_active(block.header().timestamp);
        let basefee_per_gas = block
            .header()
            .base_fee_per_gas
            .ok_or_else(|| AnyError::new(MissingBaseFee { block_number: block.header().number }))?;
        Ok(TaikoBlockExecutionCtx {
            parent_hash: block.header().parent_hash,
            parent_beacon_block_root: block.header().parent_beacon_block_root,
            ommers: &[],
            withdrawals: Some(Cow::Owned(Withdrawals::new(vec![]))),
            basefee_per_gas,
            extra_data: block.header().extra_data.clone(),
            is_uzen_active,
            expected_difficulty: is_uzen_active.then_some(block.header().difficulty),
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
        let is_uzen_active = self.chain_spec().is_uzen_active(ctx.timestamp);
        Ok(TaikoBlockExecutionCtx {
            parent_hash: parent.hash(),
            parent_beacon_block_root: normalize_parent_beacon_block_root(is_uzen_active, None),
            ommers: &[],
            withdrawals: Some(Cow::Owned(Withdrawals::new(vec![]))),
            basefee_per_gas: ctx.base_fee_per_gas,
            extra_data: ctx.extra_data,
            is_uzen_active,
            expected_difficulty: None,
            finalized_block_zk_gas: Default::default(),
        })
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

        Ok((cfg_env, block_env).into())
    }

    /// Returns an [`ExecutionCtxFor`] for the given payload.
    fn context_for_payload<'a>(
        &self,
        payload: &'a TaikoExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        let is_uzen_active = self.chain_spec().is_uzen_active(payload.timestamp());
        Ok(TaikoBlockExecutionCtx {
            parent_hash: payload.parent_hash(),
            parent_beacon_block_root: normalize_parent_beacon_block_root(
                is_uzen_active,
                payload.parent_beacon_block_root(),
            ),
            ommers: &[],
            withdrawals: payload.withdrawals().map(|w| Cow::Owned(w.clone().into())),
            basefee_per_gas: payload.execution_payload.base_fee_per_gas.saturating_to(),
            extra_data: payload.execution_payload.extra_data.clone(),
            is_uzen_active,
            expected_difficulty: None,
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
    if chain_spec.fork(TaikoHardfork::Uzen).active_at_timestamp(timestamp) {
        TaikoSpecId::UZEN
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
    fn build_pending_env(parent: &SealedHeader<Header>) -> Self {
        Self {
            timestamp: parent.timestamp.saturating_add(12),
            suggested_fee_recipient: parent.beneficiary,
            prev_randao: B256::random(),
            gas_limit: parent.gas_limit,
            extra_data: parent.extra_data.clone(),
            base_fee_per_gas: parent.base_fee_per_gas.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alethia_reth_chainspec::{TAIKO_DEVNET, hardfork::TaikoHardfork};
    use alloy_hardforks::ForkCondition;
    use std::sync::Arc;

    #[test]
    fn uzen_takes_precedence_over_shasta() {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Shasta, ForkCondition::Timestamp(0));
        chain_spec.inner.hardforks.insert(TaikoHardfork::Uzen, ForkCondition::Timestamp(0));

        let selected = taiko_spec_by_timestamp_and_block_number(&chain_spec, 0, 1);
        assert_eq!(selected, TaikoSpecId::UZEN);
    }

    #[test]
    fn shasta_remains_active_before_uzen_timestamp() {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Shasta, ForkCondition::Timestamp(0));
        chain_spec.inner.hardforks.insert(TaikoHardfork::Uzen, ForkCondition::Timestamp(10));

        let selected = taiko_spec_by_timestamp_and_block_number(&chain_spec, 0, 1);
        assert_eq!(selected, TaikoSpecId::SHASTA);
    }

    #[test]
    fn uzen_evm_env_sets_zero_blob_excess_gas_and_price() {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Shasta, ForkCondition::Timestamp(0));
        chain_spec.inner.hardforks.insert(TaikoHardfork::Uzen, ForkCondition::Timestamp(0));

        let config = TaikoEvmConfig::new(Arc::new(chain_spec));
        let header =
            Header { number: 1, timestamp: 0, base_fee_per_gas: Some(1), ..Header::default() };

        let env = config.evm_env(&header).expect("uzen env should build");
        let blob_env = env
            .block_env
            .blob_excess_gas_and_price
            .expect("uzen historical env should define blob gas pricing");

        assert_eq!(blob_env.excess_blob_gas, 0);
        assert_eq!(blob_env.blob_gasprice, 1);
    }

    #[test]
    fn pre_uzen_normalization_discards_supplied_parent_beacon_block_root() {
        assert_eq!(normalize_parent_beacon_block_root(false, Some(B256::repeat_byte(0x11))), None);
    }

    #[test]
    fn uzen_normalization_preserves_supplied_parent_beacon_block_root() {
        let root = B256::repeat_byte(0x22);
        assert_eq!(normalize_parent_beacon_block_root(true, Some(root)), Some(root));
    }

    #[test]
    fn uzen_normalization_falls_back_to_zero_root_when_missing() {
        assert_eq!(normalize_parent_beacon_block_root(true, None), Some(B256::ZERO));
    }
}
