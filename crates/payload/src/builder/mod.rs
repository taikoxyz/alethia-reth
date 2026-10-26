use std::sync::Arc;

use alloy_hardforks::EthereumHardforks;
use alloy_rlp::Encodable;
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_errors::RethError;
use reth_ethereum::{Block, EthPrimitives};
use reth_evm::{
    ConfigureEvm,
    execute::{BlockBuilder, BlockBuilderOutcome},
};
use reth_evm_ethereum::RethReceiptBuilder;
use reth_execution_cache::{CachedStateMetrics, CachedStateMetricsSource, CachedStateProvider};
use reth_payload_builder::EthBuiltPayload;
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_primitives_traits::SealedBlock;
use reth_provider::{BlockReader, ChainSpecProvider, StateProviderFactory};
use reth_revm::{database::StateProviderDatabase, db::State};
use tracing::{debug, warn};

use alethia_reth_block::{
    assembler::{TaikoBlockAssembler, etna_header_length_bound},
    config::{TaikoEvmConfig, TaikoNextBlockEnvAttributes},
    factory::TaikoBlockExecutorFactory,
    tx_selection::{ZkOverflowMemory, selection_state_seed},
};
use alethia_reth_chainspec::{hardfork::TaikoHardforks, spec::TaikoChainSpec};
use alethia_reth_consensus::validation::ANCHOR_V3_V4_GAS_LIMIT;
use alethia_reth_evm::factory::TaikoEvmFactory;
use alethia_reth_primitives::{
    etna::{execution_block_length, max_txs_payload_len},
    payload::{attributes::TaikoPayloadAttributes, builder::TaikoPayloadBuilderAttributes},
};

use self::execution::{
    ExecutionOutcome, PoolExecutionContext, execute_pool_transactions,
    execute_provided_transactions,
};

/// Transaction-phase execution helpers for legacy/new-mode payload construction.
mod execution;

/// Taiko payload builder.
#[derive(Debug, Clone)]
pub struct TaikoPayloadBuilder<Client, Pool, EvmConfig = TaikoEvmConfig> {
    /// Client providing access to node state.
    client: Client,
    /// Transaction pool for selecting transactions.
    pool: Pool,
    /// EVM configuration for payload building.
    evm_config: EvmConfig,
    /// Zk gas overflows remembered across mempool selections; clones share the same memory.
    zk_overflows: Arc<ZkOverflowMemory>,
}

impl<Client, Pool, EvmConfig> TaikoPayloadBuilder<Client, Pool, EvmConfig> {
    /// Creates a new payload builder with the given client, pool, and EVM config, and an empty
    /// zk gas overflow memory.
    pub fn new(client: Client, pool: Pool, evm_config: EvmConfig) -> Self {
        Self { client, pool, evm_config, zk_overflows: Arc::default() }
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
        taiko_payload(&self.evm_config, &self.client, &self.pool, &self.zk_overflows, args)
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
    chain_spec: &TaikoChainSpec,
) -> Result<TaikoPayloadBuilderAttributes, PayloadBuilderError> {
    let is_etna_active = chain_spec.is_etna_active(config.attributes.payload_attributes.timestamp);
    let mut attributes = TaikoPayloadBuilderAttributes::try_new_for_fork(
        config.parent_header.hash(),
        config.attributes.clone(),
        is_etna_active,
    )
    .map_err(PayloadBuilderError::other)?;
    attributes.id = config.payload_id;
    Ok(attributes)
}

/// Returns the gas available to pool transactions: the full block limit from Etna on, and the
/// limit minus the legacy anchor reservation before it.
fn pool_gas_limit(chain_spec: &TaikoChainSpec, timestamp: u64, gas_limit: u64) -> u64 {
    if chain_spec.is_etna_active(timestamp) {
        gas_limit
    } else {
        gas_limit.saturating_sub(ANCHOR_V3_V4_GAS_LIMIT)
    }
}

/// Returns the legacy missing-payload behavior for Taiko payload jobs.
fn missing_payload_behaviour() -> MissingPayloadBehaviour<EthBuiltPayload> {
    MissingPayloadBehaviour::AwaitInProgress
}

/// Returns the legacy empty-payload result for Taiko payload jobs.
fn empty_payload_result() -> Result<EthBuiltPayload, PayloadBuilderError> {
    Err(PayloadBuilderError::MissingPayload)
}

/// Rejects payload builds on a chain spec that activates the Amsterdam fork.
///
/// Taiko schedules no Amsterdam fork, so [`taiko_payload`] never wires an EIP-7928 BAL builder
/// into the `State` and [`TaikoBlockAssembler`] always seals `block_access_list_hash: None`.
/// Reth's post-execution validation skips the access-list check whenever the hash is absent, so
/// an activation arriving through genesis config (`amsterdamTime`) would silently produce
/// non-compliant blocks. Fail closed instead, mirroring the import-side rejection in
/// `TaikoBeaconConsensus::validate_header`.
fn ensure_amsterdam_inactive(
    chain_spec: &TaikoChainSpec,
    timestamp: u64,
) -> Result<(), PayloadBuilderError> {
    if chain_spec.is_amsterdam_active_at_timestamp(timestamp) {
        return Err(PayloadBuilderError::Internal(RethError::msg(
            "cannot build a Taiko payload with the Amsterdam fork active: EIP-7928 block access lists are unsupported",
        )));
    }

    Ok(())
}

/// Returns the block environment the payload job executes in, derived from its attributes.
fn next_block_env_attributes(
    attributes: &TaikoPayloadBuilderAttributes,
) -> TaikoNextBlockEnvAttributes {
    TaikoNextBlockEnvAttributes {
        timestamp: attributes.timestamp(),
        suggested_fee_recipient: attributes.beneficiary,
        prev_randao: attributes.mix_hash,
        gas_limit: attributes.gas_limit,
        extra_data: attributes.extra_data.clone(),
        base_fee_per_gas: attributes.base_fee_per_gas,
        parent_beacon_block_root: attributes.parent_beacon_block_root,
    }
}

/// Returns the transaction byte budget of an Etna build: the largest summed block-body length of
/// its transactions whose executionBlock, with a `header_reserve`-byte header, still fits in
/// `max_block_bytes`.
///
/// The budget counts transaction bodies only, as [`TxSelectionConfig::max_tx_bytes_per_list`]
/// does; the list headers are accounted for here.
///
/// # Errors
///
/// An internal error whose text contains `maxBlockBytes` when even a block without transactions
/// does not fit.
///
/// [`TxSelectionConfig::max_tx_bytes_per_list`]:
///     alethia_reth_block::tx_selection::TxSelectionConfig::max_tx_bytes_per_list
fn etna_tx_bytes_budget(
    header_reserve: usize,
    max_block_bytes: u64,
) -> Result<u64, PayloadBuilderError> {
    max_txs_payload_len(header_reserve, max_block_bytes).ok_or_else(|| {
        PayloadBuilderError::Internal(RethError::msg(format!(
            "maxBlockBytes {max_block_bytes} cannot hold an Etna block with a \
             {header_reserve}-byte header reserve"
        )))
    })
}

/// Checks that the sealed block's executionBlock, `rlp([header, [tx…]])` including its outer list
/// header and with transactions in block-body form, is at most `max_block_bytes` long. The bound
/// is inclusive.
///
/// # Errors
///
/// An internal error whose text contains `maxBlockBytes` and names the actual and the maximum
/// length and the header's length, so a header reserve shortfall can be told apart from a budget
/// that was never applied.
fn ensure_within_max_block_bytes(
    block: &SealedBlock<Block>,
    max_block_bytes: u64,
) -> Result<(), PayloadBuilderError> {
    let len = execution_block_length(block.header(), &block.body().transactions);
    if len as u64 > max_block_bytes {
        let header_len = block.header().length();
        return Err(PayloadBuilderError::Internal(RethError::msg(format!(
            "built Etna executionBlock is {len} bytes ({header_len}-byte header), above \
             maxBlockBytes {max_block_bytes}"
        ))));
    }
    Ok(())
}

/// Build a Taiko payload for the given parent/header and job attributes.
///
/// `zk_overflows` carries zk gas overflows from earlier mempool selections into this one.
#[inline]
fn taiko_payload<EvmConfig, Client, Pool>(
    evm_config: &EvmConfig,
    client: &Client,
    pool: &Pool,
    zk_overflows: &ZkOverflowMemory,
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
        mut state_root_handle,
        config,
        cancel,
        best_payload: _,
    } = args;
    let chain_spec = client.chain_spec();
    let attributes = normalize_payload_config(&config, chain_spec.as_ref())?;
    let PayloadConfig { parent_header, attributes: _, payload_id, .. } = config;

    ensure_amsterdam_inactive(chain_spec.as_ref(), attributes.timestamp())?;

    // Normalization makes `maxBlockBytes` `Some` on every Etna job and `None` before Etna, so only
    // Etna builds get a transaction byte budget and the final size check; pre-Etna builds are
    // unchanged. The header is sealed after execution, so the budget leaves room for a bound on it.
    debug_assert_eq!(
        attributes.max_block_bytes.is_some(),
        chain_spec.is_etna_active(attributes.timestamp()),
        "maxBlockBytes must be present exactly on Etna jobs"
    );
    let env_attributes = next_block_env_attributes(&attributes);
    let tx_bytes_budget = attributes
        .max_block_bytes
        .map(|max_block_bytes| {
            etna_tx_bytes_budget(
                etna_header_length_bound(&parent_header, &env_attributes),
                max_block_bytes,
            )
        })
        .transpose()?;

    let mut state_provider = client.state_by_block_hash(parent_header.hash())?;
    if let Some(execution_cache) = execution_cache {
        state_provider = Box::new(CachedStateProvider::new(
            state_provider,
            execution_cache.cache().clone(),
            Some(CachedStateMetrics::zeroed(CachedStateMetricsSource::Builder)),
        ));
    }
    let state = StateProviderDatabase::new(state_provider.as_ref());
    let mut db =
        State::builder().with_database(cached_reads.as_db_mut(state)).with_bundle_update().build();

    debug!(target: "payload_builder", id=%payload_id, parent_hash=?parent_header.hash(), parent_number=parent_header.number, "building new payload");

    // New-mode pool selection starts from this payload's parent and block environment.
    let block_state_seed = selection_state_seed(parent_header.hash(), &env_attributes, &[]);
    let mut builder = evm_config
        .builder_for_next_block(&mut db, &parent_header, env_attributes)
        .map_err(PayloadBuilderError::other)?;

    // If we have a state-root task, wire a state hook that streams per-tx state diffs. Hooks
    // are delivered through the `State` database in reth v2.4.0.
    if let Some(task) = state_root_handle.as_mut() {
        reth_evm::Evm::db_mut(builder.evm_mut())
            .set_state_hook(Some(Box::new(task.take_state_hook())));
    }

    builder.apply_pre_execution_changes().map_err(PayloadBuilderError::other)?;

    let base_fee = builder.evm_mut().block.basefee;

    let total_fees = match &attributes.transactions {
        Some(transactions) => {
            debug!(target: "payload_builder", id=%payload_id, tx_count=transactions.len(), "using provided transaction list");
            match execute_provided_transactions(
                &mut builder,
                transactions,
                base_fee,
                tx_bytes_budget,
                &cancel,
            )? {
                ExecutionOutcome::Cancelled => return Ok(BuildOutcome::Cancelled),
                ExecutionOutcome::Completed(fees) => fees,
            }
        }
        None => {
            debug!(target: "payload_builder", id=%payload_id, "selecting transactions from mempool");
            let gas_limit =
                pool_gas_limit(chain_spec.as_ref(), attributes.timestamp(), attributes.gas_limit);

            let ctx = PoolExecutionContext {
                anchor_tx: attributes.anchor_transaction.as_ref(),
                forced_tx: attributes.forced_transaction.as_ref(),
                parent_header: &parent_header,
                block_state_seed,
                block_timestamp: attributes.timestamp(),
                payload_id: payload_id.to_string(),
                base_fee,
                gas_limit,
                zk_overflows,
                tx_bytes_budget,
            };

            match execute_pool_transactions(&mut builder, &pool, &client, &ctx, &cancel)? {
                ExecutionOutcome::Cancelled => return Ok(BuildOutcome::Cancelled),
                ExecutionOutcome::Completed(fees) => fees,
            }
        }
    };

    let outcome = if let Some(mut handle) = state_root_handle {
        // Drop the state hook so the trie task sees the final state updates and can finalize.
        reth_evm::Evm::db_mut(builder.evm_mut()).set_state_hook(None);

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

    // The Amsterdam guard above rejects these builds up front, and this builder never wires a
    // BAL builder into the `State`, so the list is always absent here. The named discard is a
    // backstop against either of those changing.
    let BlockBuilderOutcome { execution_result: _, block, block_access_list, .. } = outcome;
    debug_assert!(
        block_access_list.is_none(),
        "unexpected EIP-7928 block access list for a Taiko block"
    );

    debug!(target: "payload_builder", id=%payload_id, sealed_block_header = ?block.sealed_header(), "sealed built block");

    if let Some(max_block_bytes) = attributes.max_block_bytes {
        ensure_within_max_block_bytes(block.sealed_block(), max_block_bytes)?;
    }

    Ok(BuildOutcome::Freeze(EthBuiltPayload::new(Arc::new(block), total_fees, None, None)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alethia_reth_block::testutil::etna_chain_spec;
    use alethia_reth_chainspec::{TAIKO_MAINNET, hardfork::TaikoHardfork};
    use alethia_reth_primitives::payload::attributes::{RpcL1Origin, TaikoBlockMetadata};
    use alloy_consensus::{Header, Signed, TxLegacy, TxType};
    use alloy_eips::eip4895::Withdrawal;
    use alloy_genesis::{ChainConfig, Genesis};
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::{Address, B256, Bytes, Log, Signature, TxKind, U256};
    use alloy_rpc_types_engine::{PayloadAttributes as EthPayloadAttributes, PayloadId};
    use reth_basic_payload_builder::PayloadConfig;
    use reth_ethereum::{Receipt, TransactionSigned};
    use reth_evm::{
        block::BlockExecutionResult,
        execute::{BlockAssembler, BlockAssemblerInput},
    };
    use reth_primitives_traits::SealedHeader;
    use reth_revm::db::BundleState;
    use reth_storage_api::noop::NoopProvider;
    use std::sync::Arc;

    fn test_payload_config(tx_list: Option<Bytes>) -> PayloadConfig<TaikoPayloadAttributes> {
        let parent_header = Arc::new(SealedHeader::seal_slow(Header {
            number: 7,
            gas_limit: 30_000_000,
            ..Default::default()
        }));
        let payload_id = PayloadId::new([9; 8]);
        let attributes = TaikoPayloadAttributes {
            payload_attributes: EthPayloadAttributes {
                timestamp: 100,
                prev_randao: B256::repeat_byte(0x11),
                suggested_fee_recipient: Address::repeat_byte(0x22),
                withdrawals: Some(Vec::new()),
                parent_beacon_block_root: Some(B256::with_last_byte(1)),
                slot_number: None,
                target_gas_limit: None,
            },
            base_fee_per_gas: U256::from(1u64),
            block_metadata: TaikoBlockMetadata {
                beneficiary: Address::repeat_byte(0x44),
                gas_limit: 30_000_000,
                timestamp: U256::from(100),
                mix_hash: B256::repeat_byte(0x55),
                tx_list,
                extra_data: Bytes::from_static(b"1234567890123"),
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
            anchor_transaction: None,
            forced_transaction: None,
            max_block_bytes: None,
        };

        PayloadConfig::new(parent_header, attributes, payload_id)
    }

    fn chain_spec_with_etna_at(timestamp: u64) -> TaikoChainSpec {
        let mut spec = TaikoChainSpec::default();
        spec.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(timestamp));
        spec
    }

    /// Normalizes an Etna pool job with a 45M gas limit, a 1 gwei base fee, 13-byte extraData and
    /// a non-zero beacon root, and returns its parent with it.
    fn etna_job() -> (SealedHeader, TaikoPayloadBuilderAttributes) {
        let mut config = test_payload_config(None);
        config.attributes.base_fee_per_gas = U256::from(1_000_000_000_u64);
        config.attributes.block_metadata.gas_limit = 45_000_000;
        config.attributes.max_block_bytes = Some(124_000);
        let attributes = normalize_payload_config(&config, &etna_chain_spec())
            .expect("the Etna job should normalize");
        assert_eq!(attributes.extra_data.len(), 13);
        assert!(attributes.parent_beacon_block_root.is_some_and(|root| !root.is_zero()));
        (config.parent_header.as_ref().clone(), attributes)
    }

    /// Assembles the block the Etna builder seals for `attributes` on `parent`, through the
    /// builder's own block environment and the real assembler, after one transaction with a log,
    /// `gas_used` gas and `zk_gas` zk gas.
    fn assemble_etna_block(
        parent: &SealedHeader,
        attributes: &TaikoPayloadBuilderAttributes,
        gas_used: u64,
        zk_gas: u64,
    ) -> reth_ethereum::Block {
        let evm_config = TaikoEvmConfig::new(Arc::new(etna_chain_spec()));
        let env_attributes = next_block_env_attributes(attributes);
        let evm_env =
            evm_config.next_evm_env(parent, &env_attributes).expect("the Etna block env builds");
        let ctx = evm_config
            .context_for_next_block(parent, env_attributes)
            .expect("the Etna execution context builds");
        ctx.set_finalized_block_zk_gas(zk_gas);
        let tx: TransactionSigned = Signed::new_unhashed(
            TxLegacy {
                chain_id: Some(167),
                nonce: 0,
                gas_price: 1_000_000_000,
                gas_limit: 21_000,
                to: TxKind::Call(Address::repeat_byte(0x66)),
                value: U256::ZERO,
                input: Bytes::new(),
            },
            Signature::new(U256::from(1), U256::from(2), false),
        )
        .into();
        let output = BlockExecutionResult {
            receipts: vec![Receipt {
                tx_type: TxType::Legacy,
                success: true,
                cumulative_gas_used: gas_used,
                logs: vec![Log::new_unchecked(
                    Address::repeat_byte(0x77),
                    vec![B256::repeat_byte(0x88)],
                    Bytes::from_static(&[0x99; 32]),
                )],
            }],
            requests: Default::default(),
            gas_used,
            blob_gas_used: 0,
        };
        evm_config
            .block_assembler
            .assemble_block(BlockAssemblerInput::<TaikoBlockExecutorFactory>::new(
                evm_env,
                ctx,
                parent,
                vec![tx],
                &output,
                &BundleState::default(),
                &NoopProvider::default(),
                B256::repeat_byte(0xab),
                None,
            ))
            .expect("the Etna block should assemble")
    }

    #[test]
    fn etna_header_reserve_bounds_a_maximal_assembled_header() {
        let (parent, attrs) = etna_job();
        let reserve = etna_header_length_bound(&parent, &next_block_env_attributes(&attrs));

        // A real-shaped header: the job's known fields, a full block, 100M zk gas in the
        // difficulty, non-zero roots and a non-empty bloom.
        let real = assemble_etna_block(&parent, &attrs, 45_000_000, 100_000_000).header;
        assert_eq!(real.gas_used, 45_000_000);
        assert_eq!(real.difficulty, U256::from(100_000_000_u64));
        assert_eq!(real.base_fee_per_gas, Some(1_000_000_000));
        assert!(!real.logs_bloom.is_zero());
        assert!(!real.state_root.is_zero() && !real.receipts_root.is_zero());
        assert!(real.requests_hash.is_some() && real.parent_beacon_block_root.is_some());
        assert!(real.length() <= reserve, "{} > {reserve}", real.length());
        assert!(reserve - real.length() <= 16, "{reserve} - {}", real.length());

        // The reserve is exactly the length of the largest header this job can seal: every
        // assembler field is covered and nothing is counted twice.
        let maximal = assemble_etna_block(&parent, &attrs, 45_000_000, u64::MAX).header;
        assert_eq!(maximal.length(), reserve);
    }

    #[test]
    fn etna_tx_bytes_budget_follows_the_execution_block_bound() {
        assert_eq!(
            etna_tx_bytes_budget(600, 124_000).expect("124,000 bytes hold a 600-byte header"),
            123_392
        );
        let err = etna_tx_bytes_budget(600, 603).expect_err("603 bytes cannot hold an empty block");
        assert!(err.to_string().contains("maxBlockBytes"), "{err}");
    }

    #[test]
    fn ensure_within_max_block_bytes_is_exact() {
        let (parent, attrs) = etna_job();
        let block =
            SealedBlock::seal_slow(assemble_etna_block(&parent, &attrs, 21_000, 100_000_000));
        let exact = execution_block_length(block.header(), &block.body().transactions) as u64;

        assert!(ensure_within_max_block_bytes(&block, exact).is_ok());
        let message = ensure_within_max_block_bytes(&block, exact - 1)
            .expect_err("one byte over maxBlockBytes must fail the build")
            .to_string();
        assert!(message.contains("maxBlockBytes"), "{message}");
        assert!(message.contains(&exact.to_string()), "{message}");
        assert!(message.contains(&(exact - 1).to_string()), "{message}");
        assert!(message.contains(&format!("{}-byte header", block.header().length())), "{message}");
    }

    #[test]
    fn pool_gas_limit_keeps_the_anchor_reserve_only_before_etna() {
        let spec = chain_spec_with_etna_at(100);
        assert_eq!(pool_gas_limit(&spec, 99, 1_500_000), 500_000);
        assert_eq!(pool_gas_limit(&spec, 99, 999_999), 0);
        assert_eq!(pool_gas_limit(&spec, 100, 1_500_000), 1_500_000);
    }

    #[test]
    fn etna_pool_transaction_source_normalizes_without_an_anchor() {
        let mut config = test_payload_config(None);
        config.attributes.max_block_bytes = Some(124_000);
        let attributes = normalize_payload_config(&config, &chain_spec_with_etna_at(100))
            .expect("Etna config should normalize");

        assert!(attributes.transactions.is_none());
        assert!(attributes.anchor_transaction.is_none());
        assert!(attributes.forced_transaction.is_none());
        assert_eq!(attributes.max_block_bytes, Some(124_000));
    }

    #[test]
    fn etna_normalization_rejects_invalid_attributes() {
        type Mutation = fn(&mut PayloadConfig<TaikoPayloadAttributes>);
        // The Engine API test `invalid_etna_attributes_never_start_jobs_or_publish_origins` covers
        // the zero root, the timestamp mismatch, and short extraData through `try_new_for_fork`.
        let cases: [(&str, Mutation); 3] = [
            ("timestamp", |c| {
                c.attributes.block_metadata.timestamp = U256::from(100) + (U256::from(1) << 128)
            }),
            ("must not include anchor", |c| {
                c.attributes.anchor_transaction = Some(Bytes::from_static(&[0x01]))
            }),
            ("withdrawals", |c| {
                c.attributes.payload_attributes.withdrawals = Some(vec![Withdrawal::default()])
            }),
        ];
        let spec = chain_spec_with_etna_at(100);
        for (index, (expected, mutate)) in cases.into_iter().enumerate() {
            let mut config = test_payload_config(None);
            config.attributes.max_block_bytes = Some(124_000);
            mutate(&mut config);
            match normalize_payload_config(&config, &spec) {
                Ok(_) => panic!("case {index} ({expected}) must be rejected"),
                Err(err) => {
                    assert!(err.to_string().contains(expected), "case {index}: {err}")
                }
            }
        }
    }

    #[test]
    fn legacy_normalization_keeps_zero_root_and_anchor_compatibility() {
        let mut config = test_payload_config(None);
        config.attributes.payload_attributes.parent_beacon_block_root = Some(B256::ZERO);
        config.attributes.anchor_transaction = Some(Bytes::from_static(&[0x01]));

        let err = normalize_payload_config(&config, &chain_spec_with_etna_at(101))
            .expect_err("legacy malformed anchor bytes should still reach anchor decoding");
        assert!(err.to_string().contains("anchor"), "unexpected error: {err}");
    }

    #[test]
    fn malformed_attrs_still_await_in_progress_on_missing_payload() {
        assert!(matches!(missing_payload_behaviour(), MissingPayloadBehaviour::AwaitInProgress));
    }

    #[test]
    fn amsterdam_activation_rejects_payload_builds() {
        let spec = TaikoChainSpec::from(Genesis {
            config: ChainConfig { amsterdam_time: Some(100), ..Default::default() },
            ..Default::default()
        });

        assert!(ensure_amsterdam_inactive(&spec, 99).is_ok());

        let err = ensure_amsterdam_inactive(&spec, 100)
            .expect_err("Taiko cannot seal an EIP-7928 block access list");
        assert!(err.to_string().contains("Amsterdam"), "unexpected error: {err}");
    }

    #[test]
    fn taiko_mainnet_never_activates_amsterdam() {
        // The guard above only fails closed if the shipped specs genuinely leave Amsterdam
        // unscheduled; otherwise it would reject every mainnet build.
        assert!(ensure_amsterdam_inactive(&TAIKO_MAINNET, u64::MAX).is_ok());
    }

    #[test]
    fn malformed_attrs_still_reject_empty_payloads_with_missing_payload() {
        let mut _config = test_payload_config(None);
        _config.attributes.anchor_transaction = Some(Bytes::from(vec![0x01]));
        let err = empty_payload_result().expect_err("Taiko should not build empty payloads");

        assert!(matches!(err, PayloadBuilderError::MissingPayload));
    }

    #[test]
    fn valid_attrs_still_reject_empty_payloads_with_missing_payload() {
        let _config = test_payload_config(None);

        let err = empty_payload_result().expect_err("Taiko should not build empty payloads");
        assert!(matches!(err, PayloadBuilderError::MissingPayload));
    }
}
