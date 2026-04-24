//! Taiko `eth` and `taikoAuth` namespace RPC extensions.
/// Authenticated Taiko RPC methods and tx-pool helpers.
pub mod auth;
/// Builder for Taiko `eth` API integration.
pub mod builder;
/// Error types and helpers for Taiko `eth` RPC methods.
pub mod error;
#[allow(clippy::module_inception)]
/// Public Taiko `eth` namespace methods.
pub mod eth;
/// Proof-history backed `eth_getProof` override.
pub mod proofs;
/// Aliases to `reth` Eth API types used by Taiko node wiring.
pub mod types;

#[cfg(test)]
mod tests {
    use alloy_eips::BlockId;
    use alloy_primitives::Address;
    use alloy_rpc_types_eth::EIP1186AccountProofResponse;
    use alloy_serde::JsonStorageKey;
    use async_trait::async_trait;
    use jsonrpsee::core::RpcResult;

    use super::proofs::TaikoEthProofApiServer;

    struct EmptyProofApi;

    #[async_trait]
    impl TaikoEthProofApiServer for EmptyProofApi {
        async fn get_proof(
            &self,
            _address: Address,
            _keys: Vec<JsonStorageKey>,
            _block_id: Option<BlockId>,
        ) -> RpcResult<EIP1186AccountProofResponse> {
            unreachable!("method registration test does not execute the handler")
        }
    }

    #[test]
    fn proof_history_eth_api_registers_get_proof_method() {
        let module = EmptyProofApi.into_rpc();

        assert!(module.method_names().any(|method| method == "eth_getProof"));
    }
}
