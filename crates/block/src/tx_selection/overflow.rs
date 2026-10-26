//! Memory of transactions that overflowed the block zk gas budget during selection.
//!
//! When a candidate overflows the zk gas budget, selection records its hash together with a
//! digest of the execution state it ran against: the block environment from
//! [`selection_state_seed`], advanced by [`advance_selection_state`] for every transaction
//! committed before it. A later pass that reaches the same digest skips the transaction without
//! executing it again. A different digest means the transaction may now fit, so it runs again,
//! but only while the selection call's re-run allowance lasts; past it, the transaction is skipped.
//!
//! An entry is removed only when its transaction leaves the pool ([`ZkOverflowMemory::retain`])
//! or later executes within the budget ([`ZkOverflowMemory::forget`]), so pruning against the
//! pool keeps the memory no larger than the pool. There is no eviction by size and no expiry,
//! because an expiry would let the same transactions come due again as new.

use crate::config::TaikoNextBlockEnvAttributes;
use alloy_primitives::{B256, Keccak256, TxHash};
use core::fmt;
use reth_transaction_pool::error::PoolTransactionError;
use std::{
    collections::HashMap,
    error::Error,
    fmt::{Display, Formatter},
    sync::{Mutex, MutexGuard, PoisonError},
};

/// Transactions that overflowed the block zk gas budget, each mapped to the digest of the
/// execution state it overflowed at.
#[derive(Debug, Default)]
pub struct ZkOverflowMemory {
    /// Remembered execution-state digest for each overflowing transaction hash.
    states: Mutex<HashMap<TxHash, B256>>,
}

impl ZkOverflowMemory {
    /// Remembers that `tx` overflowed at `state`, replacing any earlier state for it.
    pub fn record(&self, tx: TxHash, state: B256) {
        self.lock().insert(tx, state);
    }

    /// Returns the state `tx` last overflowed at, if it is remembered.
    pub fn state(&self, tx: &TxHash) -> Option<B256> {
        self.lock().get(tx).copied()
    }

    /// Drops the entry for `tx`, if any.
    pub fn forget(&self, tx: &TxHash) {
        self.lock().remove(tx);
    }

    /// Keeps only the entries whose transaction hash satisfies `keep`.
    pub fn retain(&self, keep: impl Fn(&TxHash) -> bool) {
        self.lock().retain(|tx, _| keep(tx));
    }

    /// Returns the number of remembered transactions.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Returns `true` if no transaction is remembered.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Locks the map. Every operation leaves the map consistent, so a lock poisoned by a
    /// panicking holder is recovered instead of propagated.
    fn lock(&self) -> MutexGuard<'_, HashMap<TxHash, B256>> {
        self.states.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Returns the digest of the execution state selection starts from: keccak256 over the parent hash
/// and every field of the next block's environment, advanced by [`advance_selection_state`] for
/// each hash in `committed`, in order.
///
/// The environment covers everything the block context hands a transaction: the timestamp, fee
/// recipient, `prev_randao`, gas limit and base fee, plus `extra_data` and the parent beacon block
/// root, which reach execution through fee sharing and the EIP-4788 system call. A change in any of
/// them can change what a transaction costs, so it leads to a re-run rather than a skip. Integers
/// are encoded as 8-byte big-endian, `extra_data` is length-prefixed and the beacon root is
/// preceded by a presence byte.
pub fn selection_state_seed(
    parent_hash: B256,
    env: &TaikoNextBlockEnvAttributes,
    committed: &[TxHash],
) -> B256 {
    // Destructured so that a new environment field fails to compile until it is hashed here.
    let TaikoNextBlockEnvAttributes {
        timestamp,
        suggested_fee_recipient,
        prev_randao,
        gas_limit,
        extra_data,
        base_fee_per_gas,
        parent_beacon_block_root,
    } = env;

    let mut hasher = Keccak256::new();
    hasher.update(parent_hash);
    hasher.update(timestamp.to_be_bytes());
    hasher.update(suggested_fee_recipient);
    hasher.update(prev_randao);
    hasher.update(gas_limit.to_be_bytes());
    hasher.update(base_fee_per_gas.to_be_bytes());
    hasher.update((extra_data.len() as u64).to_be_bytes());
    hasher.update(extra_data);
    match parent_beacon_block_root {
        Some(root) => {
            hasher.update([1u8]);
            hasher.update(root);
        }
        None => hasher.update([0u8]),
    }
    committed.iter().fold(hasher.finalize(), |state, tx| advance_selection_state(state, *tx))
}

/// Returns the execution-state digest after `tx` is committed on top of `state`:
/// keccak256(`state ‖ tx`).
pub fn advance_selection_state(state: B256, tx: TxHash) -> B256 {
    let mut hasher = Keccak256::new();
    hasher.update(state);
    hasher.update(tx);
    hasher.finalize()
}

/// Error raised when a transaction overflows the block zk gas budget.
#[derive(Debug)]
pub(crate) struct ZkGasBudgetExceeded;

impl Display for ZkGasBudgetExceeded {
    /// Formats the zk gas budget exceeded error message.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("transaction exceeds the block zk gas budget")
    }
}

impl Error for ZkGasBudgetExceeded {}

impl PoolTransactionError for ZkGasBudgetExceeded {
    /// Indicates that this error does not represent a bad transaction.
    fn is_bad_transaction(&self) -> bool {
        false
    }

    /// Allows downcasting to the concrete error type.
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, Bytes};

    /// Returns a distinct transaction hash for tests.
    fn tx_hash(byte: u8) -> TxHash {
        TxHash::with_last_byte(byte)
    }

    #[test]
    fn memory_records_overwrites_forgets_and_retains() {
        let memory = ZkOverflowMemory::default();
        assert!(memory.is_empty());
        assert_eq!(memory.state(&tx_hash(1)), None);

        memory.record(tx_hash(1), B256::with_last_byte(10));
        memory.record(tx_hash(2), B256::with_last_byte(20));
        assert_eq!(memory.len(), 2);
        assert_eq!(memory.state(&tx_hash(1)), Some(B256::with_last_byte(10)));
        assert_eq!(memory.state(&tx_hash(2)), Some(B256::with_last_byte(20)));

        // Recording again overwrites the remembered state instead of adding an entry.
        memory.record(tx_hash(1), B256::with_last_byte(11));
        assert_eq!(memory.len(), 2);
        assert_eq!(memory.state(&tx_hash(1)), Some(B256::with_last_byte(11)));

        memory.forget(&tx_hash(1));
        assert_eq!(memory.state(&tx_hash(1)), None);
        assert_eq!(memory.len(), 1);
        // Forgetting an absent hash is a no-op.
        memory.forget(&tx_hash(1));
        assert_eq!(memory.len(), 1);

        memory.record(tx_hash(3), B256::with_last_byte(30));
        memory.retain(|tx| *tx == tx_hash(3));
        assert_eq!(memory.len(), 1);
        assert!(!memory.is_empty());
        assert_eq!(memory.state(&tx_hash(2)), None);
        assert_eq!(memory.state(&tx_hash(3)), Some(B256::with_last_byte(30)));

        memory.retain(|_| false);
        assert!(memory.is_empty());
    }

    fn test_env() -> TaikoNextBlockEnvAttributes {
        TaikoNextBlockEnvAttributes {
            timestamp: 1,
            suggested_fee_recipient: Address::with_last_byte(2),
            prev_randao: B256::with_last_byte(3),
            gas_limit: 4,
            extra_data: Bytes::from_static(&[5]),
            base_fee_per_gas: 6,
            parent_beacon_block_root: Some(B256::with_last_byte(7)),
        }
    }

    #[test]
    fn state_seed_changes_with_every_input() {
        let parent = B256::with_last_byte(1);
        let env = test_env();
        let seed = selection_state_seed(parent, &env, &[]);
        assert_eq!(seed, selection_state_seed(parent, &test_env(), &[]));
        assert_ne!(selection_state_seed(B256::with_last_byte(9), &env, &[]), seed);

        let changes: [fn(&mut TaikoNextBlockEnvAttributes); 8] = [
            |env| env.timestamp += 1,
            |env| env.suggested_fee_recipient = Address::with_last_byte(9),
            |env| env.prev_randao = B256::with_last_byte(9),
            |env| env.gas_limit += 1,
            |env| env.extra_data = Bytes::from_static(&[5, 0]),
            |env| env.base_fee_per_gas += 1,
            |env| env.parent_beacon_block_root = None,
            |env| env.parent_beacon_block_root = Some(B256::ZERO),
        ];
        let variants = changes.map(|change| {
            let mut env = test_env();
            change(&mut env);
            selection_state_seed(parent, &env, &[])
        });
        for (i, variant) in variants.iter().enumerate() {
            assert_ne!(*variant, seed, "change {i} left the seed unchanged");
        }
        // A missing beacon root and a zero one are told apart.
        assert_ne!(variants[6], variants[7]);

        // Committed hashes are folded through `advance_selection_state`.
        assert_eq!(
            selection_state_seed(parent, &env, &[tx_hash(5)]),
            advance_selection_state(seed, tx_hash(5)),
        );
    }

    #[test]
    fn advance_selection_state_is_order_sensitive() {
        let start = B256::with_last_byte(1);
        let a_then_b =
            advance_selection_state(advance_selection_state(start, tx_hash(2)), tx_hash(3));
        let b_then_a =
            advance_selection_state(advance_selection_state(start, tx_hash(3)), tx_hash(2));

        assert_ne!(a_then_b, b_then_a);
        assert_ne!(advance_selection_state(start, tx_hash(2)), start);
        assert_eq!(
            a_then_b,
            advance_selection_state(advance_selection_state(start, tx_hash(2)), tx_hash(3)),
        );
    }

    #[test]
    fn zk_gas_budget_exceeded_is_not_a_bad_transaction() {
        assert!(!ZkGasBudgetExceeded.is_bad_transaction());
        assert!(ZkGasBudgetExceeded.as_any().is::<ZkGasBudgetExceeded>());
    }
}
