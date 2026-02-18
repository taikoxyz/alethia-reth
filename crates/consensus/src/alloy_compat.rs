//! Alloy compatibility implementations for Taiko types.
//!
//! This module contains conversions and trait implementations that bridge
//! between Taiko types and Alloy types (RPC, network, etc.).

use crate::transaction::{TaikoTxEnvelope, TaikoTxType, TaikoTypedTransaction};
use alloy_consensus::{TxEip1559, TxEip2930, TxEip7702, TxLegacy};
use alloy_primitives::{Address, Signature};
use alloy_rpc_types_eth::TransactionRequest;

// ============================================================================
// TaikoTxEnvelope conversions
// ============================================================================

/// Converts an RPC [`Transaction`](alloy_rpc_types_eth::Transaction) into a
/// [`TaikoTxEnvelope`].
///
/// # Panics
///
/// Panics if the inner transaction is an EIP-4844 blob transaction, which Taiko
/// does not support.
impl From<alloy_rpc_types_eth::Transaction> for TaikoTxEnvelope {
    fn from(tx: alloy_rpc_types_eth::Transaction) -> Self {
        let envelope = tx.inner.into_inner();
        Self::try_from_eth_envelope(envelope)
            .expect("TaikoTxEnvelope does not support EIP-4844 blob transactions")
    }
}

impl From<TaikoTxEnvelope> for TransactionRequest {
    fn from(value: TaikoTxEnvelope) -> Self {
        match value {
            TaikoTxEnvelope::Eip2930(tx) => tx.into_parts().0.into(),
            TaikoTxEnvelope::Eip1559(tx) => tx.into_parts().0.into(),
            TaikoTxEnvelope::Eip7702(tx) => tx.into_parts().0.into(),
            TaikoTxEnvelope::Legacy(tx) => tx.into_parts().0.into(),
        }
    }
}

impl TaikoTxEnvelope {
    /// Attempts to convert an ethereum [`TxEnvelope`] into the taiko variant.
    ///
    /// Returns the given envelope as error if [`TaikoTxEnvelope`] doesn't support the variant
    /// (EIP-4844)
    #[allow(clippy::result_large_err)]
    pub fn try_from_any_envelope(
        tx: alloy_network::AnyTxEnvelope,
    ) -> Result<Self, alloy_network::AnyTxEnvelope> {
        match tx.try_into_envelope() {
            Ok(eth) => {
                Self::try_from_eth_envelope(eth).map_err(alloy_network::AnyTxEnvelope::Ethereum)
            }
            Err(err) => {
                let unsupported = err.into_value();
                Err(unsupported)
            }
        }
    }
}

// ============================================================================
// TaikoTypedTransaction conversions
// ============================================================================

impl From<TaikoTypedTransaction> for TransactionRequest {
    fn from(tx: TaikoTypedTransaction) -> Self {
        match tx {
            TaikoTypedTransaction::Legacy(tx) => tx.into(),
            TaikoTypedTransaction::Eip2930(tx) => tx.into(),
            TaikoTypedTransaction::Eip1559(tx) => tx.into(),
            TaikoTypedTransaction::Eip7702(tx) => tx.into(),
        }
    }
}

// ============================================================================
// RPC trait implementations
// ============================================================================

/// Implementation of SignableTxRequest for TransactionRequest with TaikoTxEnvelope
/// This allows converting RPC transaction requests into signable Taiko transactions
impl reth_rpc_eth_api::SignableTxRequest<TaikoTxEnvelope> for TransactionRequest {
    async fn try_build_and_sign(
        self,
        signer: impl alloy_network::TxSigner<Signature> + Send,
    ) -> Result<TaikoTxEnvelope, reth_rpc_eth_api::SignTxRequestError> {
        use alloy_consensus::TypedTransaction;
        use reth_rpc_eth_api::SignTxRequestError;

        // Build the typed transaction from the request (returns EthereumTypedTransaction)
        let eth_typed_tx =
            self.build_typed_tx().map_err(|_| SignTxRequestError::InvalidTransactionRequest)?;

        // Convert EthereumTypedTransaction to TaikoTypedTransaction
        // by extracting and wrapping each variant
        // Note: Taiko doesn't support EIP-4844 (blob transactions)
        let mut taiko_typed_tx = match eth_typed_tx {
            TypedTransaction::Legacy(tx) => TaikoTypedTransaction::Legacy(tx),
            TypedTransaction::Eip2930(tx) => TaikoTypedTransaction::Eip2930(tx),
            TypedTransaction::Eip1559(tx) => TaikoTypedTransaction::Eip1559(tx),
            TypedTransaction::Eip7702(tx) => TaikoTypedTransaction::Eip7702(tx),
            TypedTransaction::Eip4844(_) => {
                return Err(SignTxRequestError::InvalidTransactionRequest);
            }
        };

        // Sign the transaction
        let signature = signer.sign_transaction(&mut taiko_typed_tx).await?;

        // Convert to TaikoTxEnvelope
        let envelope = TaikoTxEnvelope::from((taiko_typed_tx, signature));

        Ok(envelope)
    }
}

/// Implementation of simulation transaction conversion for Taiko.
///
/// This allows `TransactionRequest` to be converted into `TaikoTxEnvelope` for simulation
/// purposes (eth_call, eth_estimateGas, etc.) using the standard Reth trait.
impl reth_rpc_convert::transaction::TryIntoSimTx<TaikoTxEnvelope> for TransactionRequest {
    fn try_into_sim_tx(self) -> Result<TaikoTxEnvelope, alloy_consensus::error::ValueError<Self>> {
        use alloy_primitives::TxKind;

        let signature = Signature::test_signature();

        // Determine transaction type and create appropriate envelope
        let envelope = match self.transaction_type {
            Some(ty) if ty == TaikoTxType::Legacy as u8 => {
                // Legacy transaction (type 0)
                let legacy_tx = TxLegacy {
                    chain_id: self.chain_id,
                    nonce: self.nonce.unwrap_or_default(),
                    gas_price: self.gas_price.unwrap_or_default(),
                    gas_limit: self.gas.unwrap_or_default(),
                    to: self.to.unwrap_or_default(),
                    value: self.value.unwrap_or_default(),
                    input: self.input.input().cloned().unwrap_or_default(),
                };
                let typed_tx = TaikoTypedTransaction::Legacy(legacy_tx);
                TaikoTxEnvelope::new_unhashed(typed_tx, signature)
            }
            Some(ty) if ty == TaikoTxType::Eip2930 as u8 => {
                // EIP-2930 (type 1)
                let eip2930_tx = TxEip2930 {
                    chain_id: self.chain_id.unwrap_or_default(),
                    nonce: self.nonce.unwrap_or_default(),
                    gas_price: self.gas_price.unwrap_or_default(),
                    gas_limit: self.gas.unwrap_or_default(),
                    to: self.to.unwrap_or_default(),
                    value: self.value.unwrap_or_default(),
                    input: self.input.input().cloned().unwrap_or_default(),
                    access_list: self.access_list.clone().unwrap_or_default(),
                };
                let typed_tx = TaikoTypedTransaction::Eip2930(eip2930_tx);
                TaikoTxEnvelope::new_unhashed(typed_tx, signature)
            }
            Some(ty) if ty == TaikoTxType::Eip1559 as u8 => {
                // EIP-1559 (type 2)
                let eip1559_tx = TxEip1559 {
                    chain_id: self.chain_id.unwrap_or_default(),
                    nonce: self.nonce.unwrap_or_default(),
                    max_fee_per_gas: self.max_fee_per_gas.unwrap_or_default(),
                    max_priority_fee_per_gas: self.max_priority_fee_per_gas.unwrap_or_default(),
                    gas_limit: self.gas.unwrap_or_default(),
                    to: self.to.unwrap_or_default(),
                    value: self.value.unwrap_or_default(),
                    input: self.input.input().cloned().unwrap_or_default(),
                    access_list: self.access_list.clone().unwrap_or_default(),
                };
                let typed_tx = TaikoTypedTransaction::Eip1559(eip1559_tx);
                TaikoTxEnvelope::new_unhashed(typed_tx, signature)
            }
            Some(ty) if ty == TaikoTxType::Eip7702 as u8 => {
                // EIP-7702 (type 4)
                let to_address = match self.to {
                    Some(TxKind::Call(addr)) => addr,
                    // For simulation, if no 'to' or if it's Create, default to zero address
                    _ => Address::ZERO,
                };
                let eip7702_tx = TxEip7702 {
                    chain_id: self.chain_id.unwrap_or_default(),
                    nonce: self.nonce.unwrap_or_default(),
                    max_fee_per_gas: self.max_fee_per_gas.unwrap_or_default(),
                    max_priority_fee_per_gas: self.max_priority_fee_per_gas.unwrap_or_default(),
                    gas_limit: self.gas.unwrap_or_default(),
                    to: to_address,
                    value: self.value.unwrap_or_default(),
                    input: self.input.input().cloned().unwrap_or_default(),
                    access_list: self.access_list.clone().unwrap_or_default(),
                    authorization_list: self.authorization_list.clone().unwrap_or_default(),
                };
                let typed_tx = TaikoTypedTransaction::Eip7702(eip7702_tx);
                TaikoTxEnvelope::new_unhashed(typed_tx, signature)
            }
            None | Some(_) => {
                // For None or any other type, default to EIP-1559
                let eip1559_tx = TxEip1559 {
                    chain_id: self.chain_id.unwrap_or_default(),
                    nonce: self.nonce.unwrap_or_default(),
                    max_fee_per_gas: self.max_fee_per_gas.unwrap_or_default(),
                    max_priority_fee_per_gas: self.max_priority_fee_per_gas.unwrap_or_default(),
                    gas_limit: self.gas.unwrap_or_default(),
                    to: self.to.unwrap_or_default(),
                    value: self.value.unwrap_or_default(),
                    input: self.input.input().cloned().unwrap_or_default(),
                    access_list: self.access_list.clone().unwrap_or_default(),
                };
                let typed_tx = TaikoTypedTransaction::Eip1559(eip1559_tx);
                TaikoTxEnvelope::new_unhashed(typed_tx, signature)
            }
        };

        Ok(envelope)
    }
}
