use std::{borrow::Cow, convert::Infallible, sync::Arc};

use alloy_consensus::{BlockHeader, Header};
use alloy_eips::eip1559::INITIAL_BASE_FEE;
use reth::{
    chainspec::ChainSpec,
    primitives::{BlockTy, SealedBlock, SealedHeader},
    revm::{
        context::{BlockEnv, CfgEnv},
        primitives::U256,
    },
};
use reth_ethereum::EthPrimitives;
use reth_evm::{
    ConfigureEvm, EvmEnv, EvmEnvFor, NextBlockEnvAttributes, eth::EthBlockExecutionCtx,
};
use reth_evm_ethereum::{RethReceiptBuilder, revm_spec};

use crate::{
    evm::evm::TaikoEvmExtraContext,
    factory::{
        assembler::TaikoBlockAssembler, block::TaikoBlockExecutorFactory, factory::TaikoEvmFactory,
    },
};

#[derive(Debug, Clone)]
pub struct TaikoEvmConfig {
    pub executor_factory: TaikoBlockExecutorFactory,
    pub block_assembler: TaikoBlockAssembler,
    pub evm_factory: TaikoEvmFactory,
}

impl TaikoEvmConfig {
    pub fn new(chain_spec: Arc<ChainSpec>, extra_context: TaikoEvmExtraContext) -> Self {
        let evm_factory = TaikoEvmFactory::new(extra_context);
        Self::new_with_evm_factory(chain_spec, evm_factory)
    }

    /// Creates a new Ethereum EVM configuration with the given chain spec and EVM factory.
    pub fn new_with_evm_factory(chain_spec: Arc<ChainSpec>, evm_factory: TaikoEvmFactory) -> Self {
        Self {
            block_assembler: TaikoBlockAssembler::new(chain_spec.clone()),
            executor_factory: TaikoBlockExecutorFactory::new(
                RethReceiptBuilder::default(),
                chain_spec,
                evm_factory,
            ),
            evm_factory: evm_factory,
        }
    }

    /// Returns the chain spec associated with this configuration.
    pub const fn chain_spec(&self) -> &Arc<ChainSpec> {
        self.executor_factory.spec()
    }
}

impl ConfigureEvm for TaikoEvmConfig {
    type Primitives = EthPrimitives;
    type Error = Infallible;
    type NextBlockEnvCtx = NextBlockEnvAttributes;
    type BlockExecutorFactory =
        TaikoBlockExecutorFactory<RethReceiptBuilder, Arc<ChainSpec>, TaikoEvmFactory>;
    type BlockAssembler = TaikoBlockAssembler;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.block_assembler
    }

    fn evm_env(&self, header: &Header) -> EvmEnvFor<Self> {
        // configure evm env based on parent block
        let cfg_env = CfgEnv::new()
            .with_chain_id(self.chain_spec().chain().id())
            .with_spec(revm_spec(self.chain_spec(), header));

        let block_env = BlockEnv {
            number: header.number(),
            beneficiary: header.beneficiary(),
            timestamp: header.timestamp(),
            difficulty: U256::ZERO,
            prevrandao: header.mix_hash(),
            gas_limit: header.gas_limit(),
            basefee: header.base_fee_per_gas().unwrap_or_default(),
            blob_excess_gas_and_price: None,
        };

        EvmEnv { cfg_env, block_env }
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        let cfg = CfgEnv::new().with_chain_id(self.chain_spec().chain().id());

        let basefee = Some(INITIAL_BASE_FEE);

        let mut gas_limit = attributes.gas_limit;

        // If we are on the London fork boundary, we need to multiply the parent's gas limit by the
        // elasticity multiplier to get the new gas limit.

        let elasticity_multiplier = self
            .chain_spec()
            .base_fee_params_at_timestamp(attributes.timestamp)
            .elasticity_multiplier;

        // multiply the gas limit by the elasticity multiplier
        gas_limit *= elasticity_multiplier as u64;

        // set the base fee to the initial base fee from the EIP-1559 spec

        let block_env = BlockEnv {
            number: parent.number + 1,
            beneficiary: attributes.suggested_fee_recipient,
            timestamp: attributes.timestamp,
            difficulty: U256::ZERO,
            prevrandao: Some(attributes.prev_randao),
            gas_limit,
            // calculate basefee based on parent block's gas usage
            basefee: basefee.unwrap_or_default(),
            // calculate excess gas based on parent block's blob gas usage
            blob_excess_gas_and_price: None,
        };

        Ok((cfg, block_env).into())
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<BlockTy<Self::Primitives>>,
    ) -> reth_evm::ExecutionCtxFor<'a, Self> {
        EthBlockExecutionCtx {
            parent_hash: block.header().parent_hash,
            parent_beacon_block_root: block.header().parent_beacon_block_root,
            ommers: &block.body().ommers,
            withdrawals: block.body().withdrawals.as_ref().map(Cow::Borrowed),
        }
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        attributes: Self::NextBlockEnvCtx,
    ) -> reth_evm::ExecutionCtxFor<'_, Self> {
        EthBlockExecutionCtx {
            parent_hash: parent.hash(),
            parent_beacon_block_root: attributes.parent_beacon_block_root,
            ommers: &[],
            withdrawals: attributes.withdrawals.map(Cow::Owned),
        }
    }
}
