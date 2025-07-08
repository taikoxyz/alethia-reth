use std::{borrow::Cow, convert::Infallible, sync::Arc};

use crate::{
    chainspec::spec::TaikoChainSpec,
    evm::evm::TaikoEvmExtraContext,
    factory::{
        assembler::TaikoBlockAssembler,
        block::{TaikoBlockExecutionCtx, TaikoBlockExecutorFactory},
        factory::TaikoEvmFactory,
    },
};
use alloy_consensus::{BlockHeader, Header};
use alloy_evm::Database;
use alloy_rlp::Bytes;
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

#[derive(Debug, Clone)]
pub struct TaikoEvmConfig {
    pub executor_factory: TaikoBlockExecutorFactory,
    pub block_assembler: TaikoBlockAssembler,
    pub evm_factory: TaikoEvmFactory,
}

impl TaikoEvmConfig {
    pub fn new(
        chain_spec: Arc<TaikoChainSpec>,
        extra_context: TaikoEvmExtraContext,
        extra_data: Bytes,
    ) -> Self {
        let evm_factory = TaikoEvmFactory::new(extra_context);
        Self::new_with_evm_factory(chain_spec, evm_factory, extra_data)
    }

    /// Creates a new Ethereum EVM configuration with the given chain spec and EVM factory.
    pub fn new_with_evm_factory(
        chain_spec: Arc<TaikoChainSpec>,
        evm_factory: TaikoEvmFactory,
        extra_data: Bytes,
    ) -> Self {
        Self {
            block_assembler: TaikoBlockAssembler::new(chain_spec.clone())
                .with_extra_data(extra_data.into()),
            executor_factory: TaikoBlockExecutorFactory::new(
                RethReceiptBuilder::default(),
                chain_spec,
                evm_factory,
            ),
            evm_factory: evm_factory,
        }
    }

    /// Returns the chain spec associated with this configuration.
    pub const fn chain_spec(&self) -> &Arc<TaikoChainSpec> {
        self.executor_factory.spec()
    }
}

impl ConfigureEvm for TaikoEvmConfig {
    type Primitives = EthPrimitives;
    type Error = Infallible;
    type NextBlockEnvCtx = TaikoNextBlockEnvAttributes;
    type BlockExecutorFactory =
        TaikoBlockExecutorFactory<RethReceiptBuilder, Arc<TaikoChainSpec>, TaikoEvmFactory>;
    type BlockAssembler = TaikoBlockAssembler;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.block_assembler
    }

    fn evm_env(&self, header: &Header) -> EvmEnvFor<Self> {
        let cfg_env = CfgEnv::new()
            .with_chain_id(self.chain_spec().inner.chain().id())
            .with_spec(revm_spec(&self.chain_spec().inner, header));

        let block_env = BlockEnv {
            number: header.number(),
            beneficiary: header.beneficiary(),
            timestamp: header.timestamp(),
            difficulty: U256::ZERO,
            prevrandao: header.mix_hash(),
            gas_limit: header.gas_limit(),
            basefee: header.base_fee_per_gas().unwrap(),
            blob_excess_gas_and_price: None,
        };

        EvmEnv { cfg_env, block_env }
    }

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
            number: parent.number + 1,
            beneficiary: attributes.suggested_fee_recipient,
            timestamp: attributes.timestamp,
            difficulty: U256::ZERO,
            prevrandao: Some(attributes.prev_randao),
            gas_limit: attributes.gas_limit,
            basefee: attributes.base_fee_per_gas,
            blob_excess_gas_and_price: None,
        };

        Ok((cfg, block_env).into())
    }

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

    fn evm_with_env<DB: Database>(&self, db: DB, evm_env: EvmEnvFor<Self>) -> EvmFor<Self, DB> {
        self.evm_factory().create_evm(db, evm_env)
    }

    // fn create_block_builder<'a, DB, I>(
    //     &'a self,
    //     evm: EvmFor<Self, &'a mut State<DB>, I>,
    //     parent: &'a SealedHeader<HeaderTy<Self::Primitives>>,
    //     ctx: <Self::BlockExecutorFactory as BlockExecutorFactory>::ExecutionCtx<'a>,
    // ) -> impl BlockBuilder<
    //     Primitives = Self::Primitives,
    //     Executor: BlockExecutorFor<'a, Self::BlockExecutorFactory, DB, I>,
    // >
    // where
    //     DB: Database,
    //     I: InspectorFor<Self, &'a mut State<DB>> + 'a,
    // {
    //     BasicBlockBuilder {
    //         executor: self.create_executor(evm, ctx.clone()),
    //         ctx,
    //         assembler: self.block_assembler(),
    //         parent,
    //         transactions: Vec::new(),
    //     }
    // }
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
