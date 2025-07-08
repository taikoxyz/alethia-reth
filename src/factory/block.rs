use std::{borrow::Cow, sync::Arc};

use alloy_consensus::{Header, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_evm::{
    Database, EvmFactory, FromRecoveredTx, FromTxWithEncoded,
    block::{BlockExecutorFactory, BlockExecutorFor},
    eth::{
        EthBlockExecutionCtx, EthBlockExecutor, receipt_builder::ReceiptBuilder,
        spec::EthExecutorSpec,
    },
};
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_eth::Withdrawals;
use reth::{
    primitives::Log,
    revm::{Inspector, State},
};
use reth_ethereum::primitives::Transaction;
use reth_evm_ethereum::RethReceiptBuilder;

use crate::{chainspec::spec::TaikoChainSpec, factory::factory::TaikoEvmFactory};

/// Context for Ethereum block execution.
#[derive(Debug, Clone)]
pub struct TaikoBlockExecutionCtx<'a> {
    /// Parent block hash.
    pub parent_hash: B256,
    /// Parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// Block ommers
    pub ommers: &'a [Header],
    /// Block withdrawals.
    pub withdrawals: Option<Cow<'a, Withdrawals>>,
    /// Block base fee per gas.
    pub basefee_per_gas: u64,
    /// Block extra data.
    pub extra_data: Bytes,
}

/// Taiko block executor factory.
#[derive(Debug, Clone, Default)]
pub struct TaikoBlockExecutorFactory<
    R = RethReceiptBuilder,
    Spec = Arc<TaikoChainSpec>,
    EvmFactory = TaikoEvmFactory,
> {
    /// Receipt builder.
    receipt_builder: R,
    /// Chain specification.
    spec: Spec,
    /// EVM factory.
    evm_factory: EvmFactory,
}

impl<R, Spec, EvmFactory> TaikoBlockExecutorFactory<R, Spec, EvmFactory> {
    /// Creates a new [`EthBlockExecutorFactory`] with the given spec, [`EvmFactory`], and
    /// [`ReceiptBuilder`].
    pub const fn new(receipt_builder: R, spec: Spec, evm_factory: EvmFactory) -> Self {
        Self {
            receipt_builder,
            spec,
            evm_factory,
        }
    }

    /// Exposes the receipt builder.
    pub const fn receipt_builder(&self) -> &R {
        &self.receipt_builder
    }

    /// Exposes the chain specification.
    pub const fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Exposes the EVM factory.
    pub const fn evm_factory(&self) -> &EvmFactory {
        &self.evm_factory
    }
}

impl<R, Spec, EvmF> BlockExecutorFactory for TaikoBlockExecutorFactory<R, Spec, EvmF>
where
    R: ReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt<Log = Log>>,
    Spec: EthExecutorSpec,
    EvmF: EvmFactory<Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>>,
    Self: 'static,
{
    type EvmFactory = EvmF;
    type ExecutionCtx<'a> = TaikoBlockExecutionCtx<'a>;
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: EvmF::Evm<&'a mut State<DB>, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: Database + 'a,
        I: Inspector<EvmF::Context<&'a mut State<DB>>> + 'a,
    {
        // TODO: Handle the `basefee_per_gas` and `extra_data` in the context.
        EthBlockExecutor::new(
            evm,
            EthBlockExecutionCtx {
                parent_hash: ctx.parent_hash,
                parent_beacon_block_root: ctx.parent_beacon_block_root,
                ommers: ctx.ommers,
                withdrawals: ctx.withdrawals,
            },
            &self.spec,
            &self.receipt_builder,
        )
    }
}
