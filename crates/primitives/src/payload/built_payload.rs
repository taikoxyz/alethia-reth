//! Payload related types

use std::{fmt::Debug, sync::Arc};

use alloy_consensus::Block;
use alloy_eips::eip7685::Requests;
use alloy_primitives::U256;
use alloy_rpc_types_engine::{
    BlobsBundleV1, BlobsBundleV2, ExecutionPayloadEnvelopeV2, ExecutionPayloadEnvelopeV3,
    ExecutionPayloadEnvelopeV4, ExecutionPayloadEnvelopeV5, ExecutionPayloadFieldV2,
    ExecutionPayloadV1, ExecutionPayloadV3, PayloadId,
};
use reth_ethereum_engine_primitives::BuiltPayloadConversionError;
use reth_payload_primitives::{BuiltPayload, BuiltPayloadExecutedBlock};
use reth_primitives_traits::{BlockBody, NodePrimitives, SealedBlock, SignedTransaction};

use crate::TaikoPrimitives;

/// Contains the built payload.
#[derive(Debug, Clone)]
pub struct TaikoBuiltPayload<N: NodePrimitives = TaikoPrimitives> {
    /// Identifier of the payload
    pub(crate) id: PayloadId,
    /// Sealed block
    pub(crate) block: Arc<SealedBlock<N::Block>>,
    /// Block execution data for the payload, if any.
    pub(crate) executed_block: Option<BuiltPayloadExecutedBlock<N>>,
    /// The fees of the block
    pub(crate) fees: U256,
}

// === impl BuiltPayload ===

impl<N: NodePrimitives> TaikoBuiltPayload<N> {
    /// Initializes the payload with the given initial block.
    pub const fn new(
        id: PayloadId,
        block: Arc<SealedBlock<N::Block>>,
        fees: U256,
        executed_block: Option<BuiltPayloadExecutedBlock<N>>,
    ) -> Self {
        Self { id, block, fees, executed_block }
    }

    /// Returns the identifier of the payload.
    pub const fn id(&self) -> PayloadId {
        self.id
    }

    /// Returns the built block(sealed)
    pub fn block(&self) -> &SealedBlock<N::Block> {
        &self.block
    }

    /// Fees of the block
    pub const fn fees(&self) -> U256 {
        self.fees
    }

    /// Converts the value into [`SealedBlock`].
    pub fn into_sealed_block(self) -> SealedBlock<N::Block> {
        Arc::unwrap_or_clone(self.block)
    }

    /// Try converting built payload into [`ExecutionPayloadEnvelopeV3`].
    ///
    /// Returns an error if the payload contains non EIP-4844 sidecar.
    pub fn try_into_v3(self) -> Result<ExecutionPayloadEnvelopeV3, BuiltPayloadConversionError>
    where
        N::Block: Into<Block<<N::BlockBody as BlockBody>::Transaction>>,
    {
        let Self { block, fees, .. } = self;

        Ok(ExecutionPayloadEnvelopeV3 {
            execution_payload: ExecutionPayloadV3::from_block_unchecked(
                block.hash(),
                &Arc::unwrap_or_clone(block).into_block().into(),
            ),
            block_value: fees,
            // From the engine API spec:
            //
            // > Client software **MAY** use any heuristics to decide whether to set
            // `shouldOverrideBuilder` flag or not. If client software does not implement any
            // heuristic this flag **SHOULD** be set to `false`.
            //
            // Spec:
            // <https://github.com/ethereum/execution-apis/blob/fe8e13c288c592ec154ce25c534e26cb7ce0530d/src/engine/cancun.md#specification-2>
            should_override_builder: false,
            blobs_bundle: BlobsBundleV1::empty(),
        })
    }

    /// Try converting built payload into [`ExecutionPayloadEnvelopeV4`].
    ///
    /// Returns an error if the payload contains non EIP-4844 sidecar.
    pub fn try_into_v4(self) -> Result<ExecutionPayloadEnvelopeV4, BuiltPayloadConversionError>
    where
        N::Block: Into<Block<<N::BlockBody as BlockBody>::Transaction>>,
    {
        Ok(ExecutionPayloadEnvelopeV4 {
            execution_requests: Requests::default(),
            envelope_inner: self.try_into_v3()?,
        })
    }

    /// Try converting built payload into [`ExecutionPayloadEnvelopeV5`].
    pub fn try_into_v5(self) -> Result<ExecutionPayloadEnvelopeV5, BuiltPayloadConversionError>
    where
        N::Block: Into<Block<<N::BlockBody as BlockBody>::Transaction>>,
    {
        let Self { block, fees, .. } = self;

        Ok(ExecutionPayloadEnvelopeV5 {
            execution_payload: ExecutionPayloadV3::from_block_unchecked(
                block.hash(),
                &Arc::unwrap_or_clone(block).into_block().into(),
            ),
            block_value: fees,
            // From the engine API spec:
            //
            // > Client software **MAY** use any heuristics to decide whether to set
            // `shouldOverrideBuilder` flag or not. If client software does not implement any
            // heuristic this flag **SHOULD** be set to `false`.
            //
            // Spec:
            // <https://github.com/ethereum/execution-apis/blob/fe8e13c288c592ec154ce25c534e26cb7ce0530d/src/engine/cancun.md#specification-2>
            should_override_builder: false,
            blobs_bundle: BlobsBundleV2::empty(),
            execution_requests: Requests::default(),
        })
    }
}

impl TryFrom<TaikoBuiltPayload> for ExecutionPayloadEnvelopeV3 {
    type Error = BuiltPayloadConversionError;

    fn try_from(value: TaikoBuiltPayload) -> Result<Self, Self::Error> {
        value.try_into_v3()
    }
}

impl TryFrom<TaikoBuiltPayload> for ExecutionPayloadEnvelopeV4 {
    type Error = BuiltPayloadConversionError;

    fn try_from(value: TaikoBuiltPayload) -> Result<Self, Self::Error> {
        value.try_into_v4()
    }
}

impl TryFrom<TaikoBuiltPayload> for ExecutionPayloadEnvelopeV5 {
    type Error = BuiltPayloadConversionError;

    fn try_from(value: TaikoBuiltPayload) -> Result<Self, Self::Error> {
        value.try_into_v5()
    }
}

impl<N: NodePrimitives> BuiltPayload for TaikoBuiltPayload<N> {
    type Primitives = N;

    fn block(&self) -> &SealedBlock<N::Block> {
        self.block()
    }

    fn fees(&self) -> U256 {
        self.fees
    }

    fn executed_block(&self) -> Option<BuiltPayloadExecutedBlock<N>> {
        self.executed_block.clone()
    }

    fn requests(&self) -> Option<Requests> {
        None
    }
}

// V1 engine_getPayloadV1 response
impl<T, N> From<TaikoBuiltPayload<N>> for ExecutionPayloadV1
where
    T: SignedTransaction,
    N: NodePrimitives<Block = Block<T>>,
{
    fn from(value: TaikoBuiltPayload<N>) -> Self {
        Self::from_block_unchecked(
            value.block().hash(),
            &Arc::unwrap_or_clone(value.block).into_block(),
        )
    }
}

// V2 engine_getPayloadV2 response
impl<T, N> From<TaikoBuiltPayload<N>> for ExecutionPayloadEnvelopeV2
where
    T: SignedTransaction,
    N: NodePrimitives<Block = Block<T>>,
{
    fn from(value: TaikoBuiltPayload<N>) -> Self {
        let TaikoBuiltPayload { block, fees, .. } = value;

        Self {
            block_value: fees,
            execution_payload: ExecutionPayloadFieldV2::from_block_unchecked(
                block.hash(),
                &Arc::unwrap_or_clone(block).into_block(),
            ),
        }
    }
}
