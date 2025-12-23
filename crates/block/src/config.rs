use std::{borrow::Cow, convert::Infallible, sync::Arc};

use alloy_consensus::Header;
use alloy_eips::Decodable2718;
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::Bytes;
use reth_chainspec::EthChainSpec;
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor,
};
use reth_node_api::ExecutionPayload;
use reth_primitives::{BlockTy, SealedBlock, SealedHeader};
use reth_primitives_traits::{SignedTransaction, TxTy, constants::MAX_TX_GAS_LIMIT_OSAKA};
use reth_revm::{
    context::{BlockEnv, CfgEnv},
    primitives::{Address, B256, U256},
};
use reth_rpc_eth_api::helpers::pending_block::BuildPendingEnv;
use reth_storage_errors::any::AnyError;

use alethia_reth_block_core::{config as core, factory::TaikoBlockExecutionCtx};
use alethia_reth_chainspec_core::spec::TaikoChainSpec;
use alethia_reth_evm::factory::TaikoEvmFactory;
use alethia_reth_primitives::engine::types::TaikoExecutionData;

pub use core::{taiko_revm_spec, taiko_spec_by_timestamp_and_block_number};

/// A complete configuration of EVM for Taiko network.
#[derive(Debug, Clone)]
pub struct TaikoEvmConfig {
    inner: core::TaikoEvmConfig,
}

impl TaikoEvmConfig {
    /// Creates a new Taiko EVM configuration with the given chain spec and extra context.
    pub fn new(chain_spec: Arc<TaikoChainSpec>) -> Self {
        Self { inner: core::TaikoEvmConfig::new(chain_spec) }
    }

    /// Creates a new Taiko EVM configuration with the given chain spec and EVM factory.
    pub fn new_with_evm_factory(
        chain_spec: Arc<TaikoChainSpec>,
        evm_factory: TaikoEvmFactory,
    ) -> Self {
        Self { inner: core::TaikoEvmConfig::new_with_evm_factory(chain_spec, evm_factory) }
    }

    /// Returns the chain spec associated with this configuration.
    pub const fn chain_spec(&self) -> &Arc<TaikoChainSpec> {
        self.inner.chain_spec()
    }

    /// Returns a reference to the core config.
    pub const fn core(&self) -> &core::TaikoEvmConfig {
        &self.inner
    }

    /// Returns the core config, consuming `self`.
    pub fn into_core(self) -> core::TaikoEvmConfig {
        self.inner
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

impl From<core::TaikoNextBlockEnvAttributes> for TaikoNextBlockEnvAttributes {
    fn from(value: core::TaikoNextBlockEnvAttributes) -> Self {
        Self {
            timestamp: value.timestamp,
            suggested_fee_recipient: value.suggested_fee_recipient,
            prev_randao: value.prev_randao,
            gas_limit: value.gas_limit,
            extra_data: value.extra_data,
            base_fee_per_gas: value.base_fee_per_gas,
        }
    }
}

impl From<TaikoNextBlockEnvAttributes> for core::TaikoNextBlockEnvAttributes {
    fn from(value: TaikoNextBlockEnvAttributes) -> Self {
        Self {
            timestamp: value.timestamp,
            suggested_fee_recipient: value.suggested_fee_recipient,
            prev_randao: value.prev_randao,
            gas_limit: value.gas_limit,
            extra_data: value.extra_data,
            base_fee_per_gas: value.base_fee_per_gas,
        }
    }
}

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
        let mut cfg_env =
            CfgEnv::new().with_chain_id(self.chain_spec().chain().id()).with_spec(spec);

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
            blob_excess_gas_and_price: None,
        };

        Ok((cfg_env, block_env).into())
    }

    /// Returns an [`ExecutionCtxFor`] for the given payload.
    fn context_for_payload<'a>(
        &self,
        payload: &'a TaikoExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        Ok(TaikoBlockExecutionCtx {
            parent_hash: payload.parent_hash(),
            parent_beacon_block_root: payload.parent_beacon_block_root(),
            ommers: &[],
            withdrawals: payload.withdrawals().map(|w| Cow::Owned(w.clone().into())),
            basefee_per_gas: payload.execution_payload.base_fee_per_gas.saturating_to(),
            extra_data: payload.execution_payload.extra_data.clone(),
        })
    }

    /// Returns an [`ExecutableTxIterator`] for the given payload.
    fn tx_iterator_for_payload(
        &self,
        payload: &TaikoExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        Ok(payload.execution_payload.transactions.clone().unwrap_or_default().into_iter().map(
            |tx| {
                let tx = TxTy::<Self::Primitives>::decode_2718_exact(tx.as_ref())
                    .map_err(AnyError::new)?;
                let signer = tx.try_recover().map_err(AnyError::new)?;
                Ok::<_, AnyError>(tx.with_signer(signer))
            },
        ))
    }
}

impl ConfigureEvm for TaikoEvmConfig {
    /// The primitives type used by the EVM.
    type Primitives = <core::TaikoEvmConfig as ConfigureEvm>::Primitives;
    /// The error type that is returned by [`Self::next_evm_env`].
    type Error = Infallible;
    /// Context required for configuring next block environment.
    type NextBlockEnvCtx = TaikoNextBlockEnvAttributes;
    /// Configured [`BlockExecutorFactory`], contains [`EvmFactory`] internally.
    type BlockExecutorFactory = <core::TaikoEvmConfig as ConfigureEvm>::BlockExecutorFactory;
    /// The assembler to build a Taiko block.
    type BlockAssembler = <core::TaikoEvmConfig as ConfigureEvm>::BlockAssembler;

    /// Returns reference to the configured [`BlockExecutorFactory`].
    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        self.inner.block_executor_factory()
    }

    /// Returns reference to the configured [`BlockAssembler`].
    fn block_assembler(&self) -> &Self::BlockAssembler {
        self.inner.block_assembler()
    }

    /// Creates a new [`EvmEnv`] for the given header.
    fn evm_env(&self, header: &Header) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.evm_env(header)
    }

    /// Returns the configured [`EvmEnv`] for `parent + 1` block.
    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        let core_attributes: core::TaikoNextBlockEnvAttributes = attributes.clone().into();
        self.inner.next_evm_env(parent, &core_attributes)
    }

    /// Returns the configured [`BlockExecutorFactory::ExecutionCtx`] for a given block.
    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<BlockTy<Self::Primitives>>,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        self.inner.context_for_block(block)
    }

    /// Returns the configured [`BlockExecutorFactory::ExecutionCtx`] for `parent + 1` block.
    fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<ExecutionCtxFor<'_, Self>, Self::Error> {
        let core_attributes: core::TaikoNextBlockEnvAttributes = attributes.into();
        self.inner.context_for_next_block(parent, core_attributes)
    }
}

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
