use std::{fmt::Debug, sync::Arc};

use alloy_consensus::{BlockHeader as AlloyBlockHeader, EMPTY_OMMER_ROOT_HASH, Transaction};
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{Address, U256};
use alloy_sol_types::{SolCall, sol};
use reth::{
    beacon_consensus::validate_block_post_execution,
    chainspec::EthChainSpec,
    consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator},
    consensus_common::validation::{
        validate_against_parent_hash_number, validate_body_against_header,
        validate_header_base_fee, validate_header_extra_data, validate_header_gas,
    },
    primitives::SealedBlock,
};
use reth_node_api::NodePrimitives;
use reth_primitives_traits::{
    Block, BlockBody, BlockHeader, GotExpected, RecoveredBlock, SealedHeader, SignedTransaction,
};
use reth_provider::{BlockExecutionResult, BlockReader};

use crate::{
    chainspec::{
        hardfork::{TaikoHardfork, TaikoHardforks},
        spec::TaikoChainSpec,
    },
    consensus::eip4396::{SHASTA_INITIAL_BASE_FEE, calculate_next_block_eip4396_base_fee},
    evm::alloy::TAIKO_GOLDEN_TOUCH_ADDRESS,
};

sol! {
    function anchor(bytes32, bytes32, uint64, uint32) external;
    function anchorV2(uint64, bytes32, uint32, (uint8, uint8, uint32, uint64, uint32)) external;
    function anchorV3(uint64, bytes32, uint32, (uint8, uint8, uint32, uint64, uint32), bytes32[]) external;
    function updateState(uint48, address, bytes, bytes32, (uint48, uint8, address, address)[], uint16, uint48, bytes32, bytes32) external;
}

/// Anchor / system transaction call selectors.
pub const ANCHOR_V1_SELECTOR: &[u8; 4] = &anchorCall::SELECTOR;
pub const ANCHOR_V2_SELECTOR: &[u8; 4] = &anchorV2Call::SELECTOR;
pub const ANCHOR_V3_SELECTOR: &[u8; 4] = &anchorV3Call::SELECTOR;
pub const UPDATE_STATE_SHASTA_SELECTOR: &[u8; 4] = &updateStateCall::SELECTOR;

/// The gas limit for the anchor transactions before Pacaya hardfork.
pub const ANCHOR_V1_V2_GAS_LIMIT: u64 = 250_000;
/// The gas limit for the anchor transactions in Pacaya hardfork blocks.
pub const ANCHOR_V3_GAS_LIMIT: u64 = 1_000_000;

/// Taiko consensus implementation.
///
/// Provides basic checks as outlined in the execution specs.
#[derive(Debug, Clone)]
pub struct TaikoBeaconConsensus<R: BlockReader> {
    chain_spec: Arc<TaikoChainSpec>,
    block_reader: R,
}

impl<R: BlockReader> TaikoBeaconConsensus<R> {
    /// Create a new instance of [`TaikoBeaconConsensus`]
    pub fn new(chain_spec: Arc<TaikoChainSpec>, block_reader: R) -> Self {
        Self { chain_spec, block_reader }
    }
}

impl<N, R> FullConsensus<N> for TaikoBeaconConsensus<R>
where
    N: NodePrimitives,
    R: BlockReader + Debug,
{
    /// Validate a block with regard to execution results:
    ///
    /// - Compares the receipts root in the block header to the block body
    /// - Compares the gas used in the block header to the actual gas usage after execution
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<N::Block>,
        result: &BlockExecutionResult<N::Receipt>,
    ) -> Result<(), ConsensusError> {
        validate_block_post_execution(block, &self.chain_spec, &result.receipts, &result.requests)?;
        validate_anchor_transaction_in_block::<<N as NodePrimitives>::Block>(
            block,
            &self.chain_spec,
        )
    }
}

impl<B: Block, R> Consensus<B> for TaikoBeaconConsensus<R>
where
    R: BlockReader + Debug,
{
    /// The error type related to consensus.
    type Error = ConsensusError;

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
                GotExpected { got: block.ommers_hash(), expected: block.ommers_hash() }.into(),
            ));
        }

        Ok(())
    }
}

impl<H, R> HeaderValidator<H> for TaikoBeaconConsensus<R>
where
    H: BlockHeader,
    R: BlockReader + Debug,
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

        validate_header_extra_data(header)?;
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

        if self.chain_spec.is_shasta_active_at_block(header.number()) {
            // Shasta hardfork introduces stricter timestamp validation:
            // timestamps must strictly increase (no equal timestamps allowed)
            if header.timestamp() <= parent.timestamp() {
                return Err(ConsensusError::TimestampIsInPast {
                    parent_timestamp: parent.timestamp(),
                    timestamp: header.timestamp(),
                });
            }

            // Calculate the expected base fee for this block.
            let mut expected_base_fee = SHASTA_INITIAL_BASE_FEE;

            // Get the Shasta fork activation block number.
            let shasta_fork_block = self
                .chain_spec
                .taiko_fork_activation(TaikoHardfork::Shasta)
                .block_number()
                .ok_or(ConsensusError::Other("Shasta fork is not activated".to_string()))?;

            // For blocks after Shasta+1, use EIP-4396 dynamic base fee calculation
            // The first 2 blocks after Shasta use the initial base fee.
            if header.number() > shasta_fork_block + 1 {
                // Get the grandparent block to calculate parent block time.
                let parent_block_parent_hash = parent.header().parent_hash();

                // Calculate parent block time = parent.timestamp - grandparent.timestamp
                let parent_block_time = parent.header().timestamp()
                    - self
                        .block_reader
                        .block_by_hash(parent_block_parent_hash)
                        .map_err(|_| ConsensusError::ParentUnknown {
                            hash: parent_block_parent_hash,
                        })?
                        .ok_or(ConsensusError::ParentUnknown { hash: parent_block_parent_hash })?
                        .header()
                        .timestamp();

                // Calculate the expected base fee using EIP-4396 formula.
                expected_base_fee =
                    calculate_next_block_eip4396_base_fee(parent.header(), parent_block_time);
            }

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

/// Validates the base fee against the parent.
#[inline]
pub fn validate_against_parent_eip4396_base_fee<
    ChainSpec: EthChainSpec + EthereumHardforks + TaikoHardforks,
    H: BlockHeader,
>(
    header: &H,
    _parent: &H,
    _chain_spec: &ChainSpec,
) -> Result<(), ConsensusError> {
    if header.base_fee_per_gas().is_none() {
        return Err(ConsensusError::BaseFeeMissing);
    }

    Ok(())
}

/// Validates the anchor transaction in the block.
pub fn validate_anchor_transaction_in_block<B>(
    block: &RecoveredBlock<B>,
    chain_spec: &TaikoChainSpec,
) -> Result<(), ConsensusError>
where
    B: Block,
{
    let anchor_transaction = match block.body().transactions().first() {
        Some(tx) => tx,
        None => return Ok(()),
    };

    // Ensure the input data starts with one of the anchor selectors.
    if chain_spec.is_shasta_active_at_block(block.number()) {
        validate_input_selector(anchor_transaction.input(), UPDATE_STATE_SHASTA_SELECTOR)?;
    } else if chain_spec.is_pacaya_active_at_block(block.number()) {
        validate_input_selector(anchor_transaction.input(), ANCHOR_V3_SELECTOR)?;
    } else if chain_spec.is_ontake_active_at_block(block.number()) {
        validate_input_selector(anchor_transaction.input(), ANCHOR_V2_SELECTOR)?;
    } else {
        validate_input_selector(anchor_transaction.input(), ANCHOR_V1_SELECTOR)?;
    }

    // Ensure the value is zero.
    if anchor_transaction.value() != U256::ZERO {
        return Err(ConsensusError::Other("Anchor transaction value must be zero".into()));
    }

    // Ensure the gas limit is correct.
    let gas_limit = if chain_spec.is_pacaya_active_at_block(block.number()) {
        ANCHOR_V3_GAS_LIMIT
    } else {
        ANCHOR_V1_V2_GAS_LIMIT
    };
    if anchor_transaction.gas_limit() != gas_limit {
        return Err(ConsensusError::Other(format!(
            "Anchor transaction gas limit must be {gas_limit}, got {}",
            anchor_transaction.gas_limit()
        )));
    }

    // Ensure the tip is equal to zero.
    let anchor_transaction_tip =
        anchor_transaction
            .effective_tip_per_gas(block.header().base_fee_per_gas().ok_or_else(|| {
                ConsensusError::Other("Block base fee per gas must be set".into())
            })?)
            .ok_or_else(|| {
                ConsensusError::Other("Anchor transaction tip must be set to zero".into())
            })?;

    if anchor_transaction_tip != 0 {
        return Err(ConsensusError::Other(format!(
            "Anchor transaction tip must be zero, got {anchor_transaction_tip}"
        )));
    }

    // Ensure the sender is the treasury address.
    let sender = anchor_transaction.try_recover().map_err(|err| {
        ConsensusError::Other(format!("Anchor transaction sender must be recoverable: {err}"))
    })?;
    if sender != Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS) {
        return Err(ConsensusError::Other(format!(
            "Anchor transaction sender must be the treasury address, got {sender}"
        )));
    }

    Ok(())
}

// Validates the transaction input data against the expected selector.
fn validate_input_selector(
    input: &[u8],
    expected_selector: &[u8; 4],
) -> Result<(), ConsensusError> {
    if !input.starts_with(expected_selector) {
        return Err(ConsensusError::Other(format!(
            "Anchor transaction input data does not match the expected selector: {expected_selector:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use alloy_consensus::Header;
    use alloy_primitives::U64;

    use super::validate_input_selector;

    use super::*;

    #[test]
    fn test_validate_against_parent_eip4396_base_fee() {
        let parent_header = &Header::default();
        let mut header = parent_header.clone();
        header.parent_hash = parent_header.hash_slow();
        header.number = parent_header.number + 1;

        assert!(
            validate_against_parent_eip4396_base_fee(
                &header,
                parent_header,
                &Arc::new(TaikoChainSpec::default())
            )
            .is_err()
        );

        header.base_fee_per_gas = Some(U64::random().to::<u64>());
        assert!(
            validate_against_parent_eip4396_base_fee(
                &header,
                parent_header,
                &Arc::new(TaikoChainSpec::default())
            )
            .is_ok()
        );
    }

    #[test]
    fn test_validate_input_selector() {
        // Valid selector
        let input = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc];
        let expected_selector = [0x12, 0x34, 0x56, 0x78];
        assert!(validate_input_selector(&input, &expected_selector).is_ok());

        // Invalid selector
        let wrong_selector = [0x11, 0x22, 0x33, 0x44];
        assert!(validate_input_selector(&input, &wrong_selector).is_err());

        // Empty input
        let empty_input = [];
        assert!(validate_input_selector(&empty_input, &expected_selector).is_err());
    }

    #[test]
    fn test_validate_header_against_parent() {
        use crate::consensus::eip4396::{BLOCK_TIME_TARGET, calculate_next_block_eip4396_base_fee};

        // Test calculate_next_block_eip4396_base_fee function
        let mut parent = Header::default();
        parent.gas_limit = 30_000_000;
        parent.base_fee_per_gas = Some(1_000_000_000);

        // Test 1: Gas used equals target (gas_limit / 2)
        parent.gas_used = 15_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(&parent, BLOCK_TIME_TARGET);
        assert_eq!(base_fee, 1_000_000_000, "Base fee should remain the same when at target");

        // Test 2: Gas used above target
        parent.gas_used = 20_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(&parent, BLOCK_TIME_TARGET);
        assert!(base_fee > 1_000_000_000, "Base fee should increase when above target");

        // Test 3: Gas used below target
        parent.gas_used = 10_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(&parent, BLOCK_TIME_TARGET);
        assert!(base_fee < 1_000_000_000, "Base fee should decrease when below target");
    }
}
