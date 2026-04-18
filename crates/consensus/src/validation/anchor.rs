//! Anchor transaction selectors and validation helpers.

use alloy_consensus::BlockHeader as AlloyBlockHeader;
use alloy_primitives::{Address, U256};
use reth_consensus::ConsensusError;
use reth_primitives_traits::{Block, BlockBody, RecoveredBlock, SignedTransaction};

use alethia_reth_chainspec::{hardfork::TaikoHardforks, spec::TaikoChainSpec};
use alethia_reth_evm::alloy::TAIKO_GOLDEN_TOUCH_ADDRESS;

pub use crate::anchor_constants::{
    ANCHOR_V1_SELECTOR, ANCHOR_V1_V2_GAS_LIMIT, ANCHOR_V2_SELECTOR, ANCHOR_V3_SELECTOR,
    ANCHOR_V3_V4_GAS_LIMIT, ANCHOR_V4_SELECTOR,
};

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
        None if block.number() == 0 => return Ok(()),
        None => {
            return Err(ConsensusError::Other(
                "Block must contain at least the anchor transaction".into(),
            ));
        }
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

/// Validates that transaction input starts with the expected call selector.
pub(super) fn validate_input_selector(
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
