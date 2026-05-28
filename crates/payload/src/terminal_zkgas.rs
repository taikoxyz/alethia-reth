//! In-process handoff for terminal zk-gas transactions observed during payload building.

use std::{
    collections::{HashMap, VecDeque},
    sync::{LazyLock, Mutex},
};

use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::PayloadId;

/// Maximum number of unclaimed terminal zk-gas transactions retained in memory.
const MAX_PENDING_TERMINAL_ZKGAS_TXS: usize = 256;

/// Terminal transaction metadata captured before payload filtering drops the transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTerminalZkGasTx {
    /// Canonical block hash of the sealed payload.
    pub block_hash: B256,
    /// Hash of the filtered transaction.
    pub tx_hash: B256,
    /// EIP-2718 encoded signed transaction bytes.
    pub tx_rlp: Bytes,
}

/// Bounded pending terminal transactions keyed by Engine API payload id.
#[derive(Debug, Default)]
struct PendingTerminalZkGasRegistry {
    /// Captured terminal transactions by payload id.
    entries: HashMap<PayloadId, PendingTerminalZkGasTx>,
    /// Insertion order used to evict stale entries that were never consumed.
    order: VecDeque<PayloadId>,
}

impl PendingTerminalZkGasRegistry {
    /// Inserts a pending terminal transaction and evicts the oldest stale entries.
    fn insert(&mut self, payload_id: PayloadId, tx: PendingTerminalZkGasTx) {
        if self.entries.insert(payload_id, tx).is_none() {
            self.order.push_back(payload_id);
        }
        self.evict_stale_entries();
    }

    /// Returns a pending terminal transaction without consuming it.
    fn peek(&self, payload_id: PayloadId) -> Option<PendingTerminalZkGasTx> {
        self.entries.get(&payload_id).cloned()
    }

    /// Removes a pending terminal transaction after it is durably persisted or discarded.
    fn remove(&mut self, payload_id: PayloadId) -> Option<PendingTerminalZkGasTx> {
        let tx = self.entries.remove(&payload_id);
        if tx.is_some() {
            self.order.retain(|id| *id != payload_id);
        }
        tx
    }

    /// Keeps the registry bounded while tolerating duplicate stale order entries.
    fn evict_stale_entries(&mut self) {
        while self.order.front().is_some_and(|id| !self.entries.contains_key(id)) {
            self.order.pop_front();
        }
        while self.entries.len() > MAX_PENDING_TERMINAL_ZKGAS_TXS {
            let Some(oldest) = self.order.pop_front() else { break };
            self.entries.remove(&oldest);
        }
    }
}

/// Pending terminal transaction registry shared by payload build and Engine API consumption.
static PENDING_TERMINAL_ZKGAS_TXS: LazyLock<Mutex<PendingTerminalZkGasRegistry>> =
    LazyLock::new(|| Mutex::new(PendingTerminalZkGasRegistry::default()));

/// Records a terminal zk-gas transaction for a sealed payload.
pub fn record_terminal_zkgas_tx(payload_id: PayloadId, tx: PendingTerminalZkGasTx) {
    let mut pending = PENDING_TERMINAL_ZKGAS_TXS.lock().expect("terminal zkgas registry poisoned");
    pending.insert(payload_id, tx);
}

/// Returns a previously recorded terminal zk-gas transaction for `payload_id`.
pub fn peek_terminal_zkgas_tx(payload_id: PayloadId) -> Option<PendingTerminalZkGasTx> {
    let pending = PENDING_TERMINAL_ZKGAS_TXS.lock().expect("terminal zkgas registry poisoned");
    pending.peek(payload_id)
}

/// Removes a previously recorded terminal zk-gas transaction for `payload_id`.
pub fn remove_terminal_zkgas_tx(payload_id: PayloadId) -> Option<PendingTerminalZkGasTx> {
    let mut pending = PENDING_TERMINAL_ZKGAS_TXS.lock().expect("terminal zkgas registry poisoned");
    pending.remove(payload_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_terminal_tx() -> PendingTerminalZkGasTx {
        PendingTerminalZkGasTx {
            block_hash: B256::repeat_byte(0x11),
            tx_hash: B256::repeat_byte(0x33),
            tx_rlp: vec![0x01, 0x02].into(),
        }
    }

    fn payload_id(value: u64) -> PayloadId {
        PayloadId::new(value.to_be_bytes())
    }

    #[test]
    fn terminal_zkgas_registry_takes_once() {
        let payload_id = payload_id(1);
        let pending = pending_terminal_tx();

        record_terminal_zkgas_tx(payload_id, pending.clone());

        assert_eq!(peek_terminal_zkgas_tx(payload_id), Some(pending.clone()));
        assert_eq!(remove_terminal_zkgas_tx(payload_id), Some(pending));
        assert_eq!(peek_terminal_zkgas_tx(payload_id), None);
    }

    #[test]
    fn terminal_zkgas_registry_take_removes_order_entry() {
        let mut registry = PendingTerminalZkGasRegistry::default();
        let pending = pending_terminal_tx();
        let payload_id = payload_id(1);

        registry.insert(payload_id, pending.clone());

        assert_eq!(registry.remove(payload_id), Some(pending));
        assert!(registry.order.is_empty());
    }

    #[test]
    fn terminal_zkgas_registry_is_bounded() {
        let mut registry = PendingTerminalZkGasRegistry::default();
        let pending = pending_terminal_tx();

        for i in 0..MAX_PENDING_TERMINAL_ZKGAS_TXS + 1 {
            registry.insert(payload_id(i as u64), pending.clone());
        }

        assert_eq!(registry.entries.len(), MAX_PENDING_TERMINAL_ZKGAS_TXS);
        assert_eq!(registry.remove(payload_id(0)), None);
        assert_eq!(registry.remove(payload_id(1)), Some(pending));
    }
}
