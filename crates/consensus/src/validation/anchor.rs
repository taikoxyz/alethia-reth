//! Anchor transaction selectors and validation helpers.

use alloy_consensus::BlockHeader as AlloyBlockHeader;
use alloy_primitives::{Address, U256};
use reth_consensus::ConsensusError;
use reth_primitives_traits::{Block, BlockBody, RecoveredBlock, SignedTransaction};

use alethia_reth_chainspec::{hardfork::TaikoHardforks, spec::TaikoChainSpec};
use alethia_reth_primitives::addresses::{TAIKO_GOLDEN_TOUCH_ADDRESS, get_treasury_address};

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
    /// Block base fee per gas used to validate the anchor fees.
    pub base_fee_per_gas: u64,
}

/// Validates a single anchor transaction against the current hardfork rules.
pub fn validate_anchor_transaction(
    anchor_transaction: &impl SignedTransaction,
    chain_spec: &TaikoChainSpec,
    ctx: AnchorValidationContext,
) -> Result<(), ConsensusError> {
    let is_shasta = chain_spec.is_shasta_active(ctx.timestamp);
    let is_pacaya = chain_spec.is_pacaya_active_at_block(ctx.block_number);
    if is_shasta {
        if !anchor_transaction.is_eip1559() {
            return Err(ConsensusError::msg("Anchor transaction must use an EIP-1559 envelope"));
        }
        let anchor_address = get_treasury_address(chain_spec.inner.chain.id());
        if anchor_transaction.to() != Some(anchor_address) {
            return Err(ConsensusError::msg(format!(
                "Anchor transaction must call {anchor_address}"
            )));
        }
        if anchor_transaction.max_fee_per_gas() != u128::from(ctx.base_fee_per_gas) {
            return Err(ConsensusError::msg(format!(
                "Anchor transaction max_fee_per_gas mismatch: expected {}, got {}",
                ctx.base_fee_per_gas,
                anchor_transaction.max_fee_per_gas()
            )));
        }
        if anchor_transaction.max_priority_fee_per_gas() != Some(0) {
            return Err(ConsensusError::msg(
                "Anchor transaction max_priority_fee_per_gas mismatch: expected zero",
            ));
        }
        validate_input_selector(anchor_transaction.input(), ANCHOR_V4_SELECTOR)?;
    } else if is_pacaya {
        validate_input_selector(anchor_transaction.input(), ANCHOR_V3_SELECTOR)?;
    } else if chain_spec.is_ontake_active_at_block(ctx.block_number) {
        validate_input_selector(anchor_transaction.input(), ANCHOR_V2_SELECTOR)?;
    } else {
        validate_input_selector(anchor_transaction.input(), ANCHOR_V1_SELECTOR)?;
    }

    // Ensure the value is zero.
    if anchor_transaction.value() != U256::ZERO {
        return Err(ConsensusError::msg("Anchor transaction value must be zero"));
    }

    // Ensure the gas limit is correct.
    let gas_limit =
        if is_shasta || is_pacaya { ANCHOR_V3_V4_GAS_LIMIT } else { ANCHOR_V1_V2_GAS_LIMIT };
    if anchor_transaction.gas_limit() != gas_limit {
        return Err(ConsensusError::msg(format!(
            "Anchor transaction gas limit must be {gas_limit}, got {}",
            anchor_transaction.gas_limit()
        )));
    }

    if !is_shasta && anchor_transaction.effective_tip_per_gas(ctx.base_fee_per_gas) != Some(0) {
        return Err(ConsensusError::msg("Anchor transaction effective tip must be zero"));
    }

    // The golden-touch account signs the anchor, which calls the treasury address.
    let sender = anchor_transaction.try_recover().map_err(|err| {
        ConsensusError::msg(format!("Anchor transaction sender must be recoverable: {err}"))
    })?;
    if sender != Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS) {
        return Err(ConsensusError::msg(format!(
            "Anchor transaction sender must be the golden-touch account, got {sender}"
        )));
    }

    Ok(())
}

/// Validates the first transaction and requires an anchor in non-genesis Shasta blocks.
pub fn validate_anchor_transaction_in_block<B>(
    block: &RecoveredBlock<B>,
    chain_spec: &TaikoChainSpec,
) -> Result<(), ConsensusError>
where
    B: Block,
{
    let anchor_transaction = match block.body().transactions().first() {
        Some(tx) => tx,
        None if block.number() != 0 && chain_spec.is_shasta_active(block.header().timestamp()) => {
            return Err(ConsensusError::msg("Shasta block is missing its anchor transaction"));
        }
        None => return Ok(()),
    };

    validate_anchor_transaction(
        anchor_transaction,
        chain_spec,
        AnchorValidationContext {
            timestamp: block.header().timestamp(),
            block_number: block.number(),
            base_fee_per_gas: block
                .header()
                .base_fee_per_gas()
                .ok_or_else(|| ConsensusError::msg("Block base fee per gas must be set"))?,
        },
    )
}

/// Validates that transaction input starts with the expected call selector.
pub(super) fn validate_input_selector(
    input: &[u8],
    expected_selector: &[u8; 4],
) -> Result<(), ConsensusError> {
    if !input.starts_with(expected_selector) {
        return Err(ConsensusError::msg(format!(
            "Anchor transaction input data does not match the expected selector: {expected_selector:?}"
        )));
    }
    Ok(())
}
