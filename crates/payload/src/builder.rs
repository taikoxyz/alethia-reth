use std::{convert::Infallible, sync::Arc};

use alloy_consensus::Transaction;
use alloy_eips::eip4844::BYTES_PER_BLOB;
use reth::{
    api::{PayloadBuilderAttributes, PayloadBuilderError},
    providers::{ChainSpecProvider, StateProviderFactory},
    revm::{State, cancelled::CancelOnDrop, database::StateProviderDatabase, primitives::U256},
};
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_ethereum::{EthPrimitives, TransactionSigned as EthTransactionSigned};
use reth_ethereum_engine_primitives::EthBuiltPayload;
use reth_evm::{
    ConfigureEvm,
    block::{BlockExecutionError, BlockValidationError},
    execute::{BlockBuilder, BlockBuilderOutcome},
};
use reth_evm_ethereum::RethReceiptBuilder;
use reth_primitives::{Header as RethHeader, Recovered};
use tracing::{debug, trace, warn};

use alethia_reth_block::{
    assembler::TaikoBlockAssembler,
    config::{TaikoEvmConfig, TaikoNextBlockEnvAttributes},
    factory::TaikoBlockExecutorFactory,
    tx_selection::{SelectionOutcome, TxSelectionConfig, select_and_execute_pool_transactions},
};
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_consensus::validation::{AnchorValidationContext, validate_anchor_transaction};
use alethia_reth_evm::factory::TaikoEvmFactory;
use alethia_reth_primitives::payload::builder::TaikoPayloadBuilderAttributes;

/// Taiko payload builder
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaikoPayloadBuilder<Client, Pool, EvmConfig = TaikoEvmConfig> {
    /// Client providing access to node state.
    client: Client,
    /// Transaction pool for selecting transactions.
    pool: Pool,
    /// EVM configuration for payload building.
    evm_config: EvmConfig,
}

/// Outcome of executing the transaction phase for a payload build.
enum ExecutionOutcome {
    /// Execution was cancelled before completion.
    Cancelled,
    /// Execution completed successfully with accumulated fees.
    Completed(U256),
}

/// Context for executing transactions in new mode (anchor + pool transactions).
struct PoolExecutionContext<'a> {
    /// Prebuilt anchor transaction for new mode.
    anchor_tx: &'a Recovered<EthTransactionSigned>,
    /// The parent block header.
    parent_header: &'a RethHeader,
    /// Timestamp for the new block.
    block_timestamp: u64,
    /// Payload identifier for logging.
    payload_id: String,
    /// Base fee per gas for transaction selection.
    base_fee: u64,
    /// Block gas limit.
    gas_limit: u64,
}

/// Executes the provided transaction list in legacy mode.
///
/// Preserves legacy mode: validation errors are skipped, fatal errors abort
/// the build, and cancellation short-circuits the loop.
fn execute_provided_transactions(
    builder: &mut impl BlockBuilder<Primitives = EthPrimitives>,
    transactions: &[Recovered<EthTransactionSigned>],
    base_fee: u64,
    cancel: &CancelOnDrop,
) -> Result<ExecutionOutcome, PayloadBuilderError> {
    let mut total_fees = U256::ZERO;

    for tx in transactions {
        if cancel.is_cancelled() {
            return Ok(ExecutionOutcome::Cancelled);
        }

        let gas_used = match builder.execute_transaction(tx.clone()) {
            Ok(gas_used) => gas_used,
            Err(BlockExecutionError::Validation(
                BlockValidationError::InvalidTx { .. }
                | BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. },
            )) => {
                trace!(target: "payload_builder", ?tx, "skipping invalid transaction in legacy mode");
                continue;
            }
            // Fatal errors should still fail the build
            Err(err) => {
                warn!(target: "payload_builder", ?tx, %err, "fatal error executing transaction");
                return Err(PayloadBuilderError::evm(err));
            }
        };

        // Add transaction fees to total
        let miner_fee =
            tx.effective_tip_per_gas(base_fee).expect("fee is always valid; execution succeeded");
        total_fees += U256::from(miner_fee) * U256::from(gas_used);
    }

    Ok(ExecutionOutcome::Completed(total_fees))
}

/// Executes new-mode transactions: injects the anchor transaction, then pulls
/// from the mempool until exhaustion or cancellation.
fn execute_anchor_and_pool_transactions<Client, Pool>(
    builder: &mut impl BlockBuilder<Primitives = EthPrimitives>,
    pool: &Pool,
    client: &Client,
    ctx: &PoolExecutionContext<'_>,
    cancel: &CancelOnDrop,
) -> Result<ExecutionOutcome, PayloadBuilderError>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec = TaikoChainSpec>
        + reth_provider::BlockReader,
    Pool: reth_transaction_pool::TransactionPool<
            Transaction: reth_transaction_pool::PoolTransaction<
                Consensus = reth_ethereum::TransactionSigned,
            >,
        >,
{
    debug!(target: "payload_builder", id=%ctx.payload_id, "injecting anchor transaction");

    let chain_spec = client.chain_spec();
    validate_anchor_transaction(
        ctx.anchor_tx.inner(),
        chain_spec.as_ref(),
        AnchorValidationContext {
            timestamp: ctx.block_timestamp,
            block_number: ctx.parent_header.number + 1,
            base_fee_per_gas: ctx.base_fee,
        },
    )
    .map_err(PayloadBuilderError::other)?;

    // Execute the anchor transaction as the first transaction in the block
    // NOTE: anchor transaction does not contribute to the total DA size limit calculation.
    match builder.execute_transaction(ctx.anchor_tx.clone()) {
        Ok(gas_used) => {
            // Note: Anchor transaction has zero priority fee (tip), so no fees to add
            debug!(target: "payload_builder", id=%ctx.payload_id, gas_used, "anchor transaction executed successfully");
        }
        Err(err) => {
            warn!(target: "payload_builder", id=%ctx.payload_id, %err, "failed to execute anchor transaction");
            return Err(PayloadBuilderError::evm(err));
        }
    }

    // Use the shared transaction selection logic for pool transactions
    let config = TxSelectionConfig {
        base_fee: ctx.base_fee,
        gas_limit_per_list: ctx.gas_limit,
        max_da_bytes_per_list: BYTES_PER_BLOB as u64,
        max_lists: 1,
        min_tip: 0,
        locals: None,
    };

    match select_and_execute_pool_transactions(builder, pool, &config, || cancel.is_cancelled()) {
        Ok(SelectionOutcome::Cancelled) => Ok(ExecutionOutcome::Cancelled),
        Ok(SelectionOutcome::Completed(lists)) => {
            // Calculate total fees from the executed transactions
            let total_fees = lists.first().map_or(U256::ZERO, |list| {
                list.transactions.iter().fold(U256::ZERO, |acc, etx| {
                    let tip = etx
                        .tx
                        .effective_tip_per_gas(ctx.base_fee)
                        .expect("fee is always valid; execution succeeded");
                    acc + U256::from(tip) * U256::from(etx.gas_used)
                })
            });
            Ok(ExecutionOutcome::Completed(total_fees))
        }
        Err(err) => Err(PayloadBuilderError::evm(err)),
    }
}

impl<Client, Pool, EvmConfig> TaikoPayloadBuilder<Client, Pool, EvmConfig> {
    /// Creates a new payload builder with the given client, pool, and EVM config.
    pub const fn new(client: Client, pool: Pool, evm_config: EvmConfig) -> Self {
        Self { client, pool, evm_config }
    }
}

impl<Client, Pool, EvmConfig> PayloadBuilder for TaikoPayloadBuilder<Client, Pool, EvmConfig>
where
    EvmConfig: ConfigureEvm<
            Primitives = EthPrimitives,
            Error = Infallible,
            NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
            BlockExecutorFactory = TaikoBlockExecutorFactory<
                RethReceiptBuilder,
                Arc<TaikoChainSpec>,
                TaikoEvmFactory,
            >,
            BlockAssembler = TaikoBlockAssembler,
        > + Clone,
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec = TaikoChainSpec>
        + reth_provider::BlockReader
        + Clone,
    Pool: reth_transaction_pool::TransactionPool<
            Transaction: reth_transaction_pool::PoolTransaction<
                Consensus = reth_ethereum::TransactionSigned,
            >,
        > + Clone,
{
    /// The payload attributes type to accept for building.
    type Attributes = TaikoPayloadBuilderAttributes;
    /// /// The type of the built payload.
    type BuiltPayload = EthBuiltPayload;

    /// Tries to build a transaction payload using provided arguments.
    ///
    /// Constructs a transaction payload based on the given arguments,
    /// returning a `Result` indicating success or an error if building fails.
    ///
    /// # Arguments
    ///
    /// - `args`: Build arguments containing necessary components.
    ///
    /// # Returns
    ///
    /// A `Result` indicating the build outcome or an error.
    fn try_build(
        &self,
        args: BuildArguments<TaikoPayloadBuilderAttributes, EthBuiltPayload>,
    ) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError> {
        taiko_payload(self.evm_config.clone(), self.client.clone(), self.pool.clone(), args)
    }

    /// Invoked when the payload job is being resolved and there is no payload yet.
    ///
    /// This can happen if the CL requests a payload before the first payload has been built.
    fn on_missing_payload(
        &self,
        _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        MissingPayloadBehaviour::AwaitInProgress
    }

    /// Builds an empty payload without any transaction.
    fn build_empty_payload(
        &self,
        _config: PayloadConfig<TaikoPayloadBuilderAttributes>,
    ) -> Result<EthBuiltPayload, PayloadBuilderError> {
        Err(PayloadBuilderError::MissingPayload)
    }
}

// Build a Taiko network payload using the given attributes.
#[inline]
fn taiko_payload<EvmConfig, Client, Pool>(
    evm_config: EvmConfig,
    client: Client,
    pool: Pool,
    args: BuildArguments<TaikoPayloadBuilderAttributes, EthBuiltPayload>,
) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError>
where
    EvmConfig: ConfigureEvm<
            Primitives = EthPrimitives,
            Error = Infallible,
            NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
            BlockExecutorFactory = TaikoBlockExecutorFactory<
                RethReceiptBuilder,
                Arc<TaikoChainSpec>,
                TaikoEvmFactory,
            >,
            BlockAssembler = TaikoBlockAssembler,
        >,
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec = TaikoChainSpec>
        + reth_provider::BlockReader,
    Pool: reth_transaction_pool::TransactionPool<
            Transaction: reth_transaction_pool::PoolTransaction<
                Consensus = reth_ethereum::TransactionSigned,
            >,
        >,
{
    let BuildArguments { mut cached_reads, config, cancel, best_payload: _ } = args;
    let PayloadConfig { parent_header, attributes } = config;

    let state_provider = client.state_by_block_hash(parent_header.hash())?;
    let state = StateProviderDatabase::new(state_provider.as_ref());
    let mut db =
        State::builder().with_database_ref(cached_reads.as_db(state)).with_bundle_update().build();

    debug!(target: "payload_builder", id=%attributes.payload_id(), parent_hash=?parent_header.hash(), parent_number=parent_header.number, "building new payload");

    // Create block builder using the ConfigureEvm API
    let mut builder = evm_config
        .builder_for_next_block(
            &mut db,
            &parent_header,
            TaikoNextBlockEnvAttributes {
                timestamp: attributes.timestamp(),
                suggested_fee_recipient: attributes.beneficiary,
                prev_randao: attributes.mix_hash,
                gas_limit: attributes.gas_limit,
                extra_data: attributes.extra_data.clone(),
                base_fee_per_gas: attributes.base_fee_per_gas,
            },
        )
        .map_err(PayloadBuilderError::other)?;

    builder.apply_pre_execution_changes().map_err(PayloadBuilderError::other)?;

    // Get the base fee from the builder (already calculated based on attributes)
    let base_fee = builder.evm_mut().block.basefee;

    // Execute transactions - either from provided list (legacy) or from mempool (new mode)
    let total_fees = match &attributes.transactions {
        Some(transactions) => {
            // Legacy mode: Use provided transactions
            debug!(target: "payload_builder", id=%attributes.payload_id(), tx_count=transactions.len(), "using provided transaction list");
            match execute_provided_transactions(&mut builder, transactions, base_fee, &cancel)? {
                ExecutionOutcome::Cancelled => return Ok(BuildOutcome::Cancelled),
                ExecutionOutcome::Completed(fees) => fees,
            }
        }
        None => {
            // New mode: Select transactions from mempool
            debug!(target: "payload_builder", id=%attributes.payload_id(), "selecting transactions from mempool");

            let anchor_tx = attributes.anchor_transaction.as_ref().ok_or_else(|| {
                warn!(target: "payload_builder", id=%attributes.payload_id(), "missing prebuilt anchor transaction in new mode");
                PayloadBuilderError::MissingPayload
            })?;

            let ctx = PoolExecutionContext {
                anchor_tx,
                parent_header: &parent_header,
                block_timestamp: attributes.timestamp(),
                payload_id: attributes.payload_id().to_string(),
                base_fee,
                gas_limit: attributes.gas_limit,
            };

            match execute_anchor_and_pool_transactions(&mut builder, &pool, &client, &ctx, &cancel)?
            {
                ExecutionOutcome::Cancelled => return Ok(BuildOutcome::Cancelled),
                ExecutionOutcome::Completed(fees) => fees,
            }
        }
    };

    let BlockBuilderOutcome { execution_result: _, block, .. } =
        builder.finish(state_provider.as_ref())?;

    // Seal the block
    let sealed_block = Arc::new(block.sealed_block().clone());
    debug!(target: "payload_builder", id=%attributes.payload_id(), sealed_block_header = ?sealed_block.sealed_header(), "sealed built block");

    // Build the payload
    Ok(BuildOutcome::Freeze(EthBuiltPayload::new(
        attributes.payload_id(),
        sealed_block,
        total_fees,
        None,
    )))
}
