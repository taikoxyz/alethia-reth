use alloy_evm::InvalidTxError;
use core::fmt;
use reth_revm::context::result::InvalidTransaction;

/// Structured invalid-transaction errors specific to Taiko execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaikoInvalidTxError {
    /// Wraps a standard revm invalid transaction error.
    Revm(InvalidTransaction),
    /// Reports that the transaction exceeded the zk gas limit.
    ZkTxGasLimitExceeded {
        /// The configured zk gas limit for the transaction.
        limit: u64,
        /// The zk gas used when the limit was exceeded.
        used: u64,
    },
    /// Reports that zk gas accounting overflowed during a transaction.
    ZkTxGasOverflow {
        /// The zk gas used before overflow was detected.
        used: u64,
    },
    /// Reports that adding the transaction would exceed the block zk gas limit.
    ZkBlockGasLimitExceeded {
        /// The configured zk gas limit for the block.
        limit: u64,
        /// The zk gas that would have been used after adding the tx.
        used: u64,
    },
    /// Reports that block-level zk gas accounting overflowed.
    ZkBlockGasOverflow {
        /// The zk gas used before overflow was detected.
        used: u64,
    },
}

impl fmt::Display for TaikoInvalidTxError {
    /// Formats the error with a human-readable message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revm(err) => write!(f, "{err}"),
            Self::ZkTxGasLimitExceeded { limit, used } => {
                write!(f, "zk tx gas limit exceeded: used {used} > limit {limit}")
            }
            Self::ZkTxGasOverflow { used } => {
                write!(f, "zk tx gas overflow after {used}")
            }
            Self::ZkBlockGasLimitExceeded { limit, used } => {
                write!(f, "zk block gas limit exceeded: used {used} > limit {limit}")
            }
            Self::ZkBlockGasOverflow { used } => {
                write!(f, "zk block gas overflow after {used}")
            }
        }
    }
}

impl std::error::Error for TaikoInvalidTxError {}

impl InvalidTxError for TaikoInvalidTxError {
    /// Returns true if the underlying error is a nonce-too-low error.
    fn is_nonce_too_low(&self) -> bool {
        matches!(self, Self::Revm(err) if err.is_nonce_too_low())
    }

    /// Returns the underlying revm invalid transaction error, if available.
    fn as_invalid_tx_err(&self) -> Option<&InvalidTransaction> {
        match self {
            Self::Revm(err) => Some(err),
            _ => None,
        }
    }
}

impl From<InvalidTransaction> for TaikoInvalidTxError {
    /// Wraps a revm invalid transaction error in a Taiko invalid tx error.
    fn from(value: InvalidTransaction) -> Self {
        Self::Revm(value)
    }
}
