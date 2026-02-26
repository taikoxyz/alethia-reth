use std::sync::Arc;

use reth::{
    api::{PayloadBuilderAttributes, PayloadBuilderError},
    providers::{ChainSpecProvider, StateProviderFactory},
    revm::{State, database::StateProviderDatabase},
};
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_ethereum::EthPrimitives;
use reth_ethereum_engine_primitives::EthBuiltPayload;
use reth_evm::{
    ConfigureEvm,
    execute::{BlockBuilder, BlockBuilderOutcome},
};
use reth_evm_ethereum::RethReceiptBuilder;
use tracing::{debug, warn};

use alethia_reth_block::{
    assembler::TaikoBlockAssembler,
    config::{TaikoEvmConfig, TaikoNextBlockEnvAttributes},
    factory::TaikoBlockExecutorFactory,
};
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_consensus::validation::ANCHOR_V3_V4_GAS_LIMIT;
use alethia_reth_evm::factory::TaikoEvmFactory;
use alethia_reth_primitives::payload::builder::TaikoPayloadBuilderAttributes;

use self::execution::{
    ExecutionOutcome, PoolExecutionContext, execute_anchor_and_pool_transactions,
    execute_provided_transactions,
};

/// Transaction-phase execution helpers for legacy/new-mode payload construction.
mod execution;

/// Taiko payload builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaikoPayloadBuilder<Client, Pool, EvmConfig = TaikoEvmConfig> {
    /// Client providing access to node state.
    client: Client,
    /// Transaction pool for selecting transactions.
    pool: Pool,
    /// EVM configuration for payload building.
    evm_config: EvmConfig,
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
            NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
            BlockExecutorFactory = TaikoBlockExecutorFactory<
                RethReceiptBuilder,
                Arc<TaikoChainSpec>,
                TaikoEvmFactory,
            >,
            BlockAssembler = TaikoBlockAssembler,
        > + Clone,
    EvmConfig::Error: core::fmt::Debug,
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
    /// The type of the built payload.
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
        taiko_payload(&self.evm_config, &self.client, &self.pool, args)
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

/// Build a Taiko payload for the given parent/header and job attributes.
#[inline]
fn taiko_payload<EvmConfig, Client, Pool>(
    evm_config: &EvmConfig,
    client: &Client,
    pool: &Pool,
    args: BuildArguments<TaikoPayloadBuilderAttributes, EthBuiltPayload>,
) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError>
where
    EvmConfig: ConfigureEvm<
            Primitives = EthPrimitives,
            NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
            BlockExecutorFactory = TaikoBlockExecutorFactory<
                RethReceiptBuilder,
                Arc<TaikoChainSpec>,
                TaikoEvmFactory,
            >,
            BlockAssembler = TaikoBlockAssembler,
        >,
    EvmConfig::Error: core::fmt::Debug,
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
                gas_limit: attributes.gas_limit.saturating_sub(ANCHOR_V3_V4_GAS_LIMIT),
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
