use std::sync::Arc;

use alloy_consensus::{BlockHeader as AlloyBlockHeader, EMPTY_OMMER_ROOT_HASH, Transaction};
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{Address, U256};
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
use reth_provider::BlockExecutionResult;

use crate::{
    chainspec::{hardfork::TaikoHardforks, spec::TaikoChainSpec},
    evm::alloy::TAIKO_GOLDEN_TOUCH_ADDRESS,
};

use alloy_sol_types::{SolCall, sol};

sol! {
    function anchor(bytes32, bytes32, uint64, uint32) external;
    function anchorV2(uint64, bytes32, uint32, (uint8, uint8, uint32, uint64, uint32)) external;
    function anchorV3(uint64, bytes32, uint32, (uint8, uint8, uint32, uint64, uint32), bytes32[]) external;
}

pub const ANCHOR_V1_SELECTOR: &[u8; 4] = &anchorCall::SELECTOR;
pub const ANCHOR_V2_SELECTOR: &[u8; 4] = &anchorV2Call::SELECTOR;
pub const ANCHOR_V3_SELECTOR: &[u8; 4] = &anchorV3Call::SELECTOR;

/// The gas limit for the anchor transactions before Pacaya hardfork.
pub const ANCHOR_V1_V2_GAS_LIMIT: u64 = 250_000;
/// The gas limit for the anchor transactions in Pacaya hardfork blocks.
pub const ANCHOR_V3_GAS_LIMIT: u64 = 1_000_000;

/// Taiko consensus implementation.
///
/// Provides basic checks as outlined in the execution specs.
#[derive(Debug, Clone)]
pub struct TaikoBeaconConsensus {
    chain_spec: Arc<TaikoChainSpec>,
}

impl TaikoBeaconConsensus {
    /// Create a new instance of [`TaikoBeaconConsensus`]
    pub fn new(chain_spec: Arc<TaikoChainSpec>) -> Self {
        Self { chain_spec }
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
    ) -> Result<(), ConsensusError> {
        validate_block_post_execution(block, &self.chain_spec, &result.receipts, &result.requests)?;
        validate_anchor_transaction_in_block::<<N as NodePrimitives>::Block>(
            block,
            &self.chain_spec,
        )
    }
}

impl<B: Block> Consensus<B> for TaikoBeaconConsensus {
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

        validate_header_extra_data(header)?;
        validate_header_gas(header)?;
        validate_header_base_fee(header, &self.chain_spec)
    }

    /// Validate that the header information regarding parent are correct.
    ///
    /// In Taiko network, we only need to validate block number, and timestamp,
    /// and we allow a block's timestamp to be equal to its parent's timestamp. Basefee, and
    /// gas limit checks are not needed.
    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<H>,
        parent: &SealedHeader<H>,
    ) -> Result<(), ConsensusError> {
        validate_against_parent_hash_number(header.header(), parent)?;

        if header.timestamp() < parent.timestamp() {
            return Err(ConsensusError::TimestampIsInPast {
                parent_timestamp: parent.timestamp(),
                timestamp: header.timestamp(),
            });
        }

        validate_against_parent_eip4936_base_fee(
            header.header(),
            parent.header(),
            &self.chain_spec,
        )?;

        Ok(())
    }
}

/// Validates the base fee against the parent.
#[inline]
pub fn validate_against_parent_eip4936_base_fee<
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
    if ![ANCHOR_V1_SELECTOR, ANCHOR_V2_SELECTOR, ANCHOR_V3_SELECTOR]
        .iter()
        .any(|&selector| anchor_transaction.input().starts_with(selector))
    {
        return Err(ConsensusError::Other(
            "First transaction does not have a valid anchor selector".into(),
        ));
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

#[cfg(test)]
mod test {
    use alloy_consensus::{Header, constants::MAXIMUM_EXTRA_DATA_SIZE};
    use alloy_primitives::{B64, B256, Bytes, U64, U256};
    use reth_cli::chainspec::ChainSpecParser;

    use crate::chainspec::parser::TaikoChainSpecParser;

    use super::*;

    #[test]
    fn test_validate_against_parent_eip4936_base_fee() {
        let parent_header = &Header::default();
        let mut header = parent_header.clone();
        header.parent_hash = parent_header.hash_slow();
        header.number = parent_header.number + 1;

        assert!(
            validate_against_parent_eip4936_base_fee(
                &header,
                parent_header,
                &Arc::new(TaikoChainSpec::default())
            )
            .is_err()
        );

        header.base_fee_per_gas = Some(U64::random().to::<u64>());
        assert!(
            validate_against_parent_eip4936_base_fee(
                &header,
                parent_header,
                &Arc::new(TaikoChainSpec::default())
            )
            .is_ok()
        );
    }

    #[test]
    fn test_validate_header() {
        let consensus = TaikoBeaconConsensus::new(TaikoChainSpecParser::parse("mainnet").unwrap());

        let mut header = Header::default();
        header.difficulty = U256::random().saturating_add(U256::from(1));
        assert_eq!(
            consensus.validate_header(&SealedHeader::new(header.clone(), header.hash_slow())),
            Err(ConsensusError::TheMergeDifficultyIsNotZero)
        );
        header.difficulty = U256::ZERO;

        header.nonce = B64::random();
        assert_eq!(
            consensus.validate_header(&SealedHeader::new(header.clone(), header.hash_slow())),
            Err(ConsensusError::TheMergeNonceIsNotZero)
        );
        header.nonce = B64::ZERO;

        header.ommers_hash = B256::random();
        assert_eq!(
            consensus.validate_header(&SealedHeader::new(header.clone(), header.hash_slow())),
            Err(ConsensusError::TheMergeOmmerRootIsNotEmpty)
        );
        header.ommers_hash = EMPTY_OMMER_ROOT_HASH;

        header.extra_data = Bytes::from(vec![0; MAXIMUM_EXTRA_DATA_SIZE + 1]);
        assert_eq!(
            consensus.validate_header(&SealedHeader::new(header.clone(), header.hash_slow())),
            Err(ConsensusError::ExtraDataExceedsMax { len: MAXIMUM_EXTRA_DATA_SIZE + 1 })
        );
        header.extra_data = Bytes::from(vec![0; MAXIMUM_EXTRA_DATA_SIZE]);

        header.gas_used = header.gas_limit + 1;
        assert_eq!(
            consensus.validate_header(&SealedHeader::new(header.clone(), header.hash_slow())),
            Err(ConsensusError::HeaderGasUsedExceedsGasLimit {
                gas_used: header.gas_used,
                gas_limit: header.gas_limit,
            })
        );
        header.gas_used = header.gas_limit;

        header.number = 1;
        header.base_fee_per_gas = None;
        assert_eq!(
            consensus.validate_header(&SealedHeader::new(header.clone(), header.hash_slow())),
            Err(ConsensusError::BaseFeeMissing)
        );
    }

    #[test]
    fn test_validate_header_against_parent() {
        let consensus = TaikoBeaconConsensus::new(TaikoChainSpecParser::parse("mainnet").unwrap());

        let mut parent = Header::default();
        let mut header = parent.clone();
        header.number = parent.number + 1;
        header.parent_hash = B256::random();
        assert_eq!(
            consensus.validate_header_against_parent(
                &SealedHeader::new(header.clone(), header.hash_slow()),
                &SealedHeader::new(parent.clone(), parent.hash_slow())
            ),
            Err(ConsensusError::ParentHashMismatch(
                GotExpected { got: header.parent_hash, expected: parent.hash_slow() }.into()
            ))
        );

        parent.timestamp = U64::random().to::<u64>();
        header.parent_hash = parent.hash_slow();
        header.timestamp = parent.timestamp;
        header.base_fee_per_gas = Some(U64::random().to::<u64>());
        assert!(
            consensus
                .validate_header_against_parent(
                    &SealedHeader::new(header.clone(), header.hash_slow()),
                    &SealedHeader::new(parent.clone(), parent.hash_slow()),
                )
                .is_ok()
        );

        header.timestamp = parent.timestamp - 1;
        assert_eq!(
            consensus.validate_header_against_parent(
                &SealedHeader::new(header.clone(), header.hash_slow()),
                &SealedHeader::new(parent.clone(), parent.hash_slow()),
            ),
            Err(ConsensusError::TimestampIsInPast {
                parent_timestamp: parent.timestamp,
                timestamp: header.timestamp,
            })
        );

        header.timestamp = parent.timestamp + 1;
        header.base_fee_per_gas = None;
        assert_eq!(
            consensus.validate_header_against_parent(
                &SealedHeader::new(header.clone(), header.hash_slow()),
                &SealedHeader::new(parent.clone(), parent.hash_slow()),
            ),
            Err(ConsensusError::BaseFeeMissing)
        );
    }
}
