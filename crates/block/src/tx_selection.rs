//! Transaction selection utilities for building blocks from the transaction pool.
//!
//! This module provides a unified interface for selecting and executing transactions
//! from the mempool, used by both the payload builder and the RPC pre-building endpoint.

use alloy_eips::Encodable2718;
use alloy_primitives::Address;
use alloy_rlp::encode_list;
use op_alloy_flz::tx_estimated_size_fjord_bytes;
use reth_ethereum::{EthPrimitives, TransactionSigned};
use reth_evm::{
    block::{BlockExecutionError, BlockValidationError, InternalBlockExecutionError},
    execute::BlockBuilder,
};
use reth_primitives::Recovered;
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_transaction_pool::{
    BestTransactionsAttributes, PoolTransaction, TransactionPool,
    error::InvalidPoolTransactionError,
};
use tracing::trace;

/// Configuration for transaction selection.
#[derive(Debug, Clone)]
pub struct TxSelectionConfig {
    /// Base fee per gas for tip calculation.
    pub base_fee: u64,
    /// Maximum gas allowed per list.
    pub gas_limit_per_list: u64,
    /// Maximum DA bytes allowed per list (compressed size).
    pub max_da_bytes_per_list: u64,
    /// When non-zero, run a full RLP+zlib size check once the estimated list size is within this
    /// many bytes of the limit.
    pub da_size_zlib_guard_bytes: u64,
    /// Maximum number of transaction lists to produce.
    pub max_lists: usize,
    /// Minimum tip required for a transaction to be included.
    pub min_tip: u64,
    /// Local accounts to prioritize.
    /// If non-empty, only transactions from these accounts are included.
    pub locals: Vec<Address>,
}

/// A successfully executed transaction with metadata.
#[derive(Debug, Clone)]
pub struct ExecutedTx {
    /// The executed transaction.
    pub tx: Recovered<TransactionSigned>,
    /// Gas used by the transaction.
    pub gas_used: u64,
    /// Estimated DA size (compressed).
    pub da_size: u64,
}

/// A list of executed transactions with cumulative statistics.
#[derive(Debug, Clone, Default)]
pub struct ExecutedTxList {
    /// The executed transactions in this list.
    pub transactions: Vec<ExecutedTx>,
    /// Total gas used by all transactions in this list.
    pub total_gas_used: u64,
    /// Total DA bytes used by all transactions in this list.
    pub total_da_bytes: u64,
}

/// Outcome of the transaction selection process.
#[derive(Debug)]
pub enum SelectionOutcome {
    /// Selection was cancelled before completion.
    Cancelled,
    /// Selection completed successfully with the produced lists.
    Completed(Vec<ExecutedTxList>),
}

/// Default threshold for triggering the zlib guard check.
pub const DEFAULT_DA_ZLIB_GUARD_BYTES: u64 = 4 * 1024;

const ZLIB_COMPRESSION_LEVEL: u8 = 6;

/// Creates an internal error for when the transaction list invariant is violated.
fn lists_empty_error() -> BlockExecutionError {
    BlockExecutionError::Internal(InternalBlockExecutionError::msg(
        "tx selection invariant violated: lists is empty",
    ))
}

fn should_check_zlib_da_size(config: &TxSelectionConfig, estimated_after: u64) -> bool {
    config.da_size_zlib_guard_bytes > 0 &&
        estimated_after.saturating_add(config.da_size_zlib_guard_bytes) >= config.max_da_bytes_per_list
}

fn zlib_tx_list_size_bytes(
    list: &ExecutedTxList,
    candidate: &Recovered<TransactionSigned>,
) -> u64 {
    let mut txs: Vec<&TransactionSigned> = Vec::with_capacity(list.transactions.len() + 1);
    txs.extend(list.transactions.iter().map(|etx| etx.tx.inner()));
    txs.push(candidate.inner());

    let mut rlp_bytes = Vec::new();
    encode_list::<&TransactionSigned, TransactionSigned>(&txs, &mut rlp_bytes);

    miniz_oxide::deflate::compress_to_vec_zlib(&rlp_bytes, ZLIB_COMPRESSION_LEVEL).len() as u64
}

fn exceeds_da_limit(
    list: &ExecutedTxList,
    tx: &Recovered<TransactionSigned>,
    tx_da_estimated: u64,
    config: &TxSelectionConfig,
) -> bool {
    let estimated_after = list.total_da_bytes.saturating_add(tx_da_estimated);
    if estimated_after > config.max_da_bytes_per_list {
        return true;
    }

    if should_check_zlib_da_size(config, estimated_after) {
        let actual = zlib_tx_list_size_bytes(list, tx);
        return actual > config.max_da_bytes_per_list;
    }

    false
}

fn exceeds_list_limits(
    list: &ExecutedTxList,
    tx: &Recovered<TransactionSigned>,
    tx_gas_limit: u64,
    tx_da_estimated: u64,
    config: &TxSelectionConfig,
) -> (bool, bool) {
    let exceeds_gas = list.total_gas_used.saturating_add(tx_gas_limit) > config.gas_limit_per_list;
    let exceeds_da = exceeds_da_limit(list, tx, tx_da_estimated, config);
    (exceeds_gas, exceeds_da)
}

/// Selects and executes transactions from the pool.
///
/// This function iterates through the best transactions in the pool, applying
/// the configured filters and limits, and executes them against the provided
/// block builder.
///
/// # Arguments
///
/// * `builder` - The block builder to execute transactions against.
/// * `pool` - The transaction pool to select from.
/// * `config` - Configuration for transaction selection.
/// * `is_cancelled` - A function that returns true if selection should be cancelled.
///
/// # Returns
///
/// * `Ok(SelectionOutcome::Cancelled)` - If cancelled during selection.
/// * `Ok(SelectionOutcome::Completed(lists))` - If selection completed successfully.
/// * `Err(err)` - If a fatal execution error occurred.
pub fn select_and_execute_pool_transactions<B, Pool>(
    builder: &mut B,
    pool: &Pool,
    config: &TxSelectionConfig,
    is_cancelled: impl Fn() -> bool,
) -> Result<SelectionOutcome, BlockExecutionError>
where
    B: BlockBuilder<Primitives = EthPrimitives>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
{
    let mut best_txs = pool
        .best_transactions_with_attributes(BestTransactionsAttributes::new(config.base_fee, None));

    let mut lists = vec![ExecutedTxList::default()];

    while let Some(pool_tx) = best_txs.next() {
        // 1. Check cancellation
        if is_cancelled() {
            return Ok(SelectionOutcome::Cancelled);
        }

        // 2. Filter by locals (if configured)
        if !config.locals.is_empty() && !config.locals.contains(&pool_tx.sender()) {
            // Mark as underpriced to skip this transaction and its dependents
            best_txs.mark_invalid(&pool_tx, &InvalidPoolTransactionError::Underpriced);
            continue;
        }

        // 3. Filter by min_tip
        let tip = pool_tx.effective_tip_per_gas(config.base_fee);
        if tip.is_none_or(|t| t < config.min_tip as u128) {
            trace!(target: "tx_selection", ?pool_tx, "skipping transaction with insufficient tip");
            best_txs.mark_invalid(&pool_tx, &InvalidPoolTransactionError::Underpriced);
            continue;
        }

        // 4. Calculate DA size upfront (needed for limit checks)
        let tx = pool_tx.to_consensus();
        let da_size = tx_estimated_size_fjord_bytes(&tx.encoded_2718());

        // 5. Early reject transactions that cannot fit in any list
        if pool_tx.gas_limit() > config.gas_limit_per_list {
            best_txs.mark_invalid(
                &pool_tx,
                &InvalidPoolTransactionError::ExceedsGasLimit(
                    pool_tx.gas_limit(),
                    config.gas_limit_per_list,
                ),
            );
            continue;
        }
        if da_size > config.max_da_bytes_per_list {
            best_txs.mark_invalid(&pool_tx, &InvalidPoolTransactionError::Underpriced);
            continue;
        }

        // 6. Check if transaction fits in current list; if not, try starting a new one
        let current = lists.last().ok_or_else(lists_empty_error)?;
        let (exceeds_gas, exceeds_da) =
            exceeds_list_limits(current, &tx, pool_tx.gas_limit(), da_size, config);

        if exceeds_gas || exceeds_da {
            if lists.len() >= config.max_lists {
                // Can't fit in any list
                if exceeds_gas {
                    best_txs.mark_invalid(
                        &pool_tx,
                        &InvalidPoolTransactionError::ExceedsGasLimit(
                            pool_tx.gas_limit(),
                            config.gas_limit_per_list,
                        ),
                    );
                } else {
                    best_txs.mark_invalid(&pool_tx, &InvalidPoolTransactionError::Underpriced);
                }
                continue;
            }
            // Start a new list
            lists.push(ExecutedTxList::default());

            let current = lists.last().ok_or_else(lists_empty_error)?;
            let (exceeds_gas, exceeds_da) =
                exceeds_list_limits(current, &tx, pool_tx.gas_limit(), da_size, config);
            if exceeds_gas || exceeds_da {
                if exceeds_gas {
                    best_txs.mark_invalid(
                        &pool_tx,
                        &InvalidPoolTransactionError::ExceedsGasLimit(
                            pool_tx.gas_limit(),
                            config.gas_limit_per_list,
                        ),
                    );
                } else {
                    best_txs.mark_invalid(&pool_tx, &InvalidPoolTransactionError::Underpriced);
                }
                continue;
            }
        }

        // 7. Execute transaction
        let gas_used = match builder.execute_transaction(tx.clone()) {
            Ok(gas_used) => gas_used,
            Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                error, ..
            })) => {
                if error.is_nonce_too_low() {
                    // Nonce too low - just skip, don't mark invalid
                    // (could be a race condition, transaction might be valid later)
                    trace!(target: "tx_selection", %error, ?tx, "skipping nonce too low transaction");
                } else {
                    // Other validation error - mark invalid to skip dependents
                    trace!(target: "tx_selection", %error, ?tx, "skipping invalid transaction and its descendants");
                    best_txs.mark_invalid(
                        &pool_tx,
                        &InvalidPoolTransactionError::Consensus(
                            InvalidTransactionError::TxTypeNotSupported,
                        ),
                    );
                }
                continue;
            }
            // Fatal error - stop selection
            Err(err) => return Err(err),
        };

        // 8. Record successful transaction
        let current = lists.last_mut().ok_or_else(lists_empty_error)?;
        current.total_gas_used += gas_used;
        current.total_da_bytes += da_size;
        current.transactions.push(ExecutedTx { tx, gas_used, da_size });

        trace!(target: "tx_selection", gas_used, da_size, "included transaction from pool");
    }

    Ok(SelectionOutcome::Completed(lists))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_primitives::{Address, Bytes, B256, ChainId, Signature, TxKind, U256};

    fn make_signed_legacy_tx(input: Bytes) -> TransactionSigned {
        let tx = TxLegacy {
            chain_id: Some(ChainId::from(1u64)),
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input,
        };
        let signature = Signature::new(U256::from(1), U256::from(2), false);
        let signed = Signed::new_unchecked(tx, signature, B256::ZERO);
        signed.into()
    }

    fn lcg_bytes(len: usize) -> Bytes {
        let mut out = Vec::with_capacity(len);
        let mut x = 0u8;
        for _ in 0..len {
            x = x.wrapping_mul(73).wrapping_add(41);
            out.push(x);
        }
        Bytes::from(out)
    }

    #[test]
    fn zlib_guard_rejects_when_actual_exceeds_limit() {
        let list = ExecutedTxList::default();
        let mut candidate = None;

        for len in (64..=2048).step_by(64) {
            let tx = make_signed_legacy_tx(lcg_bytes(len));
            let estimated = tx_estimated_size_fjord_bytes(&tx.encoded_2718());
            let recovered = Recovered::new_unchecked(tx, Address::ZERO);
            let actual = zlib_tx_list_size_bytes(&list, &recovered);
            if actual > estimated {
                candidate = Some((recovered, estimated, actual));
                break;
            }
        }

        let (recovered, estimated, actual) =
            candidate.expect("no tx found where zlib size exceeds estimate");

        let mut config = TxSelectionConfig {
            base_fee: 0,
            gas_limit_per_list: u64::MAX,
            max_da_bytes_per_list: estimated,
            da_size_zlib_guard_bytes: 1,
            max_lists: 1,
            min_tip: 0,
            locals: vec![],
        };

        assert!(actual > config.max_da_bytes_per_list);
        assert!(exceeds_da_limit(&list, &recovered, estimated, &config));

        config.da_size_zlib_guard_bytes = 0;
        assert!(!exceeds_da_limit(&list, &recovered, estimated, &config));
    }
}
