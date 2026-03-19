use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use alloy_consensus::Header;
use alloy_evm::{
    Database,
    block::{BlockExecutorFactory, BlockExecutorFor},
    eth::receipt_builder::ReceiptBuilder,
};
use alloy_primitives::{B256, Bytes, U256};
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
    /// Whether Uzen zk gas rules are active for this block.
    pub is_uzen: bool,
    /// Imported-header difficulty expected after recomputing finalized Uzen zk gas.
    pub expected_difficulty: Option<U256>,
    /// Finalized block zk gas accumulated from fully committed Uzen transactions.
    pub finalized_block_zk_gas: Arc<AtomicU64>,
}

impl<'a> TaikoBlockExecutionCtx<'a> {
    /// Stores the finalized block zk gas value that should be written into the Uzen header.
    pub fn set_finalized_block_zk_gas(&self, zk_gas: u64) {
        self.finalized_block_zk_gas.store(zk_gas, Ordering::Relaxed);
    }

    /// Returns the finalized block zk gas accumulated from fully committed transactions.
    pub fn finalized_block_zk_gas(&self) -> u64 {
        self.finalized_block_zk_gas.load(Ordering::Relaxed)
    }

    /// Returns the imported-header difficulty expected during Uzen validation, if any.
    pub fn expected_difficulty(&self) -> Option<U256> {
        self.expected_difficulty
    }
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

impl<Spec> BlockExecutorFactory
    for TaikoBlockExecutorFactory<RethReceiptBuilder, Spec, TaikoEvmFactory>
where
    Spec: TaikoExecutorSpec + Clone,
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
        evm: <TaikoEvmFactory as alloy_evm::EvmFactory>::Evm<&'a mut State<DB>, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: Database + 'a,
        I: Inspector<<TaikoEvmFactory as alloy_evm::EvmFactory>::Context<&'a mut State<DB>>> + 'a,
    {
        TaikoBlockExecutor::new(evm, ctx, self.spec.clone(), &self.receipt_builder)
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
        let evm_factory = TaikoEvmFactory;

        TaikoBlockExecutorFactory::new(receipt_builder, spec.clone(), evm_factory);
    }
}
