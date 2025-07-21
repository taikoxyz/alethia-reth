use alloy_consensus::{BlockHeader, EMPTY_ROOT_HASH, Header};
use alloy_rpc_types_engine::PayloadError;
use reth::primitives::RecoveredBlock;
use reth_ethereum::{Block, EthPrimitives};
use reth_evm::ConfigureEvm;
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_api::{
    AddOnsContext, EngineApiMessageVersion, EngineObjectValidationError, EngineValidator,
    FullNodeComponents, InvalidPayloadAttributesError, NewPayloadError, NodeTypes,
    PayloadAttributes, PayloadOrAttributes, PayloadTypes, PayloadValidator,
};
use reth_node_builder::rpc::EngineValidatorBuilder;
use reth_primitives_traits::Block as SealedBlock;
use std::{convert::Infallible, sync::Arc};

use crate::{
    block::{assembler::TaikoBlockAssembler, factory::TaikoBlockExecutorFactory},
    chainspec::spec::TaikoChainSpec,
    evm::{config::TaikoNextBlockEnvAttributes, factory::TaikoEvmFactory},
    payload::{attributes::TaikoPayloadAttributes, engine::TaikoEngineTypes},
    rpc::engine::types::TaikoExecutionData,
};

/// Builder for [`TaikoEngineValidator`].
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct TaikoEngineValidatorBuilder;

impl<N> EngineValidatorBuilder<N> for TaikoEngineValidatorBuilder
where
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = TaikoChainSpec,
                Payload = TaikoEngineTypes,
            >,
            Evm: ConfigureEvm<
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
        >,
{
    /// The consensus implementation to build.
    type Validator = TaikoEngineValidator;

    /// Creates the engine validator.
    async fn build(self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::Validator> {
        Ok(TaikoEngineValidator::new(ctx.config.chain.clone()))
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

impl PayloadValidator for TaikoEngineValidator {
    /// The block type used by the engine.
    type Block = Block;
    /// The execution payload type used by the engine.
    type ExecutionData = TaikoExecutionData;

    /// Ensures that the given payload does not violate any consensus rules that concern the block's
    /// layout.
    ///
    /// This function must convert the payload into the executable block and pre-validate its
    /// fields.
    fn ensure_well_formed_payload(
        &self,
        payload: Self::ExecutionData,
    ) -> Result<RecoveredBlock<Self::Block>, NewPayloadError> {
        let TaikoExecutionData {
            execution_payload: payload,
            taiko_sidecar,
        } = payload;

        let expected_hash = payload.block_hash;

        // First parse the block.
        let mut block = payload.try_into_block()?;
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

        sealed_block
            .try_recover()
            .map_err(|e| NewPayloadError::Other(e.into()))
    }
}

impl<Types> EngineValidator<Types> for TaikoEngineValidator
where
    Types: PayloadTypes<PayloadAttributes = TaikoPayloadAttributes, ExecutionData = TaikoExecutionData>,
{
    /// Validates the presence or exclusion of fork-specific fields based on the payload attributes
    /// and the message version.
    fn validate_version_specific_fields(
        &self,
        _: EngineApiMessageVersion,
        _: PayloadOrAttributes<'_, Self::ExecutionData, TaikoPayloadAttributes>,
    ) -> Result<(), EngineObjectValidationError> {
        // No need to validate validate_withdrawals_presence and validate_parent_beacon_block_root_presence
        Ok(())
    }

    /// Ensures that the payload attributes are valid for the given [`EngineApiMessageVersion`].
    fn ensure_well_formed_attributes(
        &self,
        _: EngineApiMessageVersion,
        _: &TaikoPayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        // No need to validate validate_withdrawals_presence and validate_parent_beacon_block_root_presence
        Ok(())
    }

    /// Validates the payload attributes with respect to the header.
    fn validate_payload_attributes_against_header(
        &self,
        attributes: &TaikoPayloadAttributes,
        header: &Header,
    ) -> Result<(), InvalidPayloadAttributesError> {
        // We allow the payload attributes to have a timestamp that is equal to the parent header's timestamp
        // in Taiko network.
        if attributes.payload_attributes.timestamp() < header.timestamp() {
            return Err(InvalidPayloadAttributesError::InvalidTimestamp);
        }
        Ok(())
    }
}
