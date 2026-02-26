//! Taiko header, block, and anchor transaction validation logic.
use std::{fmt::Debug, sync::Arc};

use alloy_consensus::{
    BlockHeader as AlloyBlockHeader, EMPTY_OMMER_ROOT_HASH, constants::MAXIMUM_EXTRA_DATA_SIZE,
};
use alloy_primitives::B256;
use reth_consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator, ReceiptRootBloom};
use reth_consensus_common::validation::{
    validate_against_parent_hash_number, validate_body_against_header, validate_header_base_fee,
    validate_header_extra_data, validate_header_gas,
};
use reth_ethereum_consensus::validate_block_post_execution;
use reth_execution_types::BlockExecutionResult;
use reth_primitives::SealedBlock;
use reth_primitives_traits::{
    Block, BlockHeader, GotExpected, NodePrimitives, RecoveredBlock, SealedHeader,
};

use crate::eip4396::{
    MAINNET_MIN_BASE_FEE, MIN_BASE_FEE, SHASTA_INITIAL_BASE_FEE,
    calculate_next_block_eip4396_base_fee,
};
use alethia_reth_chainspec::{TAIKO_MAINNET, hardfork::TaikoHardforks, spec::TaikoChainSpec};

/// Anchor transaction selectors, gas rules, and validation functions.
mod anchor;

pub use anchor::{
    ANCHOR_V1_SELECTOR, ANCHOR_V1_V2_GAS_LIMIT, ANCHOR_V2_SELECTOR, ANCHOR_V3_SELECTOR,
    ANCHOR_V3_V4_GAS_LIMIT, ANCHOR_V4_SELECTOR, AnchorValidationContext,
    validate_anchor_transaction, validate_anchor_transaction_in_block,
};

#[cfg(test)]
mod tests;

/// Minimal block reader interface used by Taiko consensus.
pub trait TaikoBlockReader: Send + Sync + Debug {
    /// Returns the timestamp of the block referenced by the given hash, if present.
    fn block_timestamp_by_hash(&self, hash: B256) -> Option<u64>;
}

/// Taiko consensus implementation.
///
/// Provides basic checks as outlined in the execution specs.
#[derive(Debug, Clone)]
pub struct TaikoBeaconConsensus {
    /// Chain spec used for hardfork and chain-id dependent rules.
    chain_spec: Arc<TaikoChainSpec>,
    /// Block reader used to resolve grandparent timestamps.
    block_reader: Arc<dyn TaikoBlockReader>,
}

impl TaikoBeaconConsensus {
    /// Create a new instance of [`TaikoBeaconConsensus`]
    pub fn new(chain_spec: Arc<TaikoChainSpec>, block_reader: Arc<dyn TaikoBlockReader>) -> Self {
        Self { chain_spec, block_reader }
    }
}

impl<N> FullConsensus<N> for TaikoBeaconConsensus
where
    N: NodePrimitives,
{
    /// Validate a block with regard to execution results:
    ///
    /// - Compares the receipts root in the block header to the block body
    /// - Compares the gas used in the block header to the actual gas usage after execution
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<N::Block>,
        result: &BlockExecutionResult<N::Receipt>,
        receipt_root_bloom: Option<ReceiptRootBloom>,
    ) -> Result<(), ConsensusError> {
        validate_block_post_execution(
            block,
            &self.chain_spec,
            &result.receipts,
            &result.requests,
            receipt_root_bloom,
        )?;
        validate_anchor_transaction_in_block::<<N as NodePrimitives>::Block>(
            block,
            &self.chain_spec,
        )
    }
}

impl<B: Block> Consensus<B> for TaikoBeaconConsensus {
    /// Ensures the block response data matches the header.
    ///
    /// This ensures the body response items match the header's hashes:
    ///   - ommer hash
    ///   - transaction root
    ///   - withdrawals root
    fn validate_body_against_header(
        &self,
        body: &B::Body,
        header: &SealedHeader<B::Header>,
    ) -> Result<(), ConsensusError> {
        validate_body_against_header(body, header.header())
    }

    /// Validate a block without regard for state:
    ///
    /// - Compares the ommer hash in the block header to the block body
    /// - Compares the transactions root in the block header to the block body
    fn validate_block_pre_execution(&self, block: &SealedBlock<B>) -> Result<(), ConsensusError> {
        // In Taiko network, ommer hash is always empty.
        if block.ommers_hash() != EMPTY_OMMER_ROOT_HASH {
            return Err(ConsensusError::BodyOmmersHashDiff(
                GotExpected { got: block.ommers_hash(), expected: EMPTY_OMMER_ROOT_HASH }.into(),
            ));
        }

        Ok(())
    }
}

impl<H> HeaderValidator<H> for TaikoBeaconConsensus
where
    H: BlockHeader,
{
    /// Validate if header is correct and follows consensus specification.
    ///
    /// This is called on standalone header to check if all hashes are correct.
    fn validate_header(&self, header: &SealedHeader<H>) -> Result<(), ConsensusError> {
        let header = header.header();

        if !header.difficulty().is_zero() {
            return Err(ConsensusError::TheMergeDifficultyIsNotZero);
        }

        if !header.nonce().is_some_and(|nonce| nonce.is_zero()) {
            return Err(ConsensusError::TheMergeNonceIsNotZero);
        }

        if header.ommers_hash() != EMPTY_OMMER_ROOT_HASH {
            return Err(ConsensusError::TheMergeOmmerRootIsNotEmpty);
        }

        validate_header_extra_data(header, MAXIMUM_EXTRA_DATA_SIZE)?;
        validate_header_gas(header)?;
        validate_header_base_fee(header, &self.chain_spec)
    }

    /// Validate that the header information regarding parent are correct.
    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<H>,
        parent: &SealedHeader<H>,
    ) -> Result<(), ConsensusError> {
        validate_against_parent_hash_number(header.header(), parent)?;

        let header_base_fee =
            { header.header().base_fee_per_gas().ok_or(ConsensusError::BaseFeeMissing)? };

        if self.chain_spec.is_shasta_active(header.timestamp()) {
            // Shasta hardfork introduces stricter timestamp validation:
            // timestamps must strictly increase (no equal timestamps allowed)
            if header.timestamp() <= parent.timestamp() {
                return Err(ConsensusError::TimestampIsInPast {
                    parent_timestamp: parent.timestamp(),
                    timestamp: header.timestamp(),
                });
            }
            let min_base_fee_to_clamp = min_base_fee_to_clamp(self.chain_spec.as_ref());

            // Calculate the expected base fee using EIP-4396 rules. For the first post-genesis
            // block there is no grandparent timestamp, so reuse the default Shasta base fee.
            let expected_base_fee = if parent.number() == 0 {
                SHASTA_INITIAL_BASE_FEE
            } else {
                let parent_base_fee =
                    parent.header().base_fee_per_gas().ok_or(ConsensusError::BaseFeeMissing)?;
                parent_block_time(self.block_reader.as_ref(), parent)
                    .map(|block_time| {
                        calculate_next_block_eip4396_base_fee(
                            parent.header(),
                            block_time,
                            parent_base_fee,
                            min_base_fee_to_clamp,
                        )
                    })
                    // If we cannot retrieve the grandparent timestamp (e.g. when running without a
                    // fully wired block reader), fall back to the header's base fee to avoid
                    // rejecting the block outright.
                    .unwrap_or(header_base_fee)
            };

            // Verify the block's base fee matches the expected value.
            if header_base_fee != expected_base_fee {
                return Err(ConsensusError::BaseFeeDiff(GotExpected {
                    got: header_base_fee,
                    expected: expected_base_fee,
                }));
            }
        } else {
            // For blocks before Shasta, the timestamp must be greater than or equal to the parent's
            // timestamp.
            if header.timestamp() < parent.timestamp() {
                return Err(ConsensusError::TimestampIsInPast {
                    parent_timestamp: parent.timestamp(),
                    timestamp: header.timestamp(),
                });
            }
        }

        Ok(())
    }
}

/// Validates that the header has a base fee set (required after EIP-4396).
#[inline]
pub fn validate_against_parent_eip4396_base_fee<H: BlockHeader>(
    header: &H,
) -> Result<(), ConsensusError> {
    if header.base_fee_per_gas().is_none() {
        return Err(ConsensusError::BaseFeeMissing);
    }

    Ok(())
}

/// Calculates the time difference between the parent and grandparent blocks.
fn parent_block_time<H>(
    block_reader: &dyn TaikoBlockReader,
    parent: &SealedHeader<H>,
) -> Result<u64, ConsensusError>
where
    H: BlockHeader,
{
    let grandparent_hash = parent.header().parent_hash();
    let grandparent_timestamp = block_reader
        .block_timestamp_by_hash(grandparent_hash)
        .ok_or(ConsensusError::ParentUnknown { hash: grandparent_hash })?;

    Ok(parent.header().timestamp() - grandparent_timestamp)
}

#[inline]
/// Returns the minimum base fee to clamp to based on the chain ID.
fn min_base_fee_to_clamp(chain_spec: &TaikoChainSpec) -> u64 {
    if chain_spec.inner.chain.id() == TAIKO_MAINNET.inner.chain.id() {
        MAINNET_MIN_BASE_FEE
    } else {
        MIN_BASE_FEE
    }
}
