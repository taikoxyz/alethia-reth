//! Transaction pool that refuses transactions no Etna block can carry.
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use alethia_reth_block::assembler::etna_max_tx_length;
use alethia_reth_chainspec::{hardfork::TaikoHardforks, spec::TaikoChainSpec};
use alethia_reth_primitives::etna::etna_tx_length_of_encoding;
use alloy_eips::{eip7840::BlobParams, merge::EPOCH_SLOTS};
use futures_util::StreamExt;
use reth::{
    chainspec::EthChainSpec,
    transaction_pool::{
        CoinbaseTipOrdering, EthPooledTransaction, EthTransactionValidator, Pool, PoolTransaction,
        TransactionOrigin, TransactionPool, TransactionValidationOutcome,
        TransactionValidationTaskExecutor, TransactionValidator, blobstore::DiskFileBlobStore,
        error::InvalidPoolTransactionError,
    },
};
use reth_ethereum::EthPrimitives;
use reth_evm::ConfigureEvm;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{
    BuilderContext,
    components::{PoolBuilder, TxPoolBuilder, create_blob_store_with_cache},
};
use reth_primitives_traits::{AlloyBlockHeader, SealedBlock};
use reth_provider::{CanonStateNotificationStream, CanonStateSubscriptions};
use tracing::{debug, info};

/// Builds reth's Ethereum transaction pool with [`EtnaTxSizeValidator`] around its validator.
///
/// Everything else matches reth's `EthereumPoolBuilder`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TaikoPoolBuilder;

impl<Node, Evm> PoolBuilder<Node, Evm> for TaikoPoolBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = TaikoChainSpec, Primitives = EthPrimitives>>,
    Evm: ConfigureEvm<Primitives = EthPrimitives> + Clone + 'static,
{
    /// reth's Ethereum pool, validated by [`EtnaTxSizeValidator`] first.
    type Pool = Pool<
        TransactionValidationTaskExecutor<
            EtnaTxSizeValidator<EthTransactionValidator<Node::Provider, EthPooledTransaction, Evm>>,
        >,
        CoinbaseTipOrdering<EthPooledTransaction>,
        DiskFileBlobStore,
    >;

    /// Builds the pool as reth's `EthereumPoolBuilder` does, wrapping its validator.
    async fn build_pool(
        self,
        ctx: &BuilderContext<Node>,
        evm_config: Evm,
    ) -> eyre::Result<Self::Pool> {
        let pool_config = ctx.pool_config();

        let blobs_disabled = ctx.config().txpool.disable_blobs_support ||
            ctx.config().txpool.blobpool_max_count == 0;

        let blob_cache_size = if let Some(blob_cache_size) = pool_config.blob_cache_size {
            Some(blob_cache_size)
        } else {
            let current_timestamp =
                SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
            let blob_params = ctx
                .chain_spec()
                .blob_params_at_timestamp(current_timestamp)
                .unwrap_or_else(BlobParams::cancun);
            Some((blob_params.target_blob_count * EPOCH_SLOTS * 2) as u32)
        };

        let blob_store = create_blob_store_with_cache(ctx, blob_cache_size)?;

        let validator =
            TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone(), evm_config)
                .set_eip4844(!blobs_disabled)
                .kzg_settings(ctx.kzg_settings()?)
                .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
                .with_local_transactions_config(pool_config.local_transactions_config.clone())
                .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
                .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
                .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
                .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
                .build_with_tasks(ctx.task_executor().clone(), blob_store.clone())
                .map(|inner| {
                    EtnaTxSizeValidator::new(inner, ctx.chain_spec(), ctx.head().timestamp)
                });

        if validator.validator().inner.eip4844() {
            // Initializing the KZG settings is expensive, so it runs in the background before the
            // first blob transaction needs it.
            let kzg_settings = validator.validator().inner.kzg_settings().clone();
            ctx.task_executor().spawn_blocking_task(async move {
                let _ = kzg_settings.get();
                debug!(target: "reth::cli", "Initialized KZG settings");
            });
        }

        let transaction_pool = TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build_and_spawn_maintenance_task(blob_store, pool_config)?;

        info!(target: "reth::cli", "Transaction pool initialized");
        debug!(target: "reth::cli", "Spawned txpool maintenance task");

        ctx.task_executor().spawn_task(remove_oversized_at_etna(
            transaction_pool.clone(),
            ctx.provider().canonical_state_stream(),
            ctx.chain_spec(),
            ctx.head().timestamp,
        ));

        Ok(transaction_pool)
    }
}

/// Each time the canonical head moves from a pre-Etna block to an Etna block, removes the pooled
/// transactions no Etna block can carry: [`EtnaTxSizeValidator`] admitted them before Etna, and
/// from then on it refuses them.
///
/// Only those transactions leave the pool; a sender's later nonces stay queued behind the gap,
/// where a replacement can fill it.
async fn remove_oversized_at_etna<Pool: TransactionPool>(
    pool: Pool,
    mut heads: CanonStateNotificationStream,
    chain_spec: Arc<TaikoChainSpec>,
    head_timestamp: u64,
) {
    let max_tx_length = etna_max_tx_length();
    let mut was_etna = chain_spec.is_etna_active(head_timestamp);
    while let Some(notification) = heads.next().await {
        let is_etna = chain_spec.is_etna_active(notification.tip().timestamp());
        if is_etna && !was_etna {
            let oversized: Vec<_> = pool
                .all_transactions()
                .iter()
                .filter(|tx| etna_tx_length_of_encoding(tx.encoded_length()) as u64 > max_tx_length)
                .map(|tx| *tx.hash())
                .collect();
            if !oversized.is_empty() {
                info!(target: "txpool", count = oversized.len(), "removing transactions no Etna block can carry");
                pool.remove_transactions(oversized);
            }
        }
        was_etna = is_etna;
    }
}

/// Pool validator that, once the head block is an Etna block, refuses every transaction whose
/// [`etna_tx_length`](alethia_reth_primitives::etna::etna_tx_length) exceeds
/// [`etna_max_tx_length`], which no Etna block can carry, and otherwise defers to `inner`.
///
/// Such a transaction would otherwise stay in the pool for good: every Etna build leaves it out,
/// which neither evicts it nor frees its sender's later nonces.
#[derive(Debug)]
pub struct EtnaTxSizeValidator<V> {
    /// The validator every transaction that fits goes on to.
    inner: V,
    /// Chain spec that decides whether the head block is an Etna block.
    chain_spec: Arc<TaikoChainSpec>,
    /// Timestamp of the latest head block, in Unix seconds.
    head_timestamp: AtomicU64,
    /// [`etna_max_tx_length`], computed once.
    max_tx_length: u64,
}

impl<V> EtnaTxSizeValidator<V> {
    /// Wraps `inner`, starting from a head block at `head_timestamp`.
    pub fn new(inner: V, chain_spec: Arc<TaikoChainSpec>, head_timestamp: u64) -> Self {
        Self {
            inner,
            chain_spec,
            head_timestamp: AtomicU64::new(head_timestamp),
            max_tx_length: etna_max_tx_length(),
        }
    }

    /// Returns the rejection for `transaction` when the head is an Etna block and no Etna block
    /// can carry it.
    fn oversized(&self, transaction: &impl PoolTransaction) -> Option<InvalidPoolTransactionError> {
        if !self.chain_spec.is_etna_active(self.head_timestamp.load(Ordering::Relaxed)) {
            return None;
        }
        let size = etna_tx_length_of_encoding(transaction.encoded_length());
        (size as u64 > self.max_tx_length).then(|| InvalidPoolTransactionError::OversizedData {
            size,
            limit: self.max_tx_length as usize,
        })
    }
}

impl<V: TransactionValidator> TransactionValidator for EtnaTxSizeValidator<V> {
    /// The inner validator's transaction type.
    type Transaction = V::Transaction;
    /// The inner validator's block type.
    type Block = V::Block;

    /// Refuses a transaction no Etna block can carry, then defers to the inner validator.
    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        if let Some(error) = self.oversized(&transaction) {
            return TransactionValidationOutcome::Invalid(transaction, error);
        }
        self.inner.validate_transaction(origin, transaction).await
    }

    /// Refuses the transactions no Etna block can carry and passes the rest to the inner
    /// validator as one batch, keeping the input order.
    async fn validate_transactions(
        &self,
        transactions: impl IntoIterator<Item = (TransactionOrigin, Self::Transaction), IntoIter: Send>
        + Send,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        let mut outcomes = Vec::new();
        let mut fitting = Vec::new();
        for (origin, transaction) in transactions {
            match self.oversized(&transaction) {
                Some(error) => {
                    outcomes.push(Some(TransactionValidationOutcome::Invalid(transaction, error)))
                }
                None => {
                    outcomes.push(None);
                    fitting.push((origin, transaction));
                }
            }
        }
        let mut validated = self.inner.validate_transactions(fitting).await.into_iter();
        outcomes
            .into_iter()
            .map(|outcome| {
                outcome.unwrap_or_else(|| {
                    validated.next().expect("the inner validator answers every transaction")
                })
            })
            .collect()
    }

    /// Records the new head's timestamp and forwards the block to the inner validator.
    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        self.inner.on_new_head_block(new_tip_block);
        self.head_timestamp.store(new_tip_block.header().timestamp(), Ordering::Relaxed);
    }
}
