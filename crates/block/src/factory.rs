use std::{borrow::Cow, sync::Arc};

use alloy_consensus::Header;
use alloy_evm::{
    Database, EvmFactory, FromRecoveredTx, FromTxWithEncoded,
    block::{BlockExecutorFactory, BlockExecutorFor},
    eth::receipt_builder::ReceiptBuilder,
};
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_eth::Withdrawals;
use reth_evm_ethereum::RethReceiptBuilder;
use reth_revm::{Inspector, State};

use crate::executor::TaikoBlockExecutor;
use alethia_reth_chainspec::spec::{TaikoChainSpec, TaikoExecutorSpec};
use alethia_reth_evm::factory::TaikoEvmFactory;

/// Context for Taiko block execution.
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
        Self { receipt_builder, spec, evm_factory }
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

// NOTE: Specialized to TaikoEvmFactory + RethReceiptBuilder because TaikoBlockExecutor requires
// ZkGasEvm, which is only implemented by TaikoEvmWrapper.
impl<Spec> BlockExecutorFactory
    for TaikoBlockExecutorFactory<RethReceiptBuilder, Spec, TaikoEvmFactory>
where
    Spec: TaikoExecutorSpec,
    <TaikoEvmFactory as EvmFactory>::Tx: FromRecoveredTx<<RethReceiptBuilder as ReceiptBuilder>::Transaction>
        + FromTxWithEncoded<<RethReceiptBuilder as ReceiptBuilder>::Transaction>,
    Self: 'static,
{
    /// The EVM factory used by the executor.
    type EvmFactory = TaikoEvmFactory;
    /// Context required for block execution.
    ///
    /// This is similar to [`crate::EvmEnv`], but only contains context unrelated to EVM and
    /// required for execution of an entire block.
    type ExecutionCtx<'a> = TaikoBlockExecutionCtx<'a>;
    /// Transaction type used by the executor.
    type Transaction = <RethReceiptBuilder as ReceiptBuilder>::Transaction;
    /// Receipt type produced by the executor.
    type Receipt = <RethReceiptBuilder as ReceiptBuilder>::Receipt;

    /// Reference to EVM factory used by the executor.
    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }

    /// Creates an Taiko block executor with given EVM and execution context.
    fn create_executor<'a, DB, I>(
        &'a self,
        evm: <TaikoEvmFactory as EvmFactory>::Evm<&'a mut State<DB>, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: Database + 'a,
        I: Inspector<<TaikoEvmFactory as EvmFactory>::Context<&'a mut State<DB>>> + 'a,
    {
        TaikoBlockExecutor::new(
            evm,
            TaikoBlockExecutionCtx {
                parent_hash: ctx.parent_hash,
                parent_beacon_block_root: ctx.parent_beacon_block_root,
                ommers: ctx.ommers,
                withdrawals: ctx.withdrawals,
                basefee_per_gas: ctx.basefee_per_gas,
                extra_data: ctx.extra_data,
            },
            &self.spec,
            &self.receipt_builder,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_evm_ethereum::RethReceiptBuilder;
    use std::sync::Arc;

    #[test]
    fn test_taiko_block_executor_factory_creation() {
        let receipt_builder = RethReceiptBuilder::default();
        let spec = Arc::new(TaikoChainSpec::default());
        let evm_factory = TaikoEvmFactory::default();

        TaikoBlockExecutorFactory::new(receipt_builder, spec.clone(), evm_factory);
    }
}
