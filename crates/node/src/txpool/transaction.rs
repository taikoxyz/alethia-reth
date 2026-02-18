//! Taiko pooled transaction type

use alethia_reth_consensus::transaction::TaikoTxEnvelope;
use alloy_consensus::{BlobTransactionValidationError, Transaction};
use alloy_eips::{
    eip2718::{Encodable2718, Typed2718},
    eip7594::BlobTransactionSidecarVariant,
};
use alloy_primitives::{Address, B256, Bytes, TxHash, TxKind, U256};
use reth_primitives_traits::{InMemorySize, Recovered};
use reth_transaction_pool::{EthBlobTransactionSidecar, EthPoolTransaction, PoolTransaction};
use std::sync::Arc;

/// The default [`PoolTransaction`] for Taiko.
///
/// This type wraps a consensus transaction with additional cached data that's
/// frequently accessed by the pool for transaction ordering and validation:
///
/// - `cost`: Pre-calculated max cost (gas * price + value)
/// - `encoded_length`: Cached RLP encoding length for size limits
///
/// This avoids recalculating these values repeatedly during pool operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaikoPooledTransaction {
    /// `EcRecovered` transaction, the consensus format.
    pub transaction: Recovered<TaikoTxEnvelope>,

    /// For EIP-1559 transactions: `max_fee_per_gas * gas_limit + tx_value`.
    /// For legacy transactions: `gas_price * gas_limit + tx_value`.
    pub cost: U256,

    /// This is the RLP length of the transaction, computed when the transaction is added to the
    /// pool.
    pub encoded_length: usize,

    /// Cached transaction hash
    tx_hash: TxHash,
}

impl TaikoPooledTransaction {
    /// Create new instance of [Self].
    pub fn new(transaction: Recovered<TaikoTxEnvelope>, encoded_length: usize) -> Self {
        let gas_cost = U256::from(transaction.max_fee_per_gas())
            .saturating_mul(U256::from(transaction.gas_limit()));

        let cost = gas_cost.saturating_add(transaction.value());
        let tx_hash = transaction.tx_hash();

        Self { transaction, cost, encoded_length, tx_hash }
    }

    /// Return the reference to the underlying transaction.
    pub const fn transaction(&self) -> &Recovered<TaikoTxEnvelope> {
        &self.transaction
    }
}

impl PoolTransaction for TaikoPooledTransaction {
    type TryFromConsensusError = alloy_consensus::error::ValueError<TaikoTxEnvelope>;

    type Consensus = TaikoTxEnvelope;

    // The pooled type is the consensus TaikoPooledTransaction for network compatibility
    type Pooled = alethia_reth_consensus::transaction::TaikoPooledTransaction;

    fn clone_into_consensus(&self) -> Recovered<Self::Consensus> {
        self.transaction().clone()
    }

    fn into_consensus(self) -> Recovered<Self::Consensus> {
        self.transaction
    }

    fn from_pooled(tx: Recovered<Self::Pooled>) -> Self {
        // Convert consensus pooled transaction to envelope
        let envelope: TaikoTxEnvelope = tx.inner().clone().into();
        let recovered = Recovered::new_unchecked(envelope, *tx.signer_ref());
        let encoded_length = recovered.encode_2718_len();
        Self::new(recovered, encoded_length)
    }

    /// Returns hash of the transaction.
    fn hash(&self) -> &TxHash {
        &self.tx_hash
    }

    /// Returns the Sender of the transaction.
    fn sender(&self) -> Address {
        self.transaction.signer()
    }

    /// Returns a reference to the Sender of the transaction.
    fn sender_ref(&self) -> &Address {
        self.transaction.signer_ref()
    }

    /// Returns the cost that this transaction is allowed to consume:
    ///
    /// For EIP-1559 transactions: `max_fee_per_gas * gas_limit + tx_value`.
    /// For legacy transactions: `gas_price * gas_limit + tx_value`.
    fn cost(&self) -> &U256 {
        &self.cost
    }

    /// Returns the length of the rlp encoded object
    fn encoded_length(&self) -> usize {
        self.encoded_length
    }
}

impl Typed2718 for TaikoPooledTransaction {
    fn ty(&self) -> u8 {
        self.transaction.ty()
    }
}

impl InMemorySize for TaikoPooledTransaction {
    fn size(&self) -> usize {
        self.transaction.size()
    }
}

impl Transaction for TaikoPooledTransaction {
    fn chain_id(&self) -> Option<alloy_primitives::ChainId> {
        self.transaction.chain_id()
    }

    fn nonce(&self) -> u64 {
        self.transaction.nonce()
    }

    fn gas_limit(&self) -> u64 {
        self.transaction.gas_limit()
    }

    fn gas_price(&self) -> Option<u128> {
        self.transaction.gas_price()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.transaction.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.transaction.max_priority_fee_per_gas()
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        self.transaction.max_fee_per_blob_gas()
    }

    fn priority_fee_or_price(&self) -> u128 {
        self.transaction.priority_fee_or_price()
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        self.transaction.effective_gas_price(base_fee)
    }

    fn is_dynamic_fee(&self) -> bool {
        self.transaction.is_dynamic_fee()
    }

    fn kind(&self) -> TxKind {
        self.transaction.kind()
    }

    fn is_create(&self) -> bool {
        self.transaction.is_create()
    }

    fn value(&self) -> U256 {
        self.transaction.value()
    }

    fn input(&self) -> &Bytes {
        self.transaction.input()
    }

    fn access_list(&self) -> Option<&alloy_eips::eip2930::AccessList> {
        self.transaction.access_list()
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        self.transaction.blob_versioned_hashes()
    }

    fn authorization_list(&self) -> Option<&[alloy_eips::eip7702::SignedAuthorization]> {
        self.transaction.authorization_list()
    }
}

impl EthPoolTransaction for TaikoPooledTransaction {
    fn take_blob(&mut self) -> EthBlobTransactionSidecar {
        // Taiko doesn't support blobs
        EthBlobTransactionSidecar::None
    }

    fn try_into_pooled_eip4844(
        self,
        _sidecar: Arc<BlobTransactionSidecarVariant>,
    ) -> Option<Recovered<Self::Pooled>> {
        // Taiko doesn't support EIP-4844
        None
    }

    fn try_from_eip4844(
        _tx: Recovered<Self::Consensus>,
        _sidecar: BlobTransactionSidecarVariant,
    ) -> Option<Self> {
        // Taiko doesn't support EIP-4844
        None
    }

    fn validate_blob(
        &self,
        _blob: &BlobTransactionSidecarVariant,
        _settings: &alloy_eips::eip4844::env_settings::KzgSettings,
    ) -> Result<(), BlobTransactionValidationError> {
        // Taiko doesn't support blobs
        Err(BlobTransactionValidationError::NotBlobTransaction(self.ty()))
    }
}
