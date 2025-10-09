use std::sync::Arc;

use alloy_consensus::{
    BlockBody, EMPTY_OMMER_ROOT_HASH, Header, TxReceipt, constants::EMPTY_WITHDRAWALS, proofs,
};
use alloy_eips::merge::BEACON_NONCE;
use alloy_primitives::logs_bloom;
use alloy_rpc_types_eth::Withdrawals;
use reth::primitives::Block;
use reth_ethereum::{Receipt, TransactionSigned};
use reth_evm::{
    block::{BlockExecutionError, BlockExecutorFactory},
    execute::{BlockAssembler, BlockAssemblerInput},
};
use reth_evm_ethereum::EthBlockAssembler;
use reth_provider::BlockExecutionResult;

use crate::factory::TaikoBlockExecutionCtx;
use alethia_reth_chainspec::spec::TaikoChainSpec;

/// A block assembler for the Taiko network that implements the `BlockAssembler` trait.
#[derive(Clone, Debug)]
pub struct TaikoBlockAssembler {
    block_assembler: EthBlockAssembler<TaikoChainSpec>,
}

impl TaikoBlockAssembler {
    /// Creates a new instance of the [`TaikoBlockAssembler`] with the given chain specification.
    pub fn new(chain_spec: Arc<TaikoChainSpec>) -> Self {
        Self { block_assembler: EthBlockAssembler::new(chain_spec) }
    }

    /// Returns a reference to the chain specification.
    pub fn chain_spec(&self) -> Arc<TaikoChainSpec> {
        self.block_assembler.chain_spec.clone()
    }
}

impl<F> BlockAssembler<F> for TaikoBlockAssembler
where
    F: for<'a> BlockExecutorFactory<
            ExecutionCtx<'a> = TaikoBlockExecutionCtx<'a>,
            Transaction = TransactionSigned,
            Receipt = Receipt,
        >,
{
    /// The block type produced by the assembler.
    type Block = Block;

    /// Builds a Taiko network block.
    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, F>,
    ) -> Result<Self::Block, BlockExecutionError> {
        let BlockAssemblerInput {
            evm_env,
            execution_ctx: ctx,
            parent: _,
            transactions,
            output: BlockExecutionResult { receipts, requests: _, gas_used },
            state_root,
            ..
        } = input;

        let timestamp = evm_env.block_env.timestamp;

        let transactions_root = proofs::calculate_transaction_root(&transactions);
        let receipts_root = Receipt::calculate_receipt_root_no_memo(receipts);
        let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| r.logs()));

        let withdrawals = Some(Withdrawals::default());
        let withdrawals_root = Some(EMPTY_WITHDRAWALS);

        let header = Header {
            parent_hash: ctx.parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: evm_env.block_env.beneficiary,
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root,
            logs_bloom,
            timestamp: timestamp.to(),
            mix_hash: evm_env.block_env.prevrandao.unwrap_or_default(),
            nonce: BEACON_NONCE.into(),
            base_fee_per_gas: Some(evm_env.block_env.basefee),
            number: evm_env.block_env.number.to(),
            gas_limit: evm_env.block_env.gas_limit,
            difficulty: evm_env.block_env.difficulty,
            gas_used: *gas_used,
            extra_data: ctx.extra_data,
            parent_beacon_block_root: ctx.parent_beacon_block_root,
            blob_gas_used: None,
            excess_blob_gas: None,
            requests_hash: None,
        };

        Ok(Block {
            header,
            body: BlockBody { transactions, ommers: Default::default(), withdrawals },
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_get_chain_spec() {
        let chain_spec = Arc::new(TaikoChainSpec::default());
        let assembler = TaikoBlockAssembler::new(chain_spec.clone());

        assert_eq!(assembler.chain_spec(), chain_spec);
    }
}
