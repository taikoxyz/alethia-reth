//! Block assembler implementation for Taiko headers and block bodies.
use std::sync::Arc;

use alloy_consensus::{
    BlockBody, EMPTY_OMMER_ROOT_HASH, Header, TxReceipt, constants::EMPTY_WITHDRAWALS, proofs,
};
use alloy_eips::merge::BEACON_NONCE;
use alloy_primitives::{U256, logs_bloom};
use alloy_rpc_types_eth::Withdrawals;
use reth_ethereum::{Receipt, TransactionSigned};
use reth_evm::{
    block::{BlockExecutionError, BlockExecutorFactory},
    execute::{BlockAssembler, BlockAssemblerInput},
};
use reth_evm_ethereum::EthBlockAssembler;
use reth_execution_types::BlockExecutionResult;
use reth_primitives::Block;
use reth_revm::context::Block as _;

use crate::factory::TaikoBlockExecutionCtx;
use alethia_reth_chainspec::spec::TaikoChainSpec;

/// A block assembler for the Taiko network that implements the `BlockAssembler` trait.
#[derive(Clone, Debug)]
pub struct TaikoBlockAssembler {
    /// Underlying Ethereum block assembler configured with Taiko chain spec.
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
            output: BlockExecutionResult { receipts, requests: _, gas_used, blob_gas_used: _ },
            state_root,
            ..
        } = input;

        let block_env = &evm_env.block_env;
        let timestamp = block_env.timestamp();

        let transactions_root = proofs::calculate_transaction_root(&transactions);
        let receipts_root = Receipt::calculate_receipt_root_no_memo(receipts);
        let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| r.logs()));

        let withdrawals = Some(Withdrawals::default());
        let withdrawals_root = Some(EMPTY_WITHDRAWALS);
        let difficulty = if ctx.is_uzen_active {
            U256::from(ctx.finalized_block_zk_gas())
        } else {
            block_env.difficulty()
        };

        let header = Header {
            parent_hash: ctx.parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: block_env.beneficiary(),
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root,
            logs_bloom,
            timestamp: timestamp.to(),
            mix_hash: block_env.prevrandao().unwrap_or_default(),
            nonce: BEACON_NONCE.into(),
            base_fee_per_gas: Some(block_env.basefee()),
            number: block_env.number().to(),
            gas_limit: block_env.gas_limit(),
            difficulty,
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
    use alloy_consensus::{Header, Signed, TxLegacy};
    use alloy_eips::eip7685::Requests;
    use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U256};
    use reth_evm::{
        EvmEnv,
        execute::{BlockAssembler, BlockAssemblerInput},
    };
    use reth_execution_types::BlockExecutionResult;
    use reth_primitives::SealedHeader;
    use reth_revm::db::states::BundleState;
    use reth_storage_api::noop::NoopProvider;

    use super::*;
    use crate::factory::{TaikoBlockExecutionCtx, TaikoBlockExecutorFactory};
    use alethia_reth_evm::spec::TaikoSpecId;

    #[test]
    fn test_get_chain_spec() {
        let chain_spec = Arc::new(TaikoChainSpec::default());
        let assembler = TaikoBlockAssembler::new(chain_spec.clone());

        assert_eq!(assembler.chain_spec(), chain_spec);
    }

    #[test]
    fn assembled_uzen_block_uses_final_zk_gas_as_difficulty() {
        let assembler = TaikoBlockAssembler::new(Arc::new(TaikoChainSpec::default()));
        let mut evm_env: EvmEnv<TaikoSpecId> = EvmEnv::default();
        evm_env.cfg_env.spec = TaikoSpecId::UZEN;
        evm_env.block_env.number = U256::from(1);
        evm_env.block_env.timestamp = U256::from(1);
        evm_env.block_env.gas_limit = 30_000_000;
        evm_env.block_env.difficulty = U256::from(999_u64);

        let ctx = TaikoBlockExecutionCtx {
            parent_hash: B256::ZERO,
            parent_beacon_block_root: None,
            ommers: &[],
            withdrawals: None,
            basefee_per_gas: 0,
            extra_data: Bytes::default(),
            is_uzen_active: true,
            expected_difficulty: None,
            finalized_block_zk_gas: Default::default(),
        };
        ctx.set_finalized_block_zk_gas(42);

        let parent = SealedHeader::seal_slow(Header::default());
        let output: BlockExecutionResult<Receipt> = BlockExecutionResult {
            receipts: Vec::new(),
            requests: Requests::default(),
            gas_used: 0,
            blob_gas_used: 0,
        };
        let bundle_state = BundleState::default();
        let state_provider = NoopProvider::default();

        let block = assembler
            .assemble_block(BlockAssemblerInput::<TaikoBlockExecutorFactory>::new(
                evm_env,
                ctx,
                &parent,
                vec![sample_transaction()],
                &output,
                &bundle_state,
                &state_provider,
                B256::ZERO,
            ))
            .expect("Uzen block should assemble");

        assert_eq!(block.header.difficulty, U256::from(42_u64));
    }

    fn sample_transaction() -> TransactionSigned {
        let tx = TxLegacy {
            chain_id: Some(ChainId::from(167_u64)),
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::default(),
        };
        let signature = Signature::new(U256::from(1_u64), U256::from(2_u64), false);
        Signed::new_unchecked(tx, signature, B256::ZERO).into()
    }
}
