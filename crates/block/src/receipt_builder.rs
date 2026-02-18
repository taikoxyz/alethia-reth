use alethia_reth_consensus::transaction::{TaikoTxEnvelope, TaikoTxType};
use alloy_evm::eth::receipt_builder::{ReceiptBuilder, ReceiptBuilderCtx};
use reth_ethereum::Receipt;
use reth_evm::Evm;

/// A builder that operates on Taiko primitive types, specifically [`TaikoTxEnvelope`] and
/// [`Receipt`].
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct TaikoReceiptBuilder;

impl ReceiptBuilder for TaikoReceiptBuilder {
    type Transaction = TaikoTxEnvelope;
    type Receipt = Receipt;

    fn build_receipt<E: Evm>(
        &self,
        ctx: ReceiptBuilderCtx<'_, TaikoTxType, E>,
    ) -> Self::Receipt {
        let ReceiptBuilderCtx { tx_type, result, cumulative_gas_used, .. } = ctx;
        Receipt {
            tx_type: tx_type.into(),
            // Success flag was added in `EIP-658: Embedding transaction status code in
            // receipts`.
            success: result.is_success(),
            cumulative_gas_used,
            logs: result.into_logs(),
        }
    }
}
