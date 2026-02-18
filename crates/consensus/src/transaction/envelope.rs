use crate::transaction::{TaikoPooledTransaction, TaikoTypedTransaction};
use alloy_consensus::{
    EthereumTxEnvelope, Extended, SignableTransaction, Signed, TransactionEnvelope, TxEip1559,
    TxEip2930, TxEip7702, TxEnvelope, TxLegacy, error::ValueError, transaction::TxHashRef,
};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Signature};
use reth_evm::{FromRecoveredTx, FromTxWithEncoded, revm::context::TxEnv};
use reth_primitives_traits::InMemorySize;

/// The Ethereum [EIP-2718] Transaction Envelope, modified for Taiko chain.
///
/// # Note:
///
/// This enum distinguishes between tagged and untagged legacy transactions, as
/// the in-protocol merkle tree may commit to EITHER 0-prefixed or raw.
/// Therefore we must ensure that encoding returns the precise byte-array that
/// was decoded, preserving the presence or absence of the `TransactionType`
/// flag.
///
/// [EIP-2718]: https://eips.ethereum.org/EIPS/eip-2718
#[derive(Debug, Clone, TransactionEnvelope)]
#[envelope(tx_type_name = TaikoTxType, serde_cfg(feature = "serde"))]
pub enum TaikoTxEnvelope {
    /// An untagged [`TxLegacy`].
    #[envelope(ty = 0)]
    Legacy(Signed<TxLegacy>),
    /// A [`TxEip2930`] tagged with type 1.
    #[envelope(ty = 1)]
    Eip2930(Signed<TxEip2930>),
    /// A [`TxEip1559`] tagged with type 2.
    #[envelope(ty = 2)]
    Eip1559(Signed<TxEip1559>),
    /// A [`TxEip7702`] tagged with type 4.
    #[envelope(ty = 4)]
    Eip7702(Signed<TxEip7702>),
}

impl AsRef<Self> for TaikoTxEnvelope {
    fn as_ref(&self) -> &Self {
        self
    }
}

impl From<Signed<TxLegacy>> for TaikoTxEnvelope {
    fn from(v: Signed<TxLegacy>) -> Self {
        Self::Legacy(v)
    }
}

impl From<Signed<TxEip2930>> for TaikoTxEnvelope {
    fn from(v: Signed<TxEip2930>) -> Self {
        Self::Eip2930(v)
    }
}

impl From<Signed<TxEip1559>> for TaikoTxEnvelope {
    fn from(v: Signed<TxEip1559>) -> Self {
        Self::Eip1559(v)
    }
}

impl From<Signed<TxEip7702>> for TaikoTxEnvelope {
    fn from(v: Signed<TxEip7702>) -> Self {
        Self::Eip7702(v)
    }
}

impl From<Signed<TaikoTypedTransaction>> for TaikoTxEnvelope {
    fn from(value: Signed<TaikoTypedTransaction>) -> Self {
        let (tx, sig, hash) = value.into_parts();
        match tx {
            TaikoTypedTransaction::Legacy(tx_legacy) => {
                let tx = Signed::new_unchecked(tx_legacy, sig, hash);
                Self::Legacy(tx)
            }
            TaikoTypedTransaction::Eip2930(tx_eip2930) => {
                let tx = Signed::new_unchecked(tx_eip2930, sig, hash);
                Self::Eip2930(tx)
            }
            TaikoTypedTransaction::Eip1559(tx_eip1559) => {
                let tx = Signed::new_unchecked(tx_eip1559, sig, hash);
                Self::Eip1559(tx)
            }
            TaikoTypedTransaction::Eip7702(tx_eip7702) => {
                let tx = Signed::new_unchecked(tx_eip7702, sig, hash);
                Self::Eip7702(tx)
            }
        }
    }
}

impl From<(TaikoTypedTransaction, Signature)> for TaikoTxEnvelope {
    fn from(value: (TaikoTypedTransaction, Signature)) -> Self {
        Self::new_unhashed(value.0, value.1)
    }
}

impl<Tx> From<TaikoTxEnvelope> for Extended<TaikoTxEnvelope, Tx> {
    fn from(value: TaikoTxEnvelope) -> Self {
        Self::BuiltIn(value)
    }
}

impl<T> From<TaikoTxEnvelope> for EthereumTxEnvelope<T> {
    fn from(envelope: TaikoTxEnvelope) -> Self {
        // Convert your Taiko envelope to Ethereum envelope
        // This depends on your TaikoTxEnvelope structure
        match envelope {
            TaikoTxEnvelope::Legacy(tx) => EthereumTxEnvelope::Legacy(tx),
            TaikoTxEnvelope::Eip2930(tx) => EthereumTxEnvelope::Eip2930(tx),
            TaikoTxEnvelope::Eip1559(tx) => EthereumTxEnvelope::Eip1559(tx),
            TaikoTxEnvelope::Eip7702(tx) => EthereumTxEnvelope::Eip7702(tx),
        }
    }
}

impl<T> TryFrom<EthereumTxEnvelope<T>> for TaikoTxEnvelope {
    type Error = EthereumTxEnvelope<T>;

    fn try_from(value: EthereumTxEnvelope<T>) -> Result<Self, Self::Error> {
        Self::try_from_eth_envelope(value)
    }
}

impl TaikoTxEnvelope {
    /// Creates a new enveloped transaction from the given transaction, signature and hash.
    ///
    /// Caution: This assumes the given hash is the correct transaction hash.
    pub fn new_unchecked(
        transaction: TaikoTypedTransaction,
        signature: Signature,
        hash: B256,
    ) -> Self {
        Signed::new_unchecked(transaction, signature, hash).into()
    }

    /// Creates a new signed transaction from the given typed transaction and signature without the
    /// hash.
    ///
    /// Note: this only calculates the hash on the first [`TaikoTxEnvelope::hash`] call.
    pub fn new_unhashed(transaction: TaikoTypedTransaction, signature: Signature) -> Self {
        transaction.into_signed(signature).into()
    }

    /// Returns true if the transaction is a legacy transaction.
    #[inline]
    pub const fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy(_))
    }

    /// Returns true if the transaction is an EIP-2930 transaction.
    #[inline]
    pub const fn is_eip2930(&self) -> bool {
        matches!(self, Self::Eip2930(_))
    }

    /// Returns true if the transaction is an EIP-1559 transaction.
    #[inline]
    pub const fn is_eip1559(&self) -> bool {
        matches!(self, Self::Eip1559(_))
    }

    /// Returns true if the transaction is a system transaction.
    #[inline]
    pub const fn is_system_transaction(&self) -> bool {
        false
    }

    /// Attempts to convert the envelope into the pooled variant.
    pub fn try_into_pooled(self) -> Result<TaikoPooledTransaction, ValueError<Self>> {
        match self {
            Self::Legacy(tx) => Ok(tx.into()),
            Self::Eip2930(tx) => Ok(tx.into()),
            Self::Eip1559(tx) => Ok(tx.into()),
            Self::Eip7702(tx) => Ok(tx.into()),
        }
    }

    /// Attempts to convert the envelope into the ethereum pooled variant.
    pub fn try_into_eth_pooled(
        self,
    ) -> Result<alloy_consensus::transaction::PooledTransaction, ValueError<Self>> {
        self.try_into_pooled().map(Into::into)
    }

    /// Attempts to convert the taiko variant into an ethereum [`TxEnvelope`].
    pub fn try_into_eth_envelope(self) -> Result<TxEnvelope, ValueError<Self>> {
        match self {
            Self::Legacy(tx) => Ok(tx.into()),
            Self::Eip2930(tx) => Ok(tx.into()),
            Self::Eip1559(tx) => Ok(tx.into()),
            Self::Eip7702(tx) => Ok(tx.into()),
        }
    }

    /// Attempts to convert an ethereum [`TxEnvelope`] into the taiko variant.
    ///
    /// Returns the given envelope as error if [`TaikoTxEnvelope`] doesn't support the variant
    /// (EIP-4844)
    #[allow(clippy::result_large_err)]
    pub fn try_from_eth_envelope<T>(
        tx: EthereumTxEnvelope<T>,
    ) -> Result<Self, EthereumTxEnvelope<T>> {
        match tx {
            EthereumTxEnvelope::Legacy(tx) => Ok(tx.into()),
            EthereumTxEnvelope::Eip2930(tx) => Ok(tx.into()),
            EthereumTxEnvelope::Eip1559(tx) => Ok(tx.into()),
            tx @ EthereumTxEnvelope::<T>::Eip4844(_) => Err(tx),
            EthereumTxEnvelope::Eip7702(tx) => Ok(tx.into()),
        }
    }

    /// Returns mutable access to the input bytes.
    ///
    /// Caution: modifying this will cause side-effects on the hash.
    #[doc(hidden)]
    pub const fn input_mut(&mut self) -> &mut Bytes {
        match self {
            Self::Eip1559(tx) => &mut tx.tx_mut().input,
            Self::Eip2930(tx) => &mut tx.tx_mut().input,
            Self::Legacy(tx) => &mut tx.tx_mut().input,
            Self::Eip7702(tx) => &mut tx.tx_mut().input,
        }
    }

    /// Returns the [`TxLegacy`] variant if the transaction is a legacy transaction.
    pub const fn as_legacy(&self) -> Option<&Signed<TxLegacy>> {
        match self {
            Self::Legacy(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxEip2930`] variant if the transaction is an EIP-2930 transaction.
    pub const fn as_eip2930(&self) -> Option<&Signed<TxEip2930>> {
        match self {
            Self::Eip2930(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxEip1559`] variant if the transaction is an EIP-1559 transaction.
    pub const fn as_eip1559(&self) -> Option<&Signed<TxEip1559>> {
        match self {
            Self::Eip1559(tx) => Some(tx),
            _ => None,
        }
    }

    /// Return the reference to signature.
    ///
    /// Returns `None` if this is a deposit variant.
    pub const fn signature(&self) -> Option<&Signature> {
        match self {
            Self::Legacy(tx) => Some(tx.signature()),
            Self::Eip2930(tx) => Some(tx.signature()),
            Self::Eip1559(tx) => Some(tx.signature()),
            Self::Eip7702(tx) => Some(tx.signature()),
        }
    }

    /// Return the [`OpTxType`] of the inner txn.
    pub const fn tx_type(&self) -> TaikoTxType {
        match self {
            Self::Legacy(_) => TaikoTxType::Legacy,
            Self::Eip2930(_) => TaikoTxType::Eip2930,
            Self::Eip1559(_) => TaikoTxType::Eip1559,
            Self::Eip7702(_) => TaikoTxType::Eip7702,
        }
    }

    /// Returns the inner transaction hash.
    pub fn hash(&self) -> &B256 {
        match self {
            Self::Legacy(tx) => tx.hash(),
            Self::Eip1559(tx) => tx.hash(),
            Self::Eip2930(tx) => tx.hash(),
            Self::Eip7702(tx) => tx.hash(),
        }
    }

    /// Returns the inner transaction hash.
    pub fn tx_hash(&self) -> B256 {
        *self.hash()
    }

    /// Returns the signing hash for the transaction, used for signature verification.
    ///
    /// This delegates to the inner signed transaction's [`SignableTransaction::signature_hash`].
    pub fn signature_hash(&self) -> B256 {
        match self {
            Self::Legacy(tx) => tx.signature_hash(),
            Self::Eip2930(tx) => tx.signature_hash(),
            Self::Eip1559(tx) => tx.signature_hash(),
            Self::Eip7702(tx) => tx.signature_hash(),
        }
    }

    /// Return the length of the inner txn, including type byte length
    pub fn eip2718_encoded_length(&self) -> usize {
        match self {
            Self::Legacy(t) => t.eip2718_encoded_length(),
            Self::Eip2930(t) => t.eip2718_encoded_length(),
            Self::Eip1559(t) => t.eip2718_encoded_length(),
            Self::Eip7702(t) => t.eip2718_encoded_length(),
        }
    }
}

impl TxHashRef for TaikoTxEnvelope {
    fn tx_hash(&self) -> &B256 {
        Self::hash(self)
    }
}

#[cfg(feature = "k256")]
impl alloy_consensus::transaction::SignerRecoverable for TaikoTxEnvelope {
    fn recover_signer(
        &self,
    ) -> Result<alloy_primitives::Address, alloy_consensus::crypto::RecoveryError> {
        let signature_hash = match self {
            Self::Legacy(tx) => tx.signature_hash(),
            Self::Eip2930(tx) => tx.signature_hash(),
            Self::Eip1559(tx) => tx.signature_hash(),
            Self::Eip7702(tx) => tx.signature_hash(),
        };
        let signature = match self {
            Self::Legacy(tx) => tx.signature(),
            Self::Eip2930(tx) => tx.signature(),
            Self::Eip1559(tx) => tx.signature(),
            Self::Eip7702(tx) => tx.signature(),
        };
        alloy_consensus::crypto::secp256k1::recover_signer(signature, signature_hash)
    }

    fn recover_signer_unchecked(
        &self,
    ) -> Result<alloy_primitives::Address, alloy_consensus::crypto::RecoveryError> {
        let signature_hash = match self {
            Self::Legacy(tx) => tx.signature_hash(),
            Self::Eip2930(tx) => tx.signature_hash(),
            Self::Eip1559(tx) => tx.signature_hash(),
            Self::Eip7702(tx) => tx.signature_hash(),
        };
        let signature = match self {
            Self::Legacy(tx) => tx.signature(),
            Self::Eip2930(tx) => tx.signature(),
            Self::Eip1559(tx) => tx.signature(),
            Self::Eip7702(tx) => tx.signature(),
        };
        alloy_consensus::crypto::secp256k1::recover_signer_unchecked(signature, signature_hash)
    }

    fn recover_unchecked_with_buf(
        &self,
        buf: &mut Vec<u8>,
    ) -> Result<alloy_primitives::Address, alloy_consensus::crypto::RecoveryError> {
        match self {
            Self::Legacy(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_unchecked_with_buf(tx, buf)
            }
            Self::Eip2930(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_unchecked_with_buf(tx, buf)
            }
            Self::Eip1559(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_unchecked_with_buf(tx, buf)
            }
            Self::Eip7702(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_unchecked_with_buf(tx, buf)
            }
        }
    }
}

impl InMemorySize for TaikoTxEnvelope {
    fn size(&self) -> usize {
        match self {
            Self::Legacy(tx) => tx.size(),
            Self::Eip2930(tx) => tx.size(),
            Self::Eip1559(tx) => tx.size(),
            Self::Eip7702(tx) => tx.size(),
        }
    }
}

#[cfg(feature = "k256")]
impl reth_primitives_traits::SignedTransaction for TaikoTxEnvelope {}

/// Bincode-compatible serde implementation for TaikoTxEnvelope.
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub mod serde_bincode_compat {
    use alloy_consensus::{
        Signed,
        transaction::serde_bincode_compat::{TxEip1559, TxEip2930, TxEip7702, TxLegacy},
    };
    use alloy_primitives::Signature;
    use reth_primitives_traits::serde_bincode_compat::SerdeBincodeCompat;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_with::{DeserializeAs, SerializeAs};

    /// Bincode-compatible representation of an TaikoTxEnvelope.
    #[derive(Debug, Serialize, Deserialize)]
    pub enum TaikoTxEnvelope<'a> {
        /// Legacy variant.
        Legacy {
            /// Transaction signature.
            signature: Signature,
            /// Borrowed legacy transaction data.
            transaction: TxLegacy<'a>,
        },
        /// EIP-2930 variant.
        Eip2930 {
            /// Transaction signature.
            signature: Signature,
            /// Borrowed EIP-2930 transaction data.
            transaction: TxEip2930<'a>,
        },
        /// EIP-1559 variant.
        Eip1559 {
            /// Transaction signature.
            signature: Signature,
            /// Borrowed EIP-1559 transaction data.
            transaction: TxEip1559<'a>,
        },
        /// EIP-7702 variant.
        Eip7702 {
            /// Transaction signature.
            signature: Signature,
            /// Borrowed EIP-7702 transaction data.
            transaction: TxEip7702<'a>,
        },
    }

    impl<'a> From<&'a super::TaikoTxEnvelope> for TaikoTxEnvelope<'a> {
        fn from(value: &'a super::TaikoTxEnvelope) -> Self {
            match value {
                super::TaikoTxEnvelope::Legacy(signed_legacy) => Self::Legacy {
                    signature: *signed_legacy.signature(),
                    transaction: signed_legacy.tx().into(),
                },
                super::TaikoTxEnvelope::Eip2930(signed_2930) => Self::Eip2930 {
                    signature: *signed_2930.signature(),
                    transaction: signed_2930.tx().into(),
                },
                super::TaikoTxEnvelope::Eip1559(signed_1559) => Self::Eip1559 {
                    signature: *signed_1559.signature(),
                    transaction: signed_1559.tx().into(),
                },
                super::TaikoTxEnvelope::Eip7702(signed_7702) => Self::Eip7702 {
                    signature: *signed_7702.signature(),
                    transaction: signed_7702.tx().into(),
                },
            }
        }
    }

    impl<'a> From<TaikoTxEnvelope<'a>> for super::TaikoTxEnvelope {
        fn from(value: TaikoTxEnvelope<'a>) -> Self {
            match value {
                TaikoTxEnvelope::Legacy { signature, transaction } => {
                    Self::Legacy(Signed::new_unhashed(transaction.into(), signature))
                }
                TaikoTxEnvelope::Eip2930 { signature, transaction } => {
                    Self::Eip2930(Signed::new_unhashed(transaction.into(), signature))
                }
                TaikoTxEnvelope::Eip1559 { signature, transaction } => {
                    Self::Eip1559(Signed::new_unhashed(transaction.into(), signature))
                }
                TaikoTxEnvelope::Eip7702 { signature, transaction } => {
                    Self::Eip7702(Signed::new_unhashed(transaction.into(), signature))
                }
            }
        }
    }

    impl SerializeAs<super::TaikoTxEnvelope> for TaikoTxEnvelope<'_> {
        fn serialize_as<S>(
            source: &super::TaikoTxEnvelope,
            serializer: S,
        ) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let borrowed = TaikoTxEnvelope::from(source);
            borrowed.serialize(serializer)
        }
    }

    impl<'de> DeserializeAs<'de, super::TaikoTxEnvelope> for TaikoTxEnvelope<'de> {
        fn deserialize_as<D>(deserializer: D) -> Result<super::TaikoTxEnvelope, D::Error>
        where
            D: Deserializer<'de>,
        {
            let borrowed = TaikoTxEnvelope::deserialize(deserializer)?;
            Ok(borrowed.into())
        }
    }

    impl SerdeBincodeCompat for super::TaikoTxEnvelope {
        type BincodeRepr<'a> = TaikoTxEnvelope<'a>;

        fn as_repr(&self) -> Self::BincodeRepr<'_> {
            self.into()
        }

        fn from_repr(repr: Self::BincodeRepr<'_>) -> Self {
            repr.into()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use arbitrary::Arbitrary;
        use rand::Rng;
        use serde::{Deserialize, Serialize};
        use serde_with::serde_as;

        /// Tests a bincode round-trip for TaikoTxEnvelope using an arbitrary instance.
        #[test]
        fn test_taiko_tx_envelope_bincode_roundtrip_arbitrary() {
            #[serde_as]
            #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
            struct Data {
                // Use the bincode-compatible representation defined in this module.
                #[serde_as(as = "TaikoTxEnvelope<'_>")]
                envelope: super::super::TaikoTxEnvelope,
            }

            let mut bytes = [0u8; 1024];
            rand::rng().fill(bytes.as_mut_slice());
            let data = Data {
                envelope: super::super::TaikoTxEnvelope::arbitrary(
                    &mut arbitrary::Unstructured::new(&bytes),
                )
                .unwrap(),
            };

            let encoded = bincode::serde::encode_to_vec(&data, bincode::config::legacy()).unwrap();
            let (decoded, _) =
                bincode::serde::decode_from_slice::<Data, _>(&encoded, bincode::config::legacy())
                    .unwrap();
            assert_eq!(decoded, data);
        }
    }
}

impl FromRecoveredTx<TaikoTxEnvelope> for TxEnv {
    fn from_recovered_tx(tx: &TaikoTxEnvelope, caller: Address) -> Self {
        match tx {
            TaikoTxEnvelope::Legacy(tx) => TxEnv::from_recovered_tx(tx.tx(), caller),
            TaikoTxEnvelope::Eip1559(tx) => TxEnv::from_recovered_tx(tx.tx(), caller),
            TaikoTxEnvelope::Eip2930(tx) => TxEnv::from_recovered_tx(tx.tx(), caller),
            TaikoTxEnvelope::Eip7702(tx) => TxEnv::from_recovered_tx(tx.tx(), caller),
        }
    }
}

impl FromTxWithEncoded<TaikoTxEnvelope> for TxEnv {
    fn from_encoded_tx(tx: &TaikoTxEnvelope, sender: Address, _encoded: Bytes) -> Self {
        TxEnv::from_recovered_tx(tx, sender)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::SignableTransaction;
    use alloy_primitives::Signature;

    fn create_test_eip1559_enveloped_tx() -> TaikoTxEnvelope {
        use arbitrary::Arbitrary;
        use rand::Rng;

        let mut bytes = [0u8; 1024];
        rand::rng().fill(&mut bytes);

        let tx = TxEip1559::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap();
        let sig = Signature::test_signature();
        TaikoTxEnvelope::Eip1559(tx.into_signed(sig))
    }

    #[test]
    fn test_encode_decode_eip1559_arbitrary() {
        let tx_envelope = create_test_eip1559_enveloped_tx();

        let encoded = tx_envelope.encoded_2718();
        let decoded = TaikoTxEnvelope::decode_2718(&mut encoded.as_ref()).unwrap();

        assert_eq!(encoded.len(), tx_envelope.encode_2718_len());
        assert_eq!(decoded, tx_envelope);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_serde_roundtrip_deposit() {
        let tx_envelope = create_test_eip1559_enveloped_tx();

        let serialized = serde_json::to_string(&tx_envelope).unwrap();
        let deserialized: TaikoTxEnvelope = serde_json::from_str(&serialized).unwrap();

        assert_eq!(tx_envelope, deserialized);
    }

    #[test]
    fn eip1559_decode() {
        let envelope = create_test_eip1559_enveloped_tx();

        let encoded = envelope.encoded_2718();
        let mut slice = encoded.as_slice();
        let decoded = TaikoTxEnvelope::decode_2718(&mut slice).unwrap();
        assert!(matches!(decoded, TaikoTxEnvelope::Eip1559(_)));
    }
}
