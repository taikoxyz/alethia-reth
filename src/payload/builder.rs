use alloy_consensus::Transaction;
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::Bytes;
use reth::{
    api::{PayloadBuilderAttributes, PayloadBuilderError},
    providers::{ChainSpecProvider, StateProviderFactory},
    revm::{
        State,
        database::StateProviderDatabase,
        primitives::{Address, B256, U256},
    },
};
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_engine_local::LocalPayloadAttributesBuilder;
use reth_ethereum::EthPrimitives;
use reth_ethereum_engine_primitives::EthBuiltPayload;
use reth_evm::{
    ConfigureEvm,
    block::{BlockExecutionError, BlockValidationError},
    execute::{BlockBuilder, BlockBuilderOutcome},
};
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_api::PayloadAttributesBuilder;
use std::{convert::Infallible, sync::Arc};
use tracing::{debug, trace, warn};

use crate::{
    block::{assembler::TaikoBlockAssembler, factory::TaikoBlockExecutorFactory},
    chainspec::spec::TaikoChainSpec,
    evm::{
        config::{TaikoEvmConfig, TaikoNextBlockEnvAttributes},
        factory::TaikoEvmFactory,
    },
    payload::{
        attributes::{RpcL1Origin, TaikoBlockMetadata, TaikoPayloadAttributes},
        payload::TaikoPayloadBuilderAttributes,
    },
};

const TAIKO_PACAYA_BLOCK_GAS_LIMIT: u64 = 241_000_000;

/// Taiko payload builder
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaikoPayloadBuilder<Client, EvmConfig = TaikoEvmConfig> {
    /// Client providing access to node state.
    client: Client,
    /// The type responsible for creating the evm.
    evm_config: EvmConfig,
}

impl<Client, EvmConfig> TaikoPayloadBuilder<Client, EvmConfig> {
    /// `TaikoPayloadBuilder` constructor.
    pub const fn new(client: Client, evm_config: EvmConfig) -> Self {
        Self { client, evm_config }
    }
}

// Default implementation of [PayloadBuilder] for unit type
impl<Client, EvmConfig> PayloadBuilder for TaikoPayloadBuilder<Client, EvmConfig>
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
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone,
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
        taiko_payload(self.evm_config.clone(), self.client.clone(), args)
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
        _config: PayloadConfig<Self::Attributes>,
    ) -> Result<EthBuiltPayload, PayloadBuilderError> {
        Err(PayloadBuilderError::MissingPayload)
    }
}

// Build a Taiko network payload using the given attributes.
#[inline]
fn taiko_payload<EvmConfig, Client>(
    evm_config: EvmConfig,
    client: Client,
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
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks>,
{
    let BuildArguments { mut cached_reads, config, cancel, best_payload: _ } = args;
    let PayloadConfig { parent_header, attributes } = config;

    let state_provider = client.state_by_block_hash(parent_header.hash())?;
    let state = StateProviderDatabase::new(&state_provider);
    let mut db =
        State::builder().with_database(cached_reads.as_db_mut(state)).with_bundle_update().build();

    debug!(target: "payload_builder", id=%attributes.payload_id(), parent_header = ?parent_header.hash(), parent_number = parent_header.number, attributes = ?attributes, "building payload for block");

    let mut builder = evm_config
        .builder_for_next_block(
            &mut db,
            &parent_header,
            TaikoNextBlockEnvAttributes {
                timestamp: attributes.timestamp(),
                suggested_fee_recipient: attributes.suggested_fee_recipient(),
                prev_randao: attributes.prev_randao(),
                gas_limit: attributes.gas_limit,
                base_fee_per_gas: attributes.base_fee_per_gas,
                extra_data: attributes.extra_data.clone(),
            },
        )
        .map_err(PayloadBuilderError::other)?;

    debug!(target: "payload_builder", id=%attributes.payload_id(), parent_header = ?parent_header.hash(), parent_number = parent_header.number, "building new payload");
    let base_fee = attributes.base_fee_per_gas;
    let mut total_fees = U256::ZERO;

    builder.apply_pre_execution_changes().map_err(|err| {
        warn!(target: "payload_builder", %err, "failed to apply pre-execution changes");
        PayloadBuilderError::Internal(err.into())
    })?;

    for tx in &attributes.transactions {
        // check if the job was cancelled, if so we can exit early
        if cancel.is_cancelled() {
            return Ok(BuildOutcome::Cancelled);
        }

        let gas_used = match builder.execute_transaction(tx.clone()) {
            Ok(gas_used) => gas_used,
            Err(BlockExecutionError::Validation(
                BlockValidationError::InvalidTx { .. }
                | BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. },
            )) => {
                trace!(target: "payload_builder", ?tx, "skipping invalid transaction");
                continue;
            }
            // this is an error that we should treat as fatal for this attempt
            Err(err) => return Err(PayloadBuilderError::evm(err)),
        };

        // update add to total fees
        let miner_fee =
            tx.effective_tip_per_gas(base_fee).expect("fee is always valid; execution succeeded");
        total_fees += U256::from(miner_fee) * U256::from(gas_used);
    }

    let BlockBuilderOutcome { block, .. } = builder.finish(&state_provider)?;

    let sealed_block = Arc::new(block.sealed_block().clone());

    let payload = EthBuiltPayload::new(attributes.payload_id(), sealed_block, total_fees, None);

    Ok(BuildOutcome::Freeze(payload))
}

/// Implement `PayloadAttributesBuilder` for `LocalPayloadAttributesBuilder<TaikoChainSpec>`,
/// to build `TaikoPayloadAttributes` from the local payload attributes builder.
impl PayloadAttributesBuilder<TaikoPayloadAttributes>
    for LocalPayloadAttributesBuilder<TaikoChainSpec>
{
    /// Return a new payload attribute from the builder.
    fn build(&self, timestamp: u64) -> TaikoPayloadAttributes {
        TaikoPayloadAttributes {
            payload_attributes: self.build(timestamp),
            base_fee_per_gas: U256::ZERO,
            block_metadata: TaikoBlockMetadata {
                beneficiary: Address::random(),
                timestamp: U256::from(timestamp),
                gas_limit: TAIKO_PACAYA_BLOCK_GAS_LIMIT,
                mix_hash: B256::random(),
                tx_list: Bytes::new(),
                extra_data: Bytes::new(),
            },
            l1_origin: RpcL1Origin {
                block_id: U256::ZERO,
                l2_block_hash: B256::ZERO,
                l1_block_hash: None,
                l1_block_height: None,
                build_payload_args_id: [0; 8],
                is_forced_inclusion: false,
                signature: [0; 65],
            },
        }
    }
}
