//! Block assembler implementation for Taiko headers and block bodies.
use alloy_consensus::{
    BlockBody, EMPTY_OMMER_ROOT_HASH, Header, TxReceipt, constants::EMPTY_WITHDRAWALS, proofs,
};
use alloy_eips::{eip7685::EMPTY_REQUESTS_HASH, merge::BEACON_NONCE};
use alloy_primitives::{B256, Bloom, U256};
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::Withdrawals;
use reth_ethereum_primitives::{Block, Receipt, TransactionSigned};
use reth_evm::{
    block::{BlockExecutionError, BlockExecutorFactory},
    execute::{BlockAssembler, BlockAssemblerInput},
};
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::{SealedHeader, logs_bloom};
use reth_revm::context::Block as _;

use crate::{config::TaikoNextBlockEnvAttributes, factory::TaikoBlockExecutionCtx};

/// A block assembler for the Taiko network that implements the `BlockAssembler` trait.
///
/// Taiko headers are assembled entirely from the execution context and EVM environment, so the
/// assembler carries no state.
#[derive(Clone, Copy, Debug, Default)]
pub struct TaikoBlockAssembler;

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
        let receipts_root = proofs::calculate_receipt_root(
            &receipts.iter().map(TxReceipt::with_bloom_ref).collect::<Vec<_>>(),
        );
        let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| r.logs()));

        let withdrawals = Some(Withdrawals::default());
        let withdrawals_root = Some(EMPTY_WITHDRAWALS);
        let requests_hash = ctx.is_unzen_active.then_some(EMPTY_REQUESTS_HASH);
        let difficulty = if ctx.is_unzen_active {
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
            blob_gas_used: ctx.is_unzen_active.then_some(0),
            excess_blob_gas: ctx.is_unzen_active.then_some(0),
            requests_hash,
            block_access_list_hash: None,
            slot_number: None,
        };

        Ok(Block {
            header,
            body: BlockBody { transactions, ommers: Default::default(), withdrawals },
        })
    }
}

/// Returns an upper bound on the RLP length of every Etna header [`TaikoBlockAssembler`] can
/// assemble for a block built on `parent` in the environment `attributes` describes.
///
/// A payload builder seals the header only after execution, so it derives its transaction byte
/// budget from this bound instead. The bound is the length of a header template built the way
/// [`TaikoBlockAssembler::assemble_block`] builds an Etna header, so the two must change together:
/// - The fields taken from the parent and `attributes` are exact: the parent hash, beneficiary,
///   block number (`parent.number + 1`), gas limit, timestamp, extraData, mixHash and base fee come
///   from the same parent and attributes as the block environment, and the ommers hash, nonce and
///   withdrawals root are constants.
/// - The fields known only after execution are fixed-size or bounded: the state, transactions and
///   receipts roots are always 32 bytes and the logs bloom always 256 bytes, whatever their value;
///   `gasUsed` never exceeds the gas limit, because the executor rejects a transaction whose gas
///   limit exceeds the block's remaining gas; and the difficulty carries the block's zk gas, a
///   `u64`, so `u64::MAX` bounds it. An RLP integer never gets shorter as it grows.
/// - Every optional field an Etna header carries is present (blob gas used and excess blob gas, the
///   beacon root, the requests hash); a header without one of them is only shorter. The block
///   access list hash and slot number stay absent, as the assembler leaves them.
pub fn etna_header_length_bound(
    parent: &SealedHeader,
    attributes: &TaikoNextBlockEnvAttributes,
) -> usize {
    Header {
        parent_hash: parent.hash(),
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        beneficiary: attributes.suggested_fee_recipient,
        state_root: B256::ZERO,
        transactions_root: B256::ZERO,
        receipts_root: B256::ZERO,
        logs_bloom: Bloom::ZERO,
        difficulty: U256::from(u64::MAX),
        number: parent.number + 1,
        gas_limit: attributes.gas_limit,
        gas_used: attributes.gas_limit,
        timestamp: attributes.timestamp,
        extra_data: attributes.extra_data.clone(),
        mix_hash: attributes.prev_randao,
        nonce: BEACON_NONCE.into(),
        base_fee_per_gas: Some(attributes.base_fee_per_gas),
        withdrawals_root: Some(EMPTY_WITHDRAWALS),
        blob_gas_used: Some(0),
        excess_blob_gas: Some(0),
        parent_beacon_block_root: Some(attributes.parent_beacon_block_root.unwrap_or_default()),
        requests_hash: Some(EMPTY_REQUESTS_HASH),
        block_access_list_hash: None,
        slot_number: None,
    }
    .length()
}

#[cfg(test)]
mod test {
    use alloy_consensus::{Header, Signed, TxLegacy};
    use alloy_eips::eip7685::{EMPTY_REQUESTS_HASH, Requests};
    use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U256};
    use reth_evm::{
        EvmEnv,
        execute::{BlockAssembler, BlockAssemblerInput},
    };
    use reth_execution_types::BlockExecutionResult;
    use reth_primitives_traits::SealedHeader;
    use reth_revm::db::states::BundleState;
    use reth_storage_api::noop::NoopProvider;

    use super::*;
    use crate::factory::{TaikoBlockExecutionCtx, TaikoBlockExecutorFactory};
    use alethia_reth_evm::{env::TaikoEvmEnv, spec::TaikoSpecId};

    #[test]
    fn assembled_unzen_block_uses_final_zk_gas_as_difficulty() {
        let assembler = TaikoBlockAssembler;
        let mut evm_env: TaikoEvmEnv = EvmEnv::default();
        evm_env.cfg_env.spec = TaikoSpecId::UNZEN;
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
            is_unzen_active: true,
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
                None,
            ))
            .expect("Unzen block should assemble");

        assert_eq!(block.header.difficulty, U256::from(42_u64));
    }

    #[test]
    fn assembled_unzen_block_sets_requests_hash() {
        let assembler = TaikoBlockAssembler;
        let mut evm_env: TaikoEvmEnv = EvmEnv::default();
        evm_env.cfg_env.spec = TaikoSpecId::UNZEN;
        evm_env.block_env.number = U256::from(1);
        evm_env.block_env.timestamp = U256::from(1);
        evm_env.block_env.gas_limit = 30_000_000;

        let ctx = TaikoBlockExecutionCtx {
            parent_hash: B256::ZERO,
            parent_beacon_block_root: Some(B256::ZERO),
            ommers: &[],
            withdrawals: None,
            basefee_per_gas: 0,
            extra_data: Bytes::default(),
            is_unzen_active: true,
            expected_difficulty: None,
            finalized_block_zk_gas: Default::default(),
        };

        let parent = SealedHeader::seal_slow(Header::default());
        let output: BlockExecutionResult<Receipt> = BlockExecutionResult {
            receipts: Vec::new(),
            requests: Default::default(),
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
                None,
            ))
            .expect("Unzen block should assemble");

        assert_eq!(block.header.requests_hash, Some(EMPTY_REQUESTS_HASH));
        assert_eq!(block.header.blob_gas_used, Some(0));
        assert_eq!(block.header.excess_blob_gas, Some(0));
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
