use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_primitives::{
    engine::{TaikoEngineTypes, types::TaikoExecutionData},
    payload::attributes::TaikoPayloadAttributes,
};
use alloy_consensus::{BlockHeader, EMPTY_ROOT_HASH};
use alloy_rpc_types_engine::{ExecutionPayloadV1, PayloadError};
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
    rpc::{EngineValidatorBuilder, PayloadValidatorBuilder},
};
use reth_payload_primitives::{
    EngineApiMessageVersion, EngineObjectValidationError, InvalidPayloadAttributesError,
    PayloadAttributes, PayloadOrAttributes,
};
use reth_primitives_traits::Block as BlockTrait;
use std::sync::Arc;

/// Builder for [`TaikoEngineValidator`].
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
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
        ))
    }
}

/// Validator for the Taiko engine API.
#[derive(Debug, Clone)]
pub struct TaikoEngineValidator {
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

    /// Ensures that the given payload does not violate any consensus rules that concern the block's
    /// layout.
    ///
    /// This function must convert the payload into the executable block and pre-validate its
    /// fields.
    fn ensure_well_formed_payload(
        &self,
        payload: Types::ExecutionData,
    ) -> Result<RecoveredBlock<Self::Block>, NewPayloadError> {
        let TaikoExecutionData { execution_payload: payload, taiko_sidecar } = payload;

        let expected_hash = payload.block_hash;

        // First parse the block.
        let mut block = Into::<ExecutionPayloadV1>::into(payload).try_into_block()?;
        if !taiko_sidecar.tx_hash.is_zero() {
            block.header.transactions_root = taiko_sidecar.tx_hash;
        }
        if let Some(withdrawals_hash) = taiko_sidecar.withdrawals_hash {
            if !withdrawals_hash.is_zero() {
                block.header.withdrawals_root = taiko_sidecar.withdrawals_hash;
            } else {
                block.header.withdrawals_root = Some(EMPTY_ROOT_HASH);
            }
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
