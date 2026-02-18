//! Taiko-specific RPC type conversions

use alethia_reth_chainspec::spec::TaikoChainSpec;
use alloy_network::Ethereum;
use reth_rpc_convert::RpcConverter;
use reth_rpc_eth_types::receipt::EthReceiptConverter;

/// RPC converter type alias for Taiko with the given EVM config.
///
/// Uses the default `()` SimTxConverter which delegates to `TryIntoSimTx` trait.
/// The `TransactionRequest -> TaikoTxEnvelope` conversion is implemented via `TryIntoSimTx`
/// in the consensus crate for simulation purposes (eth_call, eth_estimateGas).
pub type TaikoRpcConverter<Evm> = RpcConverter<
    Ethereum,
    Evm,
    EthReceiptConverter<TaikoChainSpec>,
    (),
    (),
    (), // SimTxConverter - uses TryIntoSimTx trait (implemented in consensus crate)
    (),
    (),
>;
