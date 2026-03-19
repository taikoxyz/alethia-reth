//! Transaction-type rules shared across Taiko block-building paths.

use reth_primitives_traits::SignedTransaction;

/// Returns whether a transaction type is allowed inside a Taiko block.
///
/// Taiko does not accept blob transactions, even when Osaka/Uzen-specific EVM rules are active.
pub fn is_allowed_tx_type(tx: &impl SignedTransaction) -> bool {
    !tx.is_eip4844()
}
