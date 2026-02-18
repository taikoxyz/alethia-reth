//! Contains the transaction type identifier for Taiko.

use alloy_consensus::TxType;

use crate::transaction::envelope::TaikoTxType;
use core::fmt::Display;

#[allow(clippy::derivable_impls)]
impl Default for TaikoTxType {
    fn default() -> Self {
        Self::Legacy
    }
}

impl Display for TaikoTxType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Legacy => write!(f, "legacy"),
            Self::Eip2930 => write!(f, "eip2930"),
            Self::Eip1559 => write!(f, "eip1559"),
            Self::Eip7702 => write!(f, "eip7702"),
        }
    }
}

impl TaikoTxType {
    /// List of all variants.
    pub const ALL: [Self; 4] = [Self::Legacy, Self::Eip2930, Self::Eip1559, Self::Eip7702];
}

impl From<TaikoTxType> for TxType {
    fn from(value: TaikoTxType) -> Self {
        match value {
            TaikoTxType::Legacy => TxType::Legacy,
            TaikoTxType::Eip2930 => TxType::Eip2930,
            TaikoTxType::Eip1559 => TxType::Eip1559,
            TaikoTxType::Eip7702 => TxType::Eip7702,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rlp::{Decodable, Encodable};

    #[test]
    fn test_all_tx_types() {
        assert_eq!(TaikoTxType::ALL.len(), 4);
        let all = vec![
            TaikoTxType::Legacy,
            TaikoTxType::Eip2930,
            TaikoTxType::Eip1559,
            TaikoTxType::Eip7702,
        ];
        assert_eq!(TaikoTxType::ALL.to_vec(), all);
    }

    #[test]
    fn tx_type_roundtrip() {
        for &tx_type in &TaikoTxType::ALL {
            let mut buf = Vec::new();
            tx_type.encode(&mut buf);
            let decoded = TaikoTxType::decode(&mut &buf[..]).unwrap();
            assert_eq!(tx_type, decoded);
        }
    }
}
