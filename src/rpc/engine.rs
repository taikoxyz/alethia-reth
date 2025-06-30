use std::sync::Arc;

use alloy_consensus::{BlockHeader, Header};
use alloy_rpc_types_engine::ExecutionData;
use reth::{
    chainspec::ChainSpec, payload::EthereumExecutionPayloadValidator, primitives::RecoveredBlock,
    providers::EthStorage,
};
use reth_ethereum::{Block, EthPrimitives};
use reth_node_api::{
    AddOnsContext, EngineApiMessageVersion, EngineObjectValidationError, EngineValidator,
    FullNodeComponents, InvalidPayloadAttributesError, NewPayloadError, NodeTypes,
    PayloadAttributes, PayloadOrAttributes, PayloadTypes, PayloadValidator,
    validate_version_specific_fields,
};
use reth_node_builder::rpc::EngineValidatorBuilder;
use reth_trie_db::MerklePatriciaTrie;

use crate::payload::{attributes::TaikoPayloadAttributes, engine::TaikoEngineTypes};

/// Builder for [`EthereumEngineValidator`].
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct TaikoEngineValidatorBuilder;

impl<N> EngineValidatorBuilder<N> for TaikoEngineValidatorBuilder
where
    N: FullNodeComponents<
        Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = ChainSpec,
            StateCommitment = MerklePatriciaTrie,
            Storage = EthStorage,
            Payload = TaikoEngineTypes,
        >,
    >,
{
    type Validator = TaikoEngineValidator;

    async fn build(self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::Validator> {
        Ok(TaikoEngineValidator::new(ctx.config.chain.clone()))
    }
}

/// Validator for the Taiko engine API.
#[derive(Debug, Clone)]
pub struct TaikoEngineValidator {
    inner: EthereumExecutionPayloadValidator<ChainSpec>,
}

impl TaikoEngineValidator {
    /// Instantiates a new validator.
    pub const fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self {
            inner: EthereumExecutionPayloadValidator::new(chain_spec),
        }
    }

    /// Returns the chain spec used by the validator.
    #[inline]
    fn chain_spec(&self) -> &ChainSpec {
        self.inner.chain_spec()
    }
}

impl PayloadValidator for TaikoEngineValidator {
    type Block = Block;
    type ExecutionData = ExecutionData;

    fn ensure_well_formed_payload(
        &self,
        payload: ExecutionData,
    ) -> Result<RecoveredBlock<Self::Block>, NewPayloadError> {
        let sealed_block = self.inner.ensure_well_formed_payload(payload)?;
        sealed_block
            .try_recover()
            .map_err(|e| NewPayloadError::Other(e.into()))
    }
}

impl<Types> EngineValidator<Types> for TaikoEngineValidator
where
    Types: PayloadTypes<PayloadAttributes = TaikoPayloadAttributes, ExecutionData = ExecutionData>,
{
    fn validate_version_specific_fields(
        &self,
        version: EngineApiMessageVersion,
        payload_or_attrs: PayloadOrAttributes<'_, Self::ExecutionData, TaikoPayloadAttributes>,
    ) -> Result<(), EngineObjectValidationError> {
        validate_version_specific_fields(self.chain_spec(), version, payload_or_attrs)
    }

    fn ensure_well_formed_attributes(
        &self,
        version: EngineApiMessageVersion,
        attributes: &TaikoPayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        validate_version_specific_fields(
            self.chain_spec(),
            version,
            PayloadOrAttributes::<Self::ExecutionData, TaikoPayloadAttributes>::PayloadAttributes(
                attributes,
            ),
        )
    }

    fn validate_payload_attributes_against_header(
        &self,
        attributes: &TaikoPayloadAttributes,
        header: &Header,
    ) -> Result<(), InvalidPayloadAttributesError> {
        // We allow the payload attributes to have a timestamp that is equal to the parent header's timestamp.
        if attributes.payload_attributes.timestamp() < header.timestamp() {
            return Err(InvalidPayloadAttributesError::InvalidTimestamp);
        }
        Ok(())
    }
}
