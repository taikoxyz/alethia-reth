use std::{borrow::Cow, convert::Infallible, sync::Arc};

use alloy_consensus::{BlockHeader, Header};
use alloy_evm::Database;
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::Withdrawals;
use reth::{
    primitives::{BlockTy, SealedBlock, SealedHeader},
    revm::{
        context::{BlockEnv, CfgEnv},
        primitives::{Address, B256, U256},
    },
};
use reth_ethereum::EthPrimitives;
use reth_evm::{ConfigureEvm, EvmEnv, EvmEnvFor, EvmFactory, EvmFor};
use reth_evm_ethereum::{RethReceiptBuilder, revm_spec, revm_spec_by_timestamp_and_block_number};

use crate::{
    block::{
        assembler::TaikoBlockAssembler,
        factory::{TaikoBlockExecutionCtx, TaikoBlockExecutorFactory},
    },
    chainspec::spec::TaikoChainSpec,
    evm::factory::TaikoEvmFactory,
};

/// A complete configuration of EVM for Taiko network.
#[derive(Debug, Clone)]
pub struct TaikoEvmConfig {
    pub executor_factory: TaikoBlockExecutorFactory,
    pub block_assembler: TaikoBlockAssembler,
    pub evm_factory: TaikoEvmFactory,
}

impl TaikoEvmConfig {
    /// Creates a new Taiko EVM configuration with the given chain spec and extra context.
    pub fn new(chain_spec: Arc<TaikoChainSpec>) -> Self {
        Self::new_with_evm_factory(chain_spec, TaikoEvmFactory::default())
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

impl ConfigureEvm for TaikoEvmConfig {
    /// The primitives type used by the EVM.
    type Primitives = EthPrimitives;
    /// The error type that is returned by [`Self::next_evm_env`].
    type Error = Infallible;
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
    fn evm_env(&self, header: &Header) -> EvmEnvFor<Self> {
        let cfg_env = CfgEnv::new()
            .with_chain_id(self.chain_spec().inner.chain().id())
            .with_spec(revm_spec(&self.chain_spec().inner, header));

        let block_env = BlockEnv {
            number: U256::from(header.number()),
            beneficiary: header.beneficiary(),
            timestamp: U256::from(header.timestamp()),
            difficulty: U256::ZERO,
            prevrandao: header.mix_hash(),
            gas_limit: header.gas_limit(),
            basefee: header.base_fee_per_gas().unwrap(),
            blob_excess_gas_and_price: None,
        };

        EvmEnv { cfg_env, block_env }
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
        let cfg = CfgEnv::new()
            .with_chain_id(self.chain_spec().inner.chain().id())
            .with_spec(revm_spec_by_timestamp_and_block_number(
                &self.chain_spec().inner,
                attributes.timestamp,
                parent.number + 1,
            ));

        let block_env: BlockEnv = BlockEnv {
            number: U256::from(parent.number + 1),
            beneficiary: attributes.suggested_fee_recipient,
            timestamp: U256::from(attributes.timestamp),
            difficulty: U256::ZERO,
            prevrandao: Some(attributes.prev_randao),
            gas_limit: attributes.gas_limit,
            basefee: attributes.base_fee_per_gas,
            blob_excess_gas_and_price: None,
        };

        Ok((cfg, block_env).into())
    }

    /// Returns the configured [`BlockExecutorFactory::ExecutionCtx`] for a given block.
    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<BlockTy<Self::Primitives>>,
    ) -> reth_evm::ExecutionCtxFor<'a, Self> {
        TaikoBlockExecutionCtx {
            parent_hash: block.header().parent_hash,
            parent_beacon_block_root: block.header().parent_beacon_block_root,
            ommers: &block.body().ommers,
            withdrawals: block.body().withdrawals.as_ref().map(Cow::Borrowed),
            basefee_per_gas: block.header().base_fee_per_gas.unwrap_or_default(),
            extra_data: block.header().extra_data.clone(),
        }
    }

    /// Returns the configured [`BlockExecutorFactory::ExecutionCtx`] for `parent + 1`
    /// block.
    fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        ctx: Self::NextBlockEnvCtx,
    ) -> reth_evm::ExecutionCtxFor<'_, Self> {
        TaikoBlockExecutionCtx {
            parent_hash: parent.hash(),
            parent_beacon_block_root: None,
            ommers: &[],
            withdrawals: Some(Cow::Owned(Withdrawals::new(vec![]))),
            basefee_per_gas: ctx.base_fee_per_gas,
            extra_data: ctx.extra_data.into(),
        }
    }

    /// Returns a new EVM with the given database configured with the given environment settings,
    /// including the spec id and transaction environment.
    fn evm_with_env<DB: Database>(&self, db: DB, evm_env: EvmEnvFor<Self>) -> EvmFor<Self, DB> {
        self.evm_factory().create_evm(db, evm_env)
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
