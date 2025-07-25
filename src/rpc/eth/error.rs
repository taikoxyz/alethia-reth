use alloy_rpc_types_eth::error::EthRpcErrorCode;
use reth_rpc_server_types::result::rpc_error_with_code;

/// Errors that can occur when interacting with the `taiko_` namespace
#[derive(Debug, thiserror::Error)]
pub enum TaikoApiError {
    #[error("not found")]
    GethNotFound,
}

impl From<TaikoApiError> for jsonrpsee_types::error::ErrorObject<'static> {
    /// Converts the TaikoApiError into the jsonrpsee ErrorObject.
    fn from(error: TaikoApiError) -> Self {
        match error {
            TaikoApiError::GethNotFound => rpc_error_with_code(
                EthRpcErrorCode::ResourceNotFound.code(),
                "not found".to_string(),
            ),
        }
    }
}
