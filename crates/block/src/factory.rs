use std::{borrow::Cow, sync::Arc};

use alloy_consensus::{Header, Transaction, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_evm::{
    Database, EvmFactory, FromRecoveredTx, FromTxWithEncoded,
    block::{BlockExecutorFactory, BlockExecutorFor},
    eth::receipt_builder::ReceiptBuilder,
};
use alloy_primitives::{B256, Bytes, Log};
use alloy_rpc_types_eth::Withdrawals;

use reth_revm::{Inspector, State};

use crate::{executor::TaikoBlockExecutor, receipt_builder::TaikoReceiptBuilder};
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
    R = TaikoReceiptBuilder,
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

impl<R, Spec, EvmF> BlockExecutorFactory for TaikoBlockExecutorFactory<R, Spec, EvmF>
where
    R: ReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt<Log = Log>>,
    Spec: TaikoExecutorSpec,
    EvmF: EvmFactory<Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>>,
    Self: 'static,
{
    /// The EVM factory used by the executor.
    type EvmFactory = EvmF;
    /// Context required for block execution.
    ///
    /// This is similar to [`crate::EvmEnv`], but only contains context unrelated to EVM and
    /// required for execution of an entire block.
    type ExecutionCtx<'a> = TaikoBlockExecutionCtx<'a>;
    /// Transaction type used by the executor.
    type Transaction = R::Transaction;
    /// Receipt type produced by the executor.
    type Receipt = R::Receipt;

    /// Reference to EVM factory used by the executor.
    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }

    /// Creates an Taiko block executor with given EVM and execution context.
    fn create_executor<'a, DB, I>(
        &'a self,
        evm: EvmF::Evm<&'a mut State<DB>, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: Database + 'a,
        I: Inspector<EvmF::Context<&'a mut State<DB>>> + 'a,
    {
        TaikoBlockExecutor::new(evm, ctx, &self.spec, &self.receipt_builder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alethia_reth_evm::factory::TaikoEvmFactory;
    use std::sync::Arc;

    #[test]
    fn test_taiko_block_executor_factory_creation() {
        let receipt_builder = TaikoReceiptBuilder::default();
        let spec = Arc::new(TaikoChainSpec::default());
        let evm_factory = TaikoEvmFactory::default();

        TaikoBlockExecutorFactory::new(receipt_builder, spec.clone(), evm_factory);
    }
}
