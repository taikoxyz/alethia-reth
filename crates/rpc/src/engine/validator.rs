//! Engine payload validator implementation for Taiko execution payloads.
use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_primitives::{
    engine::{TaikoEngineTypes, types::TaikoExecutionData},
    payload::attributes::TaikoPayloadAttributes,
    transaction::is_allowed_tx_type,
};
use alloy_consensus::{BlockHeader, EMPTY_ROOT_HASH};
use alloy_rpc_types_engine::{ExecutionPayloadV1, PayloadError};
use alloy_rpc_types_eth::Withdrawals;
use reth::{chainspec::EthChainSpec, primitives::RecoveredBlock};
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
    PayloadAttributes, PayloadOrAttributes,
};
use reth_primitives_traits::{Block as BlockTrait, SealedBlock};
use std::sync::Arc;

/// Taiko-specific payload validation errors that do not map to an upstream Ethereum fork rule.
#[derive(Debug, thiserror::Error)]
enum TaikoPayloadValidationError {
    /// The payload contains blob transactions, which Taiko network never accepts.
    #[error("blob transactions are unsupported")]
    BlobTransactionsUnsupported,
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
}

impl<Types> PayloadValidator<Types> for TaikoEngineValidator
where
    Types: PayloadTypes<ExecutionData = TaikoExecutionData>,
{
    /// The block type used by the engine.
    type Block = Block;

    /// Converts the given payload into a sealed block without recovering signatures.
    fn convert_payload_to_block(
        &self,
        payload: Types::ExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        let TaikoExecutionData { execution_payload, taiko_sidecar } = payload;

        let expected_hash = execution_payload.block_hash;

        // First parse the block.
        let mut block = Into::<ExecutionPayloadV1>::into(execution_payload).try_into_block()?;
        if let Some(header_difficulty) = taiko_sidecar.header_difficulty {
            block.header.difficulty = header_difficulty;
        }
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
        _version: EngineApiMessageVersion,
        _payload_or_attrs: PayloadOrAttributes<'_, Types::ExecutionData, Types::PayloadAttributes>,
    ) -> Result<(), EngineObjectValidationError> {
        // For Taiko, we don't have version-specific validation
        Ok(())
    }

    /// Ensures that the payload attributes are valid for the given [`EngineApiMessageVersion`].
    fn ensure_well_formed_attributes(
        &self,
        _version: EngineApiMessageVersion,
        _attributes: &Types::PayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        // Attributes are well-formed if they pass the basic validation
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alethia_reth_chainspec::{TAIKO_DEVNET, hardfork::TaikoHardfork};
    use alethia_reth_primitives::engine::{
        TaikoEngineTypes,
        types::{TaikoExecutionData, TaikoExecutionDataSidecar},
    };
    use alloy_consensus::{BlockBody, Header, constants::EMPTY_WITHDRAWALS};
    use alloy_eips::merge::BEACON_NONCE;
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::{Address, B256, Bytes, U256};
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use alloy_rpc_types_eth::Withdrawals;
    use reth_primitives_traits::BlockBody as _;

    #[test]
    fn formats_blob_transactions_unsupported_error() {
        assert_eq!(
            TaikoPayloadValidationError::BlobTransactionsUnsupported.to_string(),
            "blob transactions are unsupported"
        );
    }

    #[test]
    fn rejects_round_tripping_uzen_block_through_payload_v1() {
        let validator = TaikoEngineValidator::new(Arc::new(uzen_chain_spec()));
        let payload = sample_uzen_execution_data(U256::from(7_u64), None);

        let err =
            <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                &validator, payload,
            )
            .expect_err(
                "Uzen blocks currently lose hash-relevant fields during payload conversion",
            );

        assert!(err.to_string().contains("block hash mismatch"));
    }

    #[test]
    fn accepts_uzen_payload_when_sidecar_supplies_header_difficulty() {
        let validator = TaikoEngineValidator::new(Arc::new(uzen_chain_spec()));
        let payload = sample_uzen_execution_data(U256::from(7_u64), Some(U256::from(7_u64)));

        let sealed =
            <TaikoEngineValidator as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                &validator,
                payload.clone(),
            )
            .expect("cached header difficulty should restore the original hash");

        assert_eq!(sealed.hash(), payload.execution_payload.block_hash);
        assert_eq!(sealed.header().difficulty, U256::from(7_u64));
    }

    fn uzen_chain_spec() -> TaikoChainSpec {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Uzen, ForkCondition::Timestamp(0));
        chain_spec
    }

    fn sample_uzen_execution_data(
        difficulty: U256,
        header_difficulty: Option<U256>,
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
            },
        }
    }
}
