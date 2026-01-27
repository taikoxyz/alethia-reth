use jsonrpsee_types::error::{ErrorCode, ErrorObjectOwned};

/// Errors that can occur when interacting with the `taiko_` namespace
#[derive(Debug, thiserror::Error)]
pub enum TaikoApiError {
    #[error("not found")]
    GethNotFound,
    #[error(
        "proposal last block uncertain: BatchToLastBlockID missing and no newer proposal observed"
    )]
    ProposalLastBlockUncertain,
}

impl From<TaikoApiError> for ErrorObjectOwned {
    /// Converts the TaikoApiError into the jsonrpsee ErrorObject.
    fn from(error: TaikoApiError) -> Self {
        match error {
            TaikoApiError::GethNotFound => ErrorObjectOwned::owned(
                ErrorCode::ServerError(-32004).code(),
                "not found",
                None::<()>,
            ),
            TaikoApiError::ProposalLastBlockUncertain => ErrorObjectOwned::owned(
                ErrorCode::ServerError(-32005).code(),
                "proposal last block uncertain: BatchToLastBlockID missing and no newer proposal observed",
                None::<()>,
            ),
        }
    }
}
