//! Error mapping helpers for Taiko `eth` namespace RPC responses.
use jsonrpsee_types::error::{ErrorCode, ErrorObjectOwned};
use reth_rpc_eth_types::EthApiError;
use tracing::error;

/// Errors that can occur when interacting with the `taiko_` namespace
#[derive(Debug, thiserror::Error)]
pub enum TaikoApiError {
    /// Requested entry was not found in Taiko storage.
    #[error("not found")]
    GethNotFound,
    /// Last-block lookup is ambiguous because no newer proposal confirms the head.
    #[error(
        "proposal last block uncertain: BatchToLastBlockID missing and no newer proposal observed"
    )]
    ProposalLastBlockUncertain,
    /// Last-block lookup exceeded its backward scan limit.
    #[error(
        "proposal last block lookback exceeded: BatchToLastBlockID missing and lookback limit reached"
    )]
    ProposalLastBlockLookbackExceeded,
}

impl From<TaikoApiError> for ErrorObjectOwned {
    /// Converts the TaikoApiError into the jsonrpsee ErrorObject.
    fn from(error: TaikoApiError) -> Self {
        let code = ErrorCode::ServerError(-32000).code();
        match error {
            TaikoApiError::GethNotFound => ErrorObjectOwned::owned(code, "not found", None::<()>),
            TaikoApiError::ProposalLastBlockUncertain => ErrorObjectOwned::owned(
                code,
                "proposal last block uncertain: BatchToLastBlockID missing and no newer proposal observed",
                None::<()>,
            ),
            TaikoApiError::ProposalLastBlockLookbackExceeded => ErrorObjectOwned::owned(
                code,
                "proposal last block lookback exceeded: BatchToLastBlockID missing and lookback limit reached",
                None::<()>,
            ),
        }
    }
}

/// Logs the error internally and returns a generic internal error for public RPC responses.
/// This prevents leaking sensitive information (paths, internal state) to API consumers.
pub fn internal_eth_error<E>(error: E) -> EthApiError
where
    E: std::fmt::Debug,
{
    error!(?error, "internal RPC error");
    EthApiError::InternalEthError
}
