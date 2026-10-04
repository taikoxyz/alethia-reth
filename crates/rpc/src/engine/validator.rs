//! Engine payload validator implementation for Taiko execution payloads.
use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::{hardfork::TaikoHardforks, spec::TaikoChainSpec};
use alethia_reth_primitives::{
    engine::{TaikoEngineTypes, types::TaikoExecutionData},
    etna::validate_etna_root,
    payload::{attributes::TaikoPayloadAttributes, builder::TaikoPayloadBuilderAttributes},
    transaction::is_allowed_tx_type,
};
use alloy_consensus::{BlockHeader, EMPTY_ROOT_HASH};
use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3, PayloadError,
};
use alloy_rpc_types_eth::Withdrawals;
use reth::{chainspec::EthChainSpec, primitives::RecoveredBlock};
use reth_chain_state::StateTrieOverlayManager;
use reth_engine_primitives::EngineApiValidator;
use reth_engine_tree::tree::{TreeConfig, payload_validator::BasicEngineValidator};
use reth_ethereum::{Block, EthPrimitives};
use reth_evm::ConfigureEngineEvm;
use reth_node_api::{
    AddOnsContext, FullNodeComponents, NewPayloadError, NodeTypes, PayloadTypes, PayloadValidator,
};
use reth_node_builder::{
    invalid_block_hook::InvalidBlockHookExt,
    rpc::{ChangesetCache, EngineValidatorBuilder, PayloadValidatorBuilder},
};
use reth_payload_primitives::{
    EngineApiMessageVersion, EngineObjectValidationError, InvalidPayloadAttributesError,
    PayloadAttributes, PayloadOrAttributes, VersionSpecificValidationError,
};
use reth_primitives_traits::{Block as BlockTrait, SealedBlock};
use std::sync::Arc;

/// Taiko-specific payload validation errors that do not map to an upstream Ethereum fork rule.
#[derive(Debug, thiserror::Error)]
enum TaikoPayloadValidationError {
    /// Required Etna payload data is absent or violates the Osaka body contract.
    #[error("invalid Etna payload: {0}")]
    InvalidEtnaPayload(&'static str),
    /// The payload contains blob transactions, which Taiko network never accepts.
    #[error("blob transactions are unsupported")]
    BlobTransactionsUnsupported,
    /// Unzen payloads must carry the original header difficulty through the Taiko sidecar.
    #[error("missing header difficulty for Unzen payload")]
    MissingUnzenHeaderDifficulty,
    /// Taiko payload construction does not consume the post-Amsterdam target-gas-limit attribute.
    #[error("target gas limit is unsupported on Taiko")]
    TargetGasLimitUnsupported,
    /// Before Etna, payload conversion rebuilds Unzen headers with the zero root, so a nonzero
    /// build root would disagree with the reconstructed header commitment.
    #[error("non-zero parent beacon block roots are unsupported on Taiko")]
    NonZeroParentBeaconBlockRootUnsupported,
    /// Legacy conversion discards Osaka fields, which must carry only empty bodies and zero
    /// gas/root values to avoid executing data absent from the reconstructed header.
    #[error("nonempty or nonzero Osaka fields are unsupported before Etna")]
    LegacyOsakaFieldsUnsupported,
    /// Taiko schedules no Amsterdam fork, so EIP-7928 block access lists are never accepted.
    #[error("block access lists are unsupported on Taiko")]
    BlockAccessListUnsupported,
    /// Taiko payloads never carry the post-Amsterdam slot number.
    #[error("slot numbers are unsupported on Taiko")]
    SlotNumberUnsupported,
}

/// Builder for [`TaikoEngineValidator`].
#[derive(Debug, Default, Clone)]
pub struct TaikoEngineValidatorBuilder;

impl<N> PayloadValidatorBuilder<N> for TaikoEngineValidatorBuilder
where
    N: FullNodeComponents<Evm = TaikoEvmConfig>,
    N::Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = TaikoChainSpec,
            Payload = TaikoEngineTypes,
        >,
{
    /// The consensus implementation to build.
    type Validator = TaikoEngineValidator;

    /// Creates the engine validator.
    async fn build(self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::Validator> {
        Ok(TaikoEngineValidator::new(ctx.config.chain.clone()))
    }
}

impl<N> EngineValidatorBuilder<N> for TaikoEngineValidatorBuilder
where
    N: FullNodeComponents<Evm = TaikoEvmConfig>,
    N::Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = TaikoChainSpec,
            Payload = TaikoEngineTypes,
        >,
    N::Evm: ConfigureEngineEvm<TaikoExecutionData>,
{
    /// The tree validator type that will be used by the consensus engine.
    type EngineValidator = BasicEngineValidator<N::Provider, N::Evm, TaikoEngineValidator>;

    /// Builds the tree validator for the consensus engine.
    async fn build_tree_validator(
        self,
        ctx: &AddOnsContext<'_, N>,
        tree_config: TreeConfig,
        changeset_cache: ChangesetCache,
        state_trie_overlays: StateTrieOverlayManager<<N::Types as NodeTypes>::Primitives>,
    ) -> eyre::Result<Self::EngineValidator> {
        let validator = <Self as PayloadValidatorBuilder<N>>::build(self, ctx).await?;
        let data_dir = ctx.config.datadir.clone().resolve_datadir(ctx.config.chain.chain());
        let invalid_block_hook = ctx.create_invalid_block_hook(&data_dir).await?;
        Ok(BasicEngineValidator::new(
            ctx.node.provider().clone(),
            Arc::new(ctx.node.consensus().clone()),
            ctx.node.evm_config().clone(),
            validator,
            tree_config,
            invalid_block_hook,
            changeset_cache,
            state_trie_overlays,
            ctx.node.task_executor().clone(),
        ))
    }
}

/// Validator for the Taiko engine API.
#[derive(Debug, Clone)]
pub struct TaikoEngineValidator {
    /// Chain spec used for payload and attribute validation rules.
    pub chain_spec: Arc<TaikoChainSpec>,
}

impl TaikoEngineValidator {
    /// Instantiates a new validator.
    pub const fn new(chain_spec: Arc<TaikoChainSpec>) -> Self {
        Self { chain_spec }
    }

    /// Checks Etna body and sidecar invariants on both Engine RPC and direct tree submissions.
    fn validate_etna_payload(&self, payload: &TaikoExecutionData) -> Result<(), NewPayloadError> {
        let invalid = |reason| {
            NewPayloadError::other(TaikoPayloadValidationError::InvalidEtnaPayload(reason))
        };
        let sidecar = &payload.taiko_sidecar;
        let osaka = sidecar.osaka.as_ref().ok_or_else(|| invalid("missing Osaka fields"))?;
        validate_etna_root(
            true,
            payload.execution_payload.block_number,
            Some(osaka.parent_beacon_block_root),
        )
        .map_err(NewPayloadError::other)?;
        if sidecar.header_difficulty.is_none() {
            return Err(invalid("missing header difficulty"));
        }
        let transactions = payload
            .execution_payload
            .transactions
            .as_ref()
            .ok_or_else(|| invalid("missing complete transaction array"))?;
        if !osaka.withdrawals.is_empty() {
            return Err(invalid("withdrawals must be empty"));
        }
        if osaka.blob_gas_used != 0 || osaka.excess_blob_gas != 0 {
            return Err(invalid("blob gas must be zero"));
        }
        if !osaka.expected_blob_versioned_hashes.is_empty() {
            return Err(invalid("blob versioned hashes must be empty"));
        }
        if !osaka.execution_requests.is_empty() {
            return Err(invalid("execution requests must be empty"));
        }
        if sidecar.tx_hash != alloy_consensus::proofs::ordered_trie_root_encoded(transactions) ||
            sidecar.withdrawals_hash !=
                Some(alloy_consensus::proofs::calculate_withdrawals_root(&osaka.withdrawals))
        {
            return Err(invalid("legacy root overrides must match the actual body"));
        }
        if sidecar.block_access_list.is_some() {
            return Err(NewPayloadError::other(
                TaikoPayloadValidationError::BlockAccessListUnsupported,
            ));
        }
        if sidecar.slot_number.is_some() {
            return Err(NewPayloadError::other(TaikoPayloadValidationError::SlotNumberUnsupported));
        }
        Ok(())
    }
}

impl<Types> PayloadValidator<Types> for TaikoEngineValidator
where
    Types: PayloadTypes<ExecutionData = TaikoExecutionData>,
{
    /// The block type used by the engine.
    type Block = Block;

    /// Converts the given payload into a sealed block without recovering signatures.
    ///
    /// The inbound-only sidecar sentinels are rejected here, not just in
    /// [`EngineApiValidator::validate_version_specific_fields`]: reth's `reth_newPayload`
    /// extension submits payloads straight to the engine tree without the engine-API layer, and
    /// the tree decodes a present block access list into its BAL execution path. This method is
    /// the choke point every payload route funnels through, so failing closed here covers the
    /// bypass.
    fn convert_payload_to_block(
        &self,
        payload: Types::ExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        let is_etna_active = self.chain_spec.is_etna_active(payload.execution_payload.timestamp);
        if is_etna_active {
            self.validate_etna_payload(&payload)?;
        } else if payload.taiko_sidecar.osaka.as_ref().is_some_and(|osaka| {
            !osaka.parent_beacon_block_root.is_zero() ||
                !osaka.withdrawals.is_empty() ||
                osaka.blob_gas_used != 0 ||
                osaka.excess_blob_gas != 0 ||
                !osaka.expected_blob_versioned_hashes.is_empty() ||
                !osaka.execution_requests.is_empty()
        }) {
            // Conversion errors avoid the invalid-header cache. Reth may execute concurrently,
            // so the execution context also ignores legacy sidecar roots. Honest legacy
            // block_to_payload sidecars remain accepted, including withdrawal-only sidecars.
            return Err(NewPayloadError::other(
                TaikoPayloadValidationError::LegacyOsakaFieldsUnsupported,
            ));
        }
        let TaikoExecutionData { execution_payload, taiko_sidecar } = payload;

        if taiko_sidecar.block_access_list.is_some() {
            return Err(NewPayloadError::other(
                TaikoPayloadValidationError::BlockAccessListUnsupported,
            ));
        }
        if taiko_sidecar.slot_number.is_some() {
            return Err(NewPayloadError::other(TaikoPayloadValidationError::SlotNumberUnsupported));
        }

        let expected_hash = execution_payload.block_hash;
        let is_unzen_active = self.chain_spec.is_unzen_active(execution_payload.timestamp);

        if is_unzen_active && taiko_sidecar.header_difficulty.is_none() {
            return Err(NewPayloadError::other(
                TaikoPayloadValidationError::MissingUnzenHeaderDifficulty,
            ));
        }

        let block = if is_etna_active {
            // Validation above requires the sidecar; reconstruct roots from the actual body.
            let osaka = taiko_sidecar.osaka.ok_or_else(|| {
                NewPayloadError::other(TaikoPayloadValidationError::InvalidEtnaPayload(
                    "missing Osaka fields",
                ))
            })?;
            let v3 = ExecutionPayloadV3 {
                payload_inner: ExecutionPayloadV2 {
                    payload_inner: execution_payload.into(),
                    withdrawals: osaka.withdrawals,
                },
                blob_gas_used: osaka.blob_gas_used,
                excess_blob_gas: osaka.excess_blob_gas,
            };
            let mut block = v3.try_into_block()?;
            block.header.difficulty = taiko_sidecar.header_difficulty.ok_or_else(|| {
                NewPayloadError::other(TaikoPayloadValidationError::InvalidEtnaPayload(
                    "missing header difficulty",
                ))
            })?;
            block.header.parent_beacon_block_root = Some(osaka.parent_beacon_block_root);
            block.header.requests_hash = Some(EMPTY_REQUESTS_HASH);
            if block.body.transactions.iter().any(|tx| !is_allowed_tx_type(tx)) {
                return Err(NewPayloadError::other(
                    TaikoPayloadValidationError::BlobTransactionsUnsupported,
                ));
            }
            block
        } else {
            // First parse the block.
            let mut block = Into::<ExecutionPayloadV1>::into(execution_payload).try_into_block()?;
            if let Some(header_difficulty) = taiko_sidecar.header_difficulty {
                block.header.difficulty = header_difficulty;
            }
            block.header.parent_beacon_block_root = is_unzen_active.then_some(B256::ZERO);
            block.header.blob_gas_used = is_unzen_active.then_some(0);
            block.header.excess_blob_gas = is_unzen_active.then_some(0);
            block.header.requests_hash = is_unzen_active.then_some(EMPTY_REQUESTS_HASH);
            if !taiko_sidecar.tx_hash.is_zero() {
                block.header.transactions_root = taiko_sidecar.tx_hash;
            }
            if let Some(withdrawals_hash) = taiko_sidecar.withdrawals_hash {
                if !withdrawals_hash.is_zero() {
                    block.header.withdrawals_root = taiko_sidecar.withdrawals_hash;
                } else {
                    block.header.withdrawals_root = Some(EMPTY_ROOT_HASH);
                }
                block.body.withdrawals = Some(Withdrawals::default());
            }
            block
        };
        let sealed_block = block.seal_slow();

        // Ensure the hash included in the payload matches the block hash
        if expected_hash != sealed_block.hash() {
            return Err(PayloadError::BlockHash {
                execution: sealed_block.hash(),
                consensus: expected_hash,
            })
            .map_err(|e| NewPayloadError::Other(e.into()));
        }

        Ok(sealed_block)
    }

    /// Ensures that the given payload does not violate any consensus rules that concern the block's
    /// layout.
    ///
    /// This function must convert the payload into the executable block and pre-validate its
    /// fields.
    fn ensure_well_formed_payload(
        &self,
        payload: Types::ExecutionData,
    ) -> Result<RecoveredBlock<Self::Block>, NewPayloadError> {
        let sealed_block =
            <Self as PayloadValidator<Types>>::convert_payload_to_block(self, payload)?;

        if sealed_block.body().transactions().into_iter().any(|tx| !is_allowed_tx_type(tx)) {
            return Err(NewPayloadError::other(
                TaikoPayloadValidationError::BlobTransactionsUnsupported,
            ));
        }

        sealed_block.try_recover().map_err(|e| NewPayloadError::Other(e.into()))
    }

    /// Validates the payload attributes with respect to the header.
    fn validate_payload_attributes_against_header(
        &self,
        attr: &Types::PayloadAttributes,
        header: &<Self::Block as BlockTrait>::Header,
    ) -> Result<(), InvalidPayloadAttributesError> {
        // We allow the payload attributes to have a timestamp that is equal to the parent header's
        // timestamp in Taiko network.
        if attr.timestamp() < header.timestamp() {
            return Err(InvalidPayloadAttributesError::InvalidTimestamp);
        }
        Ok(())
    }
}

// EngineApiValidator implementation for TaikoEngineValidator
impl<Types> EngineApiValidator<Types> for TaikoEngineValidator
where
    Types: PayloadTypes<PayloadAttributes = TaikoPayloadAttributes, ExecutionData = TaikoExecutionData>,
{
    /// Validates the presence or exclusion of fork-specific fields based on the payload attributes
    /// and the message version.
    fn validate_version_specific_fields(
        &self,
        version: EngineApiMessageVersion,
        payload_or_attrs: PayloadOrAttributes<'_, Types::ExecutionData, Types::PayloadAttributes>,
    ) -> Result<(), EngineObjectValidationError> {
        let is_etna_active = self.chain_spec.is_etna_active(payload_or_attrs.timestamp());
        let expected_version = match &payload_or_attrs {
            PayloadOrAttributes::ExecutionPayload(_) if is_etna_active => {
                EngineApiMessageVersion::V4
            }
            PayloadOrAttributes::PayloadAttributes(_) if is_etna_active => {
                EngineApiMessageVersion::V3
            }
            _ => EngineApiMessageVersion::V2,
        };
        if version != expected_version {
            return Err(EngineObjectValidationError::UnsupportedFork);
        }
        let validation_kind = payload_or_attrs.message_validation_kind();

        if payload_or_attrs.block_access_list().is_some() {
            return Err(validation_kind
                .to_error(VersionSpecificValidationError::BlockAccessListNotSupported));
        }
        if payload_or_attrs.slot_number().is_some() {
            return Err(
                validation_kind.to_error(VersionSpecificValidationError::SlotNumberNotSupported)
            );
        }
        if payload_or_attrs.target_gas_limit().is_some() {
            return Err(EngineObjectValidationError::InvalidParams(Box::new(
                TaikoPayloadValidationError::TargetGasLimitUnsupported,
            )));
        }
        if is_etna_active {
            match payload_or_attrs {
                PayloadOrAttributes::ExecutionPayload(payload) => self
                    .validate_etna_payload(payload)
                    .map_err(|err| EngineObjectValidationError::InvalidParams(Box::new(err)))?,
                PayloadOrAttributes::PayloadAttributes(attributes) => {
                    TaikoPayloadBuilderAttributes::try_new_for_fork(
                        B256::ZERO,
                        attributes.clone(),
                        true,
                    )
                    .map_err(|err| EngineObjectValidationError::InvalidParams(Box::new(err)))?;
                }
            }
            return Ok(());
        }
        // Zero (the network invariant) and absent roots are equivalent downstream; only a
        // non-zero root would be silently committed into an unimportable block, so it fails
        // closed here before a payload job can start.
        if payload_or_attrs.parent_beacon_block_root().is_some_and(|root| !root.is_zero()) {
            return Err(EngineObjectValidationError::InvalidParams(Box::new(
                TaikoPayloadValidationError::NonZeroParentBeaconBlockRootUnsupported,
            )));
        }

        Ok(())
    }

    /// Ensures that the payload attributes are valid for the given [`EngineApiMessageVersion`].
    fn ensure_well_formed_attributes(
        &self,
        version: EngineApiMessageVersion,
        attributes: &Types::PayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        <Self as EngineApiValidator<Types>>::validate_version_specific_fields(
            self,
            version,
            PayloadOrAttributes::from_attributes(attributes),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alethia_reth_chainspec::{TAIKO_DEVNET, hardfork::TaikoHardfork};
    use alethia_reth_primitives::{
        engine::{
            TaikoEngineTypes,
            types::{TaikoExecutionData, TaikoExecutionDataSidecar},
        },
        payload::attributes::{RpcL1Origin, TaikoBlockMetadata, TaikoPayloadAttributes},
    };
    use alloy_consensus::{BlockBody, Header, constants::EMPTY_WITHDRAWALS};
    use alloy_eips::merge::BEACON_NONCE;
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::{Address, B256, Bytes, U256};
    use alloy_rpc_types_engine::{ExecutionPayloadV1, PayloadAttributes as EthPayloadAttributes};
    use alloy_rpc_types_eth::Withdrawals;
    use reth_primitives_traits::BlockBody as _;

    fn etna_validator() -> TaikoEngineValidator {
        let mut spec = unzen_chain_spec();
        spec.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(100));
        TaikoEngineValidator::new(Arc::new(spec))
    }

    fn etna_data(difficulty: U256) -> TaikoExecutionData {
        let old = sample_unzen_execution_data(difficulty, Some(difficulty), Some(B256::ZERO));
        let mut block = convert_payload(old).unwrap().into_block();
        block.header.timestamp = 100;
        block.header.parent_beacon_block_root = Some(B256::with_last_byte(42));
        block.header.extra_data = Bytes::from(vec![0; 7]);
        TaikoEngineTypes::block_to_payload(block.seal_slow(), None)
    }

    fn convert_etna(payload: TaikoExecutionData) -> Result<SealedBlock<Block>, NewPayloadError> {
        <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
            &etna_validator(),
            payload,
        )
    }

    fn version_etna(payload: &TaikoExecutionData) -> Result<(), EngineObjectValidationError> {
        <TaikoEngineValidator as EngineApiValidator<TaikoEngineTypes>>::validate_version_specific_fields(&etna_validator(), EngineApiMessageVersion::V4, PayloadOrAttributes::from_execution_payload(payload))
    }

    #[test]
    fn legacy_osaka_sidecars_preserve_honest_blocks_and_reject_discarded_data() {
        let mut spec = unzen_chain_spec();
        spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(50));
        spec.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(100));
        let validator = TaikoEngineValidator::new(Arc::new(spec));
        for timestamp in [49, 99] {
            let mut block = convert_payload(sample_unzen_execution_data(
                U256::ZERO,
                Some(U256::ZERO),
                Some(B256::ZERO),
            ))
            .unwrap()
            .into_block();
            block.header.timestamp = timestamp;
            if timestamp == 49 {
                block.header.parent_beacon_block_root = None;
                block.header.blob_gas_used = None;
                block.header.excess_blob_gas = None;
                block.header.requests_hash = None;
            }
            let block = block.seal_slow();
            let data = TaikoEngineTypes::block_to_payload(block.clone(), None);
            // A pre-Unzen block has an Osaka sidecar solely to preserve withdrawals.
            assert!(data.taiko_sidecar.osaka.is_some());
            let convert = |data| {
                <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                    &validator, data,
                )
            };
            assert_eq!(convert(data.clone()).unwrap(), block);
            let mut absent = data.clone();
            absent.taiko_sidecar.osaka = None;
            assert_eq!(convert(absent).unwrap(), block);
            for case in 0..6 {
                let mut invalid = data.clone();
                let osaka = invalid.taiko_sidecar.osaka.as_mut().unwrap();
                match case {
                    0 => osaka.parent_beacon_block_root = B256::with_last_byte(7),
                    1 => osaka.withdrawals.push(Default::default()),
                    2 => osaka.blob_gas_used = 1,
                    3 => osaka.excess_blob_gas = 1,
                    4 => osaka.expected_blob_versioned_hashes.push(B256::with_last_byte(1)),
                    _ => osaka.execution_requests.push(Bytes::from_static(&[0, 1])),
                }
                assert!(convert(invalid).is_err(), "timestamp {timestamp}, case {case}");
            }
        }
    }

    #[test]
    fn etna_invalid_sidecar_mutations_fail_rpc_and_direct_tree_validation() {
        for case in 0..12 {
            let mut data = etna_data(U256::ZERO);
            match case {
                0 => data.taiko_sidecar.osaka = None,
                1 => {
                    data.taiko_sidecar.osaka.as_mut().unwrap().parent_beacon_block_root = B256::ZERO
                }
                2 => data.taiko_sidecar.header_difficulty = None,
                3 => data.execution_payload.transactions = None,
                4 => {
                    data.taiko_sidecar.osaka.as_mut().unwrap().withdrawals.push(Default::default())
                }
                5 => data.taiko_sidecar.osaka.as_mut().unwrap().blob_gas_used = 1,
                6 => data.taiko_sidecar.osaka.as_mut().unwrap().excess_blob_gas = 1,
                7 => data
                    .taiko_sidecar
                    .osaka
                    .as_mut()
                    .unwrap()
                    .expected_blob_versioned_hashes
                    .push(B256::with_last_byte(1)),
                8 => data
                    .taiko_sidecar
                    .osaka
                    .as_mut()
                    .unwrap()
                    .execution_requests
                    .push(Bytes::from_static(&[1])),
                9 => data.taiko_sidecar.block_access_list = Some(Bytes::new()),
                10 => data.taiko_sidecar.slot_number = Some(1),
                _ => data.taiko_sidecar.tx_hash = B256::with_last_byte(1),
            }
            assert!(version_etna(&data).is_err(), "RPC accepted case {case}");
            assert!(convert_etna(data).is_err(), "direct tree accepted case {case}");
        }
        let mut data = etna_data(U256::ZERO);
        data.taiko_sidecar.withdrawals_hash = Some(B256::with_last_byte(1));
        assert!(version_etna(&data).is_err());
        assert!(convert_etna(data).is_err());
    }

    #[test]
    fn etna_original_hash_is_checked_after_restoring_root_and_difficulty() {
        let mut data = etna_data(U256::from(91));
        data.execution_payload.block_hash = B256::with_last_byte(1);
        assert!(convert_etna(data).unwrap_err().to_string().contains("block hash"));
        let mut data = etna_data(U256::from(91));
        data.taiko_sidecar.header_difficulty = Some(U256::from(92));
        assert!(convert_etna(data).is_err());
        // A self-consistent declared difficulty survives conversion unchanged. Actual zk-gas
        // equality belongs to the shared executor's post-execution validation, not this layer.
        let data = etna_data(U256::from(92));
        assert_eq!(convert_etna(data).unwrap().difficulty, U256::from(92));
    }

    #[test]
    fn etna_direct_blocks_reject_withdrawal_only_and_partial_osaka_inputs() {
        let original = convert_etna(etna_data(U256::ZERO)).unwrap().into_block();
        for case in 0..7 {
            let mut block = original.clone();
            match case {
                0 => {
                    block.body.withdrawals =
                        Some(vec![alloy_rpc_types_eth::Withdrawal::default()].into());
                    block.header.withdrawals_root =
                        Some(alloy_consensus::proofs::calculate_withdrawals_root(
                            block.body.withdrawals.as_ref().unwrap(),
                        ));
                    block.header.parent_beacon_block_root = None;
                    block.header.blob_gas_used = None;
                    block.header.excess_blob_gas = None;
                    block.header.requests_hash = None;
                }
                1 => block.header.parent_beacon_block_root = None,
                2 => block.header.blob_gas_used = None,
                3 => block.header.excess_blob_gas = None,
                4 => block.header.requests_hash = None,
                5 => {
                    block.header.withdrawals_root = None;
                    block.body.withdrawals = None;
                }
                _ => block.header.requests_hash = Some(B256::with_last_byte(9)),
            }
            let data = TaikoEngineTypes::block_to_payload(block.seal_slow(), None);
            if case == 0 {
                assert_eq!(data.taiko_sidecar.osaka.as_ref().unwrap().withdrawals.len(), 1);
                assert!(version_etna(&data).is_err());
            }
            assert!(
                convert_etna(data).is_err(),
                "partial input case {case} was silently reconstructed"
            );
        }
    }

    #[test]
    fn etna_blob_transaction_rejected_without_versioned_hash_side_parameters() {
        use alloy_eips::eip2718::Encodable2718;
        let tx = reth_ethereum::TransactionSigned::new_unhashed(
            alloy_consensus::TxEip4844::default().into(),
            alloy_primitives::Signature::new(U256::from(1), U256::from(2), false),
        );
        let mut block = convert_etna(etna_data(U256::ZERO)).unwrap().into_block();
        block.body.transactions.push(tx.clone());
        block.header.transactions_root =
            alloy_consensus::proofs::calculate_transaction_root(&block.body.transactions);
        let mut data = TaikoEngineTypes::block_to_payload(block.seal_slow(), None);
        data.execution_payload.transactions = Some(vec![tx.encoded_2718().into()]);
        data.taiko_sidecar.osaka.as_mut().unwrap().expected_blob_versioned_hashes.clear();
        assert!(convert_etna(data).unwrap_err().to_string().contains("blob transactions"));
    }

    #[test]
    fn etna_genesis_retains_canonical_zero_root_exception() {
        let mut block = convert_etna(etna_data(U256::ZERO)).unwrap().into_block();
        block.header.number = 0;
        block.header.parent_beacon_block_root = Some(B256::ZERO);
        let data = TaikoEngineTypes::block_to_payload(block.seal_slow(), None);
        version_etna(&data).unwrap();
        assert_eq!(convert_etna(data).unwrap().parent_beacon_block_root, Some(B256::ZERO));
    }

    #[test]
    fn etna_version_matrix_uses_target_timestamp() {
        for timestamp in [99, 100] {
            for version in [EngineApiMessageVersion::V2, EngineApiMessageVersion::V4] {
                let mut data = etna_data(U256::ZERO);
                data.execution_payload.timestamp = timestamp;
                if timestamp == 99 {
                    data.taiko_sidecar.osaka = None;
                }
                let result = <TaikoEngineValidator as EngineApiValidator<TaikoEngineTypes>>::validate_version_specific_fields(
                    &etna_validator(), version, PayloadOrAttributes::from_execution_payload(&data),
                );
                assert_eq!(
                    result.is_ok(),
                    (timestamp == 99) == (version == EngineApiMessageVersion::V2),
                    "timestamp={timestamp}, version={version:?}: {result:?}"
                );
            }
        }
    }

    #[test]
    fn formats_blob_transactions_unsupported_error() {
        assert_eq!(
            TaikoPayloadValidationError::BlobTransactionsUnsupported.to_string(),
            "blob transactions are unsupported"
        );
    }

    #[test]
    fn rejects_unzen_payload_without_header_difficulty() {
        let validator = TaikoEngineValidator::new(Arc::new(unzen_chain_spec()));
        let payload = sample_unzen_execution_data(U256::from(7_u64), None, None);

        let err =
            <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                &validator, payload,
            )
            .expect_err("Unzen payloads must supply header difficulty explicitly");

        assert_eq!(err.to_string(), "missing header difficulty for Unzen payload");
    }

    #[test]
    fn accepts_unzen_payload_when_sidecar_supplies_header_difficulty() {
        let validator = TaikoEngineValidator::new(Arc::new(unzen_chain_spec()));
        let payload = sample_unzen_execution_data(
            U256::from(7_u64),
            Some(U256::from(7_u64)),
            Some(B256::ZERO),
        );

        let sealed =
            <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                &validator,
                payload.clone(),
            )
            .expect("explicit header difficulty should restore the original hash");

        assert_eq!(sealed.hash(), payload.execution_payload.block_hash);
        assert_eq!(sealed.header().difficulty, U256::from(7_u64));
    }

    #[test]
    fn accepts_unzen_payload_when_validator_infers_parent_beacon_block_root() {
        let validator = TaikoEngineValidator::new(Arc::new(unzen_chain_spec()));
        let payload = sample_unzen_execution_data(
            U256::from(7_u64),
            Some(U256::from(7_u64)),
            Some(B256::ZERO),
        );

        let sealed =
            <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                &validator,
                payload.clone(),
            )
            .expect("validator should infer parent beacon block root from Unzen activation");

        assert_eq!(sealed.hash(), payload.execution_payload.block_hash);
        assert_eq!(sealed.header().parent_beacon_block_root, Some(B256::ZERO));
        assert_eq!(sealed.header().blob_gas_used, Some(0));
        assert_eq!(sealed.header().excess_blob_gas, Some(0));
        assert_eq!(sealed.header().requests_hash, Some(EMPTY_REQUESTS_HASH));
    }

    #[test]
    fn rejects_non_zero_parent_beacon_block_root_in_payload_attributes() {
        let mut attributes = sample_payload_attributes();
        attributes.payload_attributes.parent_beacon_block_root = Some(B256::repeat_byte(0x33));

        let error = validate_payload_attributes(&attributes)
            .expect_err("a non-zero beacon root cannot round-trip and must fail closed");
        assert!(error.to_string().contains("non-zero parent beacon block roots"));

        // The zero root is the network invariant and must stay valid — the shared fixture
        // already carries `Some(B256::ZERO)`.
        validate_payload_attributes(&sample_payload_attributes())
            .expect("the zero beacon root must remain accepted");
    }

    #[test]
    fn rejects_slot_number_in_v2_payload_attributes_json() {
        let attributes =
            payload_attributes_with_extra_field("slotNumber", serde_json::json!("0x1"));

        let error = validate_payload_attributes(&attributes)
            .expect_err("Taiko V2 payload attributes must reject slotNumber");

        assert_eq!(
            error.to_string(),
            "Payload attributes validation error: slot number not supported in this engine API version"
        );
    }

    #[test]
    fn rejects_target_gas_limit_in_v2_payload_attributes_json() {
        let attributes =
            payload_attributes_with_extra_field("targetGasLimit", serde_json::json!("0x1c9c380"));

        let error = validate_payload_attributes(&attributes)
            .expect_err("Taiko V2 payload attributes must reject targetGasLimit");

        assert_eq!(error.to_string(), "Invalid params: target gas limit is unsupported on Taiko");
    }

    #[test]
    fn rejects_block_access_list_in_v2_execution_payload_json() {
        let payload = execution_data_with_extra_field("blockAccessList", serde_json::json!("0xc0"));

        let error = validate_execution_payload(&payload)
            .expect_err("Taiko V2 execution payloads must reject blockAccessList");

        assert_eq!(
            error.to_string(),
            "Payload validation error: block access list not supported in this engine API version"
        );
    }

    #[test]
    fn rejects_slot_number_in_v2_execution_payload_json() {
        let payload = execution_data_with_extra_field("slotNumber", serde_json::json!("0x1"));

        let error = validate_execution_payload(&payload)
            .expect_err("Taiko V2 execution payloads must reject slotNumber");

        assert_eq!(
            error.to_string(),
            "Payload validation error: slot number not supported in this engine API version"
        );
    }

    #[test]
    fn rejects_block_access_list_supplied_through_block_to_payload() {
        // reth's `reth_newPayload` BlockRlp arm (mounted unconditionally on the auth server)
        // and the debug consensus clients convert caller input through
        // `PayloadTypes::block_to_payload`, so the sidecar sentinel must carry the caller's
        // block access list into validation instead of silently dropping it.
        let bal = Bytes::from_static(&[0xc0]);
        let block = Block::new(
            Header::default(),
            BlockBody { transactions: Vec::new(), ommers: Vec::new(), withdrawals: None },
        );

        let payload = TaikoEngineTypes::block_to_payload(
            SealedBlock::new_unhashed(block.clone()),
            Some(bal.clone()),
        );
        assert_eq!(
            payload.taiko_sidecar.block_access_list,
            Some(bal),
            "block_to_payload must preserve a caller-supplied block access list"
        );

        // The engine-API layer rejects it on the `engine_newPayloadVx` route.
        let error = validate_execution_payload(&payload)
            .expect_err("a caller-supplied block access list must fail closed");
        assert_eq!(
            error.to_string(),
            "Payload validation error: block access list not supported in this engine API version"
        );

        // The engine-tree conversion — the route `reth_newPayload` reaches without the
        // engine-API layer — must fail closed as well.
        let error = convert_payload(payload)
            .expect_err("the engine-tree conversion route must also reject the BAL");
        assert_eq!(error.to_string(), "block access lists are unsupported on Taiko");

        let bal_free = TaikoEngineTypes::block_to_payload(SealedBlock::new_unhashed(block), None);
        validate_execution_payload(&bal_free)
            .expect("conversions without a block access list must remain valid");
    }

    #[test]
    fn rejects_inbound_sentinels_on_the_engine_tree_route() {
        // reth's `reth_newPayload` ExecutionData arm accepts a caller-supplied sidecar and
        // bypasses `validate_version_specific_fields` entirely, so the sentinels must be
        // rejected by the conversion itself.
        let with_bal =
            execution_data_with_extra_field("blockAccessList", serde_json::json!("0xc0"));
        let error = convert_payload(with_bal)
            .expect_err("a payload-borne block access list must fail closed at conversion");
        assert_eq!(error.to_string(), "block access lists are unsupported on Taiko");

        let with_slot = execution_data_with_extra_field("slotNumber", serde_json::json!("0x1"));
        let error = convert_payload(with_slot)
            .expect_err("a payload-borne slot number must fail closed at conversion");
        assert_eq!(error.to_string(), "slot numbers are unsupported on Taiko");
    }

    fn convert_payload(payload: TaikoExecutionData) -> Result<SealedBlock<Block>, NewPayloadError> {
        <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
            &TaikoEngineValidator::new(Arc::new(unzen_chain_spec())),
            payload,
        )
    }

    fn validate_payload_attributes(
        attributes: &TaikoPayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        <TaikoEngineValidator as EngineApiValidator<TaikoEngineTypes>>::
            validate_version_specific_fields(
                &TaikoEngineValidator::new(Arc::new(unzen_chain_spec())),
                EngineApiMessageVersion::V2,
                PayloadOrAttributes::from_attributes(attributes),
            )
    }

    fn validate_execution_payload(
        payload: &TaikoExecutionData,
    ) -> Result<(), EngineObjectValidationError> {
        <TaikoEngineValidator as EngineApiValidator<TaikoEngineTypes>>::
            validate_version_specific_fields(
                &TaikoEngineValidator::new(Arc::new(unzen_chain_spec())),
                EngineApiMessageVersion::V2,
                PayloadOrAttributes::from_execution_payload(payload),
            )
    }

    fn payload_attributes_with_extra_field(
        field: &str,
        value: serde_json::Value,
    ) -> TaikoPayloadAttributes {
        let mut json = serde_json::to_value(sample_payload_attributes())
            .expect("sample payload attributes must serialize");
        json.as_object_mut()
            .expect("payload attributes must serialize as an object")
            .insert(field.to_owned(), value);
        serde_json::from_value(json).expect("payload attributes with fork field must deserialize")
    }

    fn execution_data_with_extra_field(
        field: &str,
        value: serde_json::Value,
    ) -> TaikoExecutionData {
        let mut json = serde_json::to_value(sample_unzen_execution_data(
            U256::from(7_u64),
            Some(U256::from(7_u64)),
            Some(B256::ZERO),
        ))
        .expect("sample execution data must serialize");
        json.as_object_mut()
            .expect("execution data must serialize as an object")
            .insert(field.to_owned(), value);
        serde_json::from_value(json).expect("execution data with fork field must deserialize")
    }

    fn sample_payload_attributes() -> TaikoPayloadAttributes {
        TaikoPayloadAttributes {
            payload_attributes: EthPayloadAttributes {
                timestamp: 1,
                prev_randao: B256::with_last_byte(0x11),
                suggested_fee_recipient: Address::with_last_byte(0x22),
                withdrawals: Some(Vec::new()),
                parent_beacon_block_root: Some(B256::ZERO),
                slot_number: None,
                target_gas_limit: None,
            },
            base_fee_per_gas: U256::from(1_u64),
            block_metadata: TaikoBlockMetadata {
                beneficiary: Address::with_last_byte(0x33),
                gas_limit: 30_000_000,
                timestamp: U256::from(1_u64),
                mix_hash: B256::with_last_byte(0x44),
                tx_list: None,
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
            anchor_transaction: None,
        }
    }

    fn unzen_chain_spec() -> TaikoChainSpec {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(0));
        chain_spec
    }

    fn sample_unzen_execution_data(
        difficulty: U256,
        header_difficulty: Option<U256>,
        parent_beacon_block_root: Option<B256>,
    ) -> TaikoExecutionData {
        let block = reth_ethereum::Block {
            header: Header {
                parent_hash: B256::with_last_byte(0x11),
                beneficiary: Address::with_last_byte(0x22),
                state_root: B256::with_last_byte(0x33),
                transactions_root: alloy_consensus::proofs::calculate_transaction_root(&Vec::<
                    reth_ethereum::TransactionSigned,
                >::new(
                )),
                receipts_root: B256::with_last_byte(0x44),
                withdrawals_root: Some(EMPTY_WITHDRAWALS),
                logs_bloom: Default::default(),
                number: 1,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: 1,
                mix_hash: B256::with_last_byte(0x55),
                nonce: BEACON_NONCE.into(),
                base_fee_per_gas: Some(1),
                extra_data: Bytes::default(),
                difficulty,
                parent_beacon_block_root,
                blob_gas_used: Some(0),
                excess_blob_gas: Some(0),
                requests_hash: Some(EMPTY_REQUESTS_HASH),
                ..Default::default()
            },
            body: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: Some(Withdrawals::default()),
            },
        };
        let block_hash = block.header.hash_slow();
        let execution_payload = ExecutionPayloadV1::from_block_unchecked(block_hash, &block);

        TaikoExecutionData {
            execution_payload: execution_payload.into(),
            taiko_sidecar: TaikoExecutionDataSidecar {
                tx_hash: block.body.calculate_tx_root(),
                withdrawals_hash: Some(EMPTY_WITHDRAWALS),
                header_difficulty,
                taiko_block: Some(true),
                block_access_list: None,
                slot_number: None,
                osaka: None,
            },
        }
    }
}
