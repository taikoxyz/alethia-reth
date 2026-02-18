//! Taiko header, block, and anchor transaction validation logic.
use std::{fmt::Debug, sync::Arc};

use alloy_consensus::{
    BlockHeader as AlloyBlockHeader, EMPTY_OMMER_ROOT_HASH, TxReceipt,
    constants::{EMPTY_WITHDRAWALS, MAXIMUM_EXTRA_DATA_SIZE},
    proofs::calculate_receipt_root,
};
use alloy_eips::{Encodable2718, eip7685::Requests};
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
use alloy_sol_types::{SolCall, sol};
use reth_consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator, ReceiptRootBloom};
use reth_consensus_common::validation::{
    MAX_RLP_BLOCK_SIZE, validate_against_parent_hash_number, validate_body_against_header,
    validate_header_base_fee, validate_header_extra_data, validate_header_gas,
};
use reth_evm::block::BlockExecutionResult;
use reth_primitives_traits::{
    Block, BlockBody, BlockHeader, GotExpected, NodePrimitives, Receipt, RecoveredBlock,
    SealedBlock, SealedHeader, SignedTransaction, receipt::gas_spent_by_transactions,
};

use crate::eip4396::{
    MAINNET_MIN_BASE_FEE, MIN_BASE_FEE, SHASTA_INITIAL_BASE_FEE,
    calculate_next_block_eip4396_base_fee,
};
use alethia_reth_chainspec::{TAIKO_MAINNET, hardfork::TaikoHardforks, spec::TaikoChainSpec};
use alethia_reth_evm::alloy::TAIKO_GOLDEN_TOUCH_ADDRESS;

sol! {
    function anchor(bytes32, bytes32, uint64, uint32) external;
    function anchorV2(uint64, bytes32, uint32, (uint8, uint8, uint32, uint64, uint32)) external;
    function anchorV3(uint64, bytes32, uint32, (uint8, uint8, uint32, uint64, uint32), bytes32[]) external;
    function anchorV4((uint48, bytes32, bytes32)) external;
}

/// Anchor / system transaction call selectors.
pub const ANCHOR_V1_SELECTOR: &[u8; 4] = &anchorCall::SELECTOR;
/// Selector for the Ontake-era `anchorV2` transaction.
pub const ANCHOR_V2_SELECTOR: &[u8; 4] = &anchorV2Call::SELECTOR;
/// Selector for the Pacaya-era `anchorV3` transaction.
pub const ANCHOR_V3_SELECTOR: &[u8; 4] = &anchorV3Call::SELECTOR;
/// Selector for the Shasta-era `anchorV4` transaction.
pub const ANCHOR_V4_SELECTOR: &[u8; 4] = &anchorV4Call::SELECTOR;

/// The gas limit for the anchor transactions before Pacaya hardfork.
pub const ANCHOR_V1_V2_GAS_LIMIT: u64 = 250_000;
/// The gas limit for the anchor transactions in Pacaya and Shasta hardfork blocks.
pub const ANCHOR_V3_V4_GAS_LIMIT: u64 = 1_000_000;

/// Minimal block reader interface used by Taiko consensus.
pub trait TaikoBlockReader: Send + Sync + Debug {
    /// Returns the timestamp of the block referenced by the given hash, if present.
    fn block_timestamp_by_hash(&self, hash: B256) -> Option<u64>;
}

#[derive(Debug, Clone, Default)]
/// Noop Taiko block reader implementation that returns `None` for all queries.
pub struct NoopTaikoBlockReader;

impl TaikoBlockReader for NoopTaikoBlockReader {
    fn block_timestamp_by_hash(&self, _hash: B256) -> Option<u64> {
        None
    }
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

    /// Create a new instance of [`TaikoBeaconConsensus`] with a noop block reader.
    pub fn new_with_noop_block_reader(chain_spec: Arc<TaikoChainSpec>) -> Self {
        Self { chain_spec, block_reader: Arc::new(NoopTaikoBlockReader) }
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
        _receipt_root_bloom: Option<ReceiptRootBloom>,
    ) -> Result<(), ConsensusError> {
        validate_block_post_execution(block, &self.chain_spec, &result.receipts, &result.requests)?;
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

        // In Taiko network, withdrawals root is always empty.
        if let Some(withdrawals_root) = block.withdrawals_root() {
            if withdrawals_root != EMPTY_WITHDRAWALS {
                return Err(ConsensusError::BodyWithdrawalsRootDiff(
                    GotExpected { got: withdrawals_root, expected: EMPTY_WITHDRAWALS }.into(),
                ));
            }
        } else {
            return Err(ConsensusError::WithdrawalsRootMissing);
        }

        if self.chain_spec.is_osaka_active_at_timestamp(block.timestamp()) &&
            block.rlp_length() > MAX_RLP_BLOCK_SIZE
        {
            return Err(ConsensusError::BlockTooLarge {
                rlp_length: block.rlp_length(),
                max_rlp_length: MAX_RLP_BLOCK_SIZE,
            })
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
        validate_header_base_fee(header, &self.chain_spec)?;

        // Ensures that Cancun related fields are still not set in Taiko
        if header.blob_gas_used().is_some() {
            return Err(ConsensusError::BlobGasUsedUnexpected);
        } else if header.excess_blob_gas().is_some() {
            return Err(ConsensusError::ExcessBlobGasUnexpected);
        } else if header.parent_beacon_block_root().is_some() {
            return Err(ConsensusError::ParentBeaconBlockRootUnexpected);
        }

        Ok(())
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

/// Context required to validate an anchor transaction.
pub struct AnchorValidationContext {
    /// Timestamp for hardfork selection.
    pub timestamp: u64,
    /// Block number for hardfork selection and gas limit rules.
    pub block_number: u64,
    /// Base fee per gas for tip validation.
    pub base_fee_per_gas: u64,
}

/// Validates a single anchor transaction against the current hardfork rules.
pub fn validate_anchor_transaction(
    anchor_transaction: &impl SignedTransaction,
    chain_spec: &TaikoChainSpec,
    ctx: AnchorValidationContext,
) -> Result<(), ConsensusError> {
    // Ensure the input data starts with one of the anchor selectors.
    if chain_spec.is_shasta_active(ctx.timestamp) {
        validate_input_selector(anchor_transaction.input(), ANCHOR_V4_SELECTOR)?;
    } else if chain_spec.is_pacaya_active_at_block(ctx.block_number) {
        validate_input_selector(anchor_transaction.input(), ANCHOR_V3_SELECTOR)?;
    } else if chain_spec.is_ontake_active_at_block(ctx.block_number) {
        validate_input_selector(anchor_transaction.input(), ANCHOR_V2_SELECTOR)?;
    } else {
        validate_input_selector(anchor_transaction.input(), ANCHOR_V1_SELECTOR)?;
    }

    // Ensure the value is zero.
    if anchor_transaction.value() != U256::ZERO {
        return Err(ConsensusError::Other("Anchor transaction value must be zero".into()));
    }

    // Ensure the gas limit is correct.
    let gas_limit = if chain_spec.is_pacaya_active_at_block(ctx.block_number) {
        ANCHOR_V3_V4_GAS_LIMIT
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
        anchor_transaction.effective_tip_per_gas(ctx.base_fee_per_gas).ok_or_else(|| {
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

    validate_anchor_transaction(
        anchor_transaction,
        chain_spec,
        AnchorValidationContext {
            timestamp: block.header().timestamp(),
            block_number: block.number(),
            base_fee_per_gas: block.header().base_fee_per_gas().ok_or_else(|| {
                ConsensusError::Other("Block base fee per gas must be set".into())
            })?,
        },
    )
}

/// Validate that transaction input starts with the expected call selector.
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

/// Validate a block with regard to execution results:
///
/// - Compares the receipts root in the block header to the block body
/// - Compares the gas used in the block header to the actual gas usage after execution
///
/// Note: Taiko does not use requests hash validation, so that part is omitted.
pub fn validate_block_post_execution<B, R, ChainSpec>(
    block: &RecoveredBlock<B>,
    chain_spec: &ChainSpec,
    receipts: &[R],
    _requests: &Requests,
) -> Result<(), ConsensusError>
where
    B: Block,
    R: Receipt,
    ChainSpec: EthereumHardforks,
{
    // Check if gas used matches the value set in header.
    let cumulative_gas_used =
        receipts.last().map(|receipt| receipt.cumulative_gas_used()).unwrap_or(0);
    if block.header().gas_used() != cumulative_gas_used {
        return Err(ConsensusError::BlockGasUsed {
            gas: GotExpected { got: cumulative_gas_used, expected: block.header().gas_used() },
            gas_spent_by_tx: gas_spent_by_transactions(receipts),
        })
    }

    // Before Byzantium, receipts contained state root that would mean that expensive
    // operation as hashing that is required for state root got calculated in every
    // transaction This was replaced with is_success flag.
    // See more about EIP here: https://eips.ethereum.org/EIPS/eip-658
    if chain_spec.is_byzantium_active_at_block(block.header().number()) &&
        let Err(error) = verify_receipts(
            block.header().receipts_root(),
            block.header().logs_bloom(),
            receipts,
        )
    {
        let receipts = receipts
            .iter()
            .map(|r| Bytes::from(r.with_bloom_ref().encoded_2718()))
            .collect::<Vec<_>>();
        tracing::debug!(%error, ?receipts, "receipts verification failed");
        return Err(error)
    }

    Ok(())
}

/// Calculate the receipts root, and compare it against the expected receipts root and logs
/// bloom.
fn verify_receipts<R: Receipt>(
    expected_receipts_root: B256,
    expected_logs_bloom: Bloom,
    receipts: &[R],
) -> Result<(), ConsensusError> {
    // Calculate receipts root.
    let receipts_with_bloom = receipts.iter().map(TxReceipt::with_bloom_ref).collect::<Vec<_>>();
    let receipts_root = calculate_receipt_root(&receipts_with_bloom);

    // Calculate header logs bloom.
    let logs_bloom = receipts_with_bloom.iter().fold(Bloom::ZERO, |bloom, r| bloom | r.bloom_ref());

    compare_receipts_root_and_logs_bloom(
        receipts_root,
        logs_bloom,
        expected_receipts_root,
        expected_logs_bloom,
    )?;

    Ok(())
}

/// Compare the calculated receipts root with the expected receipts root, also compare
/// the calculated logs bloom with the expected logs bloom.
fn compare_receipts_root_and_logs_bloom(
    calculated_receipts_root: B256,
    calculated_logs_bloom: Bloom,
    expected_receipts_root: B256,
    expected_logs_bloom: Bloom,
) -> Result<(), ConsensusError> {
    if calculated_receipts_root != expected_receipts_root {
        return Err(ConsensusError::BodyReceiptRootDiff(
            GotExpected { got: calculated_receipts_root, expected: expected_receipts_root }.into(),
        ))
    }

    if calculated_logs_bloom != expected_logs_bloom {
        return Err(ConsensusError::BodyBloomLogDiff(
            GotExpected { got: calculated_logs_bloom, expected: expected_logs_bloom }.into(),
        ))
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::validate_input_selector;
    use alethia_reth_chainspec::{TAIKO_DEVNET, TAIKO_MAINNET};
    use alloy_consensus::Header;

    use super::*;

    #[test]
    fn test_validate_against_parent_eip4396_base_fee() {
        let mut header = Header::default();

        assert!(validate_against_parent_eip4396_base_fee(&header).is_err());

        header.base_fee_per_gas = Some(1);
        assert!(validate_against_parent_eip4396_base_fee(&header).is_ok());
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
    fn test_anchor_v4_selector_matches_protocol() {
        assert_eq!(ANCHOR_V4_SELECTOR, &[0x52, 0x3e, 0x68, 0x54]);
    }

    #[test]
    fn test_validate_header_against_parent() {
        use crate::eip4396::{
            BLOCK_TIME_TARGET, MAX_BASE_FEE, MIN_BASE_FEE, calculate_next_block_eip4396_base_fee,
        };

        // Test calculate_next_block_eip4396_base_fee function
        let mut parent = Header {
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            number: 1,
            ..Default::default()
        };

        // Test 1: Gas used equals target (gas_limit / 2)
        parent.gas_used = 15_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(
            &parent,
            BLOCK_TIME_TARGET,
            parent.base_fee_per_gas.expect("parent base fee set"),
            MIN_BASE_FEE,
        );
        assert_eq!(base_fee, 1_000_000_000, "Base fee should remain the same when at target");

        // Test 2: Gas used above target
        parent.gas_used = 20_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(
            &parent,
            BLOCK_TIME_TARGET,
            parent.base_fee_per_gas.expect("parent base fee set"),
            MIN_BASE_FEE,
        );
        assert_eq!(
            base_fee, MAX_BASE_FEE,
            "Base fee should stay clamped at MAX_BASE_FEE when the parent is already at the cap"
        );

        // Test 3: Gas used below target
        parent.gas_used = 10_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(
            &parent,
            BLOCK_TIME_TARGET,
            parent.base_fee_per_gas.expect("parent base fee set"),
            MIN_BASE_FEE,
        );
        assert!(base_fee < 1_000_000_000, "Base fee should decrease when below target");
    }

    #[test]
    fn test_min_base_fee_to_clamp_uses_chain_id() {
        let mut non_mainnet_spec = TAIKO_DEVNET.as_ref().clone();
        non_mainnet_spec.inner.chain = TAIKO_MAINNET.inner.chain;
        assert_eq!(
            min_base_fee_to_clamp(&non_mainnet_spec),
            MAINNET_MIN_BASE_FEE,
            "Mainnet clamp should be selected by chain id"
        );
    }
}
