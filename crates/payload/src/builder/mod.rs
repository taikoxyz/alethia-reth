use std::sync::Arc;

use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_ethereum::EthPrimitives;
use reth_evm::{
    ConfigureEvm,
    execute::{BlockBuilder, BlockBuilderOutcome, BlockExecutor},
};
use reth_evm_ethereum::RethReceiptBuilder;
use reth_execution_cache::{CachedStateMetrics, CachedStateMetricsSource, CachedStateProvider};
use reth_payload_builder::EthBuiltPayload;
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_provider::{BlockReader, ChainSpecProvider, StateProviderFactory};
use reth_revm::{database::StateProviderDatabase, db::State};
use tracing::{debug, warn};

use alethia_reth_block::{
    assembler::TaikoBlockAssembler,
    config::{TaikoEvmConfig, TaikoNextBlockEnvAttributes},
    factory::TaikoBlockExecutorFactory,
};
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_consensus::validation::ANCHOR_V3_V4_GAS_LIMIT;
use alethia_reth_evm::factory::TaikoEvmFactory;
use alethia_reth_primitives::payload::{
    attributes::TaikoPayloadAttributes, builder::TaikoPayloadBuilderAttributes,
};

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
    type Attributes = TaikoPayloadAttributes;
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
        args: BuildArguments<TaikoPayloadAttributes, EthBuiltPayload>,
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
        missing_payload_behaviour()
    }

    /// Builds an empty payload without any transaction.
    fn build_empty_payload(
        &self,
        _config: PayloadConfig<TaikoPayloadAttributes>,
    ) -> Result<EthBuiltPayload, PayloadBuilderError> {
        empty_payload_result()
    }
}

/// Normalizes engine-facing payload config into the Taiko builder attribute shape.
fn normalize_payload_config(
    config: &PayloadConfig<TaikoPayloadAttributes>,
) -> Result<TaikoPayloadBuilderAttributes, PayloadBuilderError> {
    let mut attributes = TaikoPayloadBuilderAttributes::try_new(
        config.parent_header.hash(),
        config.attributes.clone(),
    )
    .map_err(PayloadBuilderError::other)?;
    attributes.id = config.payload_id;
    Ok(attributes)
}

/// Returns the legacy missing-payload behavior for Taiko payload jobs.
fn missing_payload_behaviour() -> MissingPayloadBehaviour<EthBuiltPayload> {
    MissingPayloadBehaviour::AwaitInProgress
}

/// Returns the legacy empty-payload result for Taiko payload jobs.
fn empty_payload_result() -> Result<EthBuiltPayload, PayloadBuilderError> {
    Err(PayloadBuilderError::MissingPayload)
}

/// Build a Taiko payload for the given parent/header and job attributes.
#[inline]
fn taiko_payload<EvmConfig, Client, Pool>(
    evm_config: &EvmConfig,
    client: &Client,
    pool: &Pool,
    args: BuildArguments<TaikoPayloadAttributes, EthBuiltPayload>,
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
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec = TaikoChainSpec> + BlockReader,
    Pool: reth_transaction_pool::TransactionPool<
            Transaction: reth_transaction_pool::PoolTransaction<
                Consensus = reth_ethereum::TransactionSigned,
            >,
        >,
{
    let BuildArguments {
        mut cached_reads,
        execution_cache,
        trie_handle,
        config,
        cancel,
        best_payload: _,
    } = args;
    let attributes = normalize_payload_config(&config)?;
    let PayloadConfig { parent_header, attributes: _, payload_id } = config;

    let mut state_provider = client.state_by_block_hash(parent_header.hash())?;
    if let Some(execution_cache) = execution_cache {
        state_provider = Box::new(CachedStateProvider::new(
            state_provider,
            execution_cache.cache().clone(),
            CachedStateMetrics::zeroed(CachedStateMetricsSource::Builder),
        ));
    }
    let state = StateProviderDatabase::new(state_provider.as_ref());
    let mut db =
        State::builder().with_database(cached_reads.as_db_mut(state)).with_bundle_update().build();

    debug!(target: "payload_builder", id=%payload_id, parent_hash=?parent_header.hash(), parent_number=parent_header.number, "building new payload");

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

    if let Some(ref handle) = trie_handle {
        builder.executor_mut().set_state_hook(Some(Box::new(handle.state_hook())));
    }

    builder.apply_pre_execution_changes().map_err(PayloadBuilderError::other)?;

    let base_fee = builder.evm_mut().block.basefee;

    let total_fees = match &attributes.transactions {
        Some(transactions) => {
            debug!(target: "payload_builder", id=%payload_id, tx_count=transactions.len(), "using provided transaction list");
            match execute_provided_transactions(&mut builder, transactions, base_fee, &cancel)? {
                ExecutionOutcome::Cancelled => return Ok(BuildOutcome::Cancelled),
                ExecutionOutcome::Completed(fees) => fees,
            }
        }
        None => {
            debug!(target: "payload_builder", id=%payload_id, "selecting transactions from mempool");

            let anchor_tx = attributes.anchor_transaction.as_ref().ok_or_else(|| {
                warn!(target: "payload_builder", id=%payload_id, "missing prebuilt anchor transaction in new mode");
                PayloadBuilderError::MissingPayload
            })?;

            let ctx = PoolExecutionContext {
                anchor_tx,
                parent_header: &parent_header,
                block_timestamp: attributes.timestamp(),
                payload_id: payload_id.to_string(),
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

    let BlockBuilderOutcome { execution_result: _, block, .. } = if let Some(mut handle) =
        trie_handle
    {
        // Drop the state hook so the trie task sees the final state updates and can finalize.
        builder.executor_mut().set_state_hook(None);

        // The sparse trie computes alongside transaction execution, so this usually just waits
        // for the last root/trie update. If that pipeline fails, fall back to synchronous state
        // root computation inside `finish`.
        match handle.state_root() {
            Ok(outcome) => {
                debug!(target: "payload_builder", id=%payload_id, state_root=?outcome.state_root, "received state root from sparse trie");
                builder.finish(
                    state_provider.as_ref(),
                    Some((outcome.state_root, Arc::unwrap_or_clone(outcome.trie_updates))),
                )?
            }
            Err(err) => {
                warn!(target: "payload_builder", id=%payload_id, %err, "sparse trie failed, falling back to sync state root");
                builder.finish(state_provider.as_ref(), None)?
            }
        }
    } else {
        builder.finish(state_provider.as_ref(), None)?
    };

    let sealed_block = Arc::new(block.into_sealed_block());
    debug!(target: "payload_builder", id=%payload_id, sealed_block_header = ?sealed_block.sealed_header(), "sealed built block");

    Ok(BuildOutcome::Freeze(EthBuiltPayload::new(sealed_block, total_fees, None, None)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alethia_reth_primitives::payload::attributes::{RpcL1Origin, TaikoBlockMetadata};
    use alloy_consensus::Header;
    use alloy_primitives::{Address, B256, Bytes, U256};
    use alloy_rpc_types_engine::{PayloadAttributes as EthPayloadAttributes, PayloadId};
    use reth_basic_payload_builder::PayloadConfig;
    use reth_primitives_traits::SealedHeader;
    use std::sync::Arc;

    fn test_payload_config(
        tx_list: Option<Bytes>,
        base_fee_per_gas: U256,
        anchor_transaction: Option<Bytes>,
    ) -> PayloadConfig<TaikoPayloadAttributes> {
        let parent_header = Arc::new(SealedHeader::seal_slow(Header {
            number: 7,
            gas_limit: 30_000_000,
            ..Default::default()
        }));
        let payload_id = PayloadId::new([9; 8]);
        let attributes = TaikoPayloadAttributes {
            payload_attributes: EthPayloadAttributes {
                timestamp: 42,
                prev_randao: B256::repeat_byte(0x11),
                suggested_fee_recipient: Address::repeat_byte(0x22),
                withdrawals: Some(Vec::new()),
                parent_beacon_block_root: Some(B256::repeat_byte(0x33)),
                slot_number: None,
            },
            base_fee_per_gas,
            block_metadata: TaikoBlockMetadata {
                beneficiary: Address::repeat_byte(0x44),
                gas_limit: 30_000_000,
                timestamp: U256::from(42),
                mix_hash: B256::repeat_byte(0x55),
                tx_list,
                extra_data: Bytes::new(),
            },
            l1_origin: RpcL1Origin {
                block_id: U256::ZERO,
                l2_block_hash: B256::ZERO,
                l1_block_height: None,
                l1_block_hash: None,
                build_payload_args_id: [0; 8],
                is_forced_inclusion: false,
                signature: [0; 65],
            },
            anchor_transaction,
        };

        PayloadConfig::new(parent_header, attributes, payload_id)
    }

    #[test]
    fn fixed_transaction_lists_freeze_payload_builds() {
        let config = test_payload_config(Some(Bytes::new()), U256::from(1u64), None);
        let attributes = normalize_payload_config(&config).expect("config should normalize");

        assert!(attributes.transactions.is_some());
    }

    #[test]
    fn mempool_mode_also_freezes_payload_builds() {
        let config = test_payload_config(None, U256::from(1u64), None);
        let attributes = normalize_payload_config(&config).expect("config should normalize");

        assert!(attributes.transactions.is_none());
    }

    #[test]
    fn malformed_attrs_still_await_in_progress_on_missing_payload() {
        assert!(matches!(missing_payload_behaviour(), MissingPayloadBehaviour::AwaitInProgress));
    }

    #[test]
    fn malformed_attrs_still_reject_empty_payloads_with_missing_payload() {
        let _config = test_payload_config(None, U256::from(1u64), Some(Bytes::from(vec![0x01])));
        let err = empty_payload_result().expect_err("Taiko should not build empty payloads");

        assert!(matches!(err, PayloadBuilderError::MissingPayload));
    }

    #[test]
    fn valid_attrs_still_reject_empty_payloads_with_missing_payload() {
        let _config = test_payload_config(None, U256::from(1u64), None);

        let err = empty_payload_result().expect_err("Taiko should not build empty payloads");
        assert!(matches!(err, PayloadBuilderError::MissingPayload));
    }
}
