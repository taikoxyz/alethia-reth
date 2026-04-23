//! Prover helpers for executing derived candidate blocks.

use alloy_consensus::transaction::Recovered;
use reth_ethereum_primitives::{Block, Receipt, TransactionSigned};
use reth_evm::{
    ConfigureEvm,
    block::BlockExecutionError,
    execute::{BlockAssembler, BlockAssemblerInput},
};
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::{RecoveredBlock, SealedHeader};
use reth_revm::{
    Database, State,
    db::states::{BundleState, bundle_state::BundleRetention},
};
use reth_storage_api::noop::NoopProvider;
use reth_trie_common::{HashedPostState, KeccakKeyHasher};

use crate::{
    config::{MissingBaseFee, TaikoEvmConfig, TaikoNextBlockEnvAttributes},
    executor::TaikoBlockExecutor,
    factory::TaikoBlockExecutorFactory,
};

/// Execution artifacts produced by prover-mode derived block execution.
#[derive(Debug)]
pub struct DerivedBlockExecutionOutcome {
    /// Transactions that were actually committed by block execution.
    pub committed_transactions: Vec<Recovered<TransactionSigned>>,
    /// Execution result for the committed transactions.
    pub execution_result: BlockExecutionResult<Receipt>,
    /// Hashed post-state derived from the execution bundle.
    pub hashed_state: HashedPostState,
    /// Finalized zk gas accumulated by committed transactions.
    pub finalized_block_zk_gas: u64,
}

/// Derives next-block environment attributes from a candidate derived block header.
fn attributes_from_derived_block(
    derived_block: &RecoveredBlock<Block>,
) -> Result<TaikoNextBlockEnvAttributes, BlockExecutionError> {
    let header = derived_block.header();
    let base_fee_per_gas = header.base_fee_per_gas.ok_or_else(|| {
        BlockExecutionError::other(MissingBaseFee { block_number: header.number })
    })?;

    Ok(TaikoNextBlockEnvAttributes {
        timestamp: header.timestamp,
        suggested_fee_recipient: header.beneficiary,
        prev_randao: header.mix_hash,
        gas_limit: header.gas_limit,
        extra_data: header.extra_data.clone(),
        base_fee_per_gas,
    })
}

/// Executes a candidate derived block in prover mode.
pub fn execute_derived_block<DB>(
    evm_config: &TaikoEvmConfig,
    parent_header: &SealedHeader,
    derived_block: &RecoveredBlock<Block>,
    db: DB,
) -> Result<DerivedBlockExecutionOutcome, BlockExecutionError>
where
    DB: Database + std::fmt::Debug,
{
    let mut state = State::builder().with_database(db).with_bundle_update().build();
    let attributes = attributes_from_derived_block(derived_block)?;
    let evm_env =
        evm_config.next_evm_env(parent_header, &attributes).map_err(BlockExecutionError::other)?;
    let evm = evm_config.evm_with_env(&mut state, evm_env);
    let execution_ctx = evm_config
        .context_for_next_block(parent_header, attributes)
        .map_err(BlockExecutionError::other)?;
    let finalized_zk_gas = execution_ctx.finalized_block_zk_gas.clone();
    let executor = TaikoBlockExecutor::new(
        evm,
        execution_ctx,
        evm_config.executor_factory.spec().clone(),
        evm_config.executor_factory.receipt_builder(),
    );

    let execution_outcome =
        executor.execute_block_with_committed_transactions(derived_block.transactions_recovered())?;
    state.merge_transitions(BundleRetention::Reverts);

    let bundle_state = state.take_bundle();
    let hashed_state = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state());

    Ok(DerivedBlockExecutionOutcome {
        committed_transactions: execution_outcome.committed_transactions,
        execution_result: execution_outcome.execution_result,
        hashed_state,
        finalized_block_zk_gas: finalized_zk_gas.load(std::sync::atomic::Ordering::Relaxed),
    })
}

/// Assembles the filtered block produced by derived block execution.
pub fn assemble_filtered_block(
    evm_config: &TaikoEvmConfig,
    parent_header: &SealedHeader,
    derived_block: &RecoveredBlock<Block>,
    committed_transactions: Vec<Recovered<TransactionSigned>>,
    execution_result: &BlockExecutionResult<Receipt>,
    finalized_block_zk_gas: u64,
    state_root: alloy_primitives::B256,
) -> Result<RecoveredBlock<Block>, BlockExecutionError> {
    let attributes = attributes_from_derived_block(derived_block)?;
    let evm_env =
        evm_config.next_evm_env(parent_header, &attributes).map_err(BlockExecutionError::other)?;
    let execution_ctx = evm_config
        .context_for_next_block(parent_header, attributes)
        .map_err(BlockExecutionError::other)?;
    execution_ctx.set_finalized_block_zk_gas(finalized_block_zk_gas);
    let bundle_state = BundleState::default();
    let state_provider = NoopProvider::default();

    let senders = committed_transactions.iter().map(Recovered::signer).collect();
    let transactions = committed_transactions.into_iter().map(Recovered::into_inner).collect();

    let block = evm_config.block_assembler.assemble_block(BlockAssemblerInput::<
        TaikoBlockExecutorFactory,
    >::new(
        evm_env,
        execution_ctx,
        parent_header,
        transactions,
        execution_result,
        &bundle_state,
        &state_provider,
        state_root,
    ))?;

    Ok(RecoveredBlock::new_unhashed(block, senders))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{Header, Signed, TxLegacy, transaction::Recovered};
    use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U256};
    use reth_ethereum_primitives::{Block, BlockBody};

    use super::*;
    use crate::{
        config::TaikoEvmConfig,
        testutil::{BENCH_SUCCESS_TARGET, db_with_contracts},
    };
    use alethia_reth_chainspec::spec::TaikoChainSpec;

    const TEST_CALLER: Address = Address::with_last_byte(0x30);

    fn test_tx(chain_id: u64, nonce: u64) -> Recovered<TransactionSigned> {
        let tx = TxLegacy {
            chain_id: Some(ChainId::from(chain_id)),
            nonce,
            gas_price: 1,
            gas_limit: 5_000_000,
            to: TxKind::Call(BENCH_SUCCESS_TARGET),
            value: U256::ZERO,
            input: Bytes::default(),
        };
        let signature = Signature::new(U256::from(1_u64), U256::from(2_u64), false);
        Recovered::new_unchecked(
            Signed::new_unchecked(tx, signature, B256::with_last_byte(TEST_CALLER.as_slice()[19]))
                .into(),
            TEST_CALLER,
        )
    }

    #[test]
    fn execute_derived_block_skips_invalid_nonce_transaction_and_records_committed_txs() {
        let chain_spec = Arc::new(TaikoChainSpec::default());
        let chain_id = chain_spec.inner.chain().id();
        let config = TaikoEvmConfig::new(chain_spec);
        let parent_header = SealedHeader::seal_slow(Header::default());
        let anchor_tx = test_tx(chain_id, 0);
        let valid_tx = test_tx(chain_id, 1);
        let invalid_tx = test_tx(chain_id, 99);
        let transactions =
            vec![anchor_tx.clone_inner(), valid_tx.clone_inner(), invalid_tx.clone_inner()];
        let senders = vec![anchor_tx.signer(), valid_tx.signer(), invalid_tx.signer()];
        let derived_block = RecoveredBlock::new_unhashed(
            Block {
                header: Header {
                    number: 1,
                    timestamp: 1,
                    gas_limit: 30_000_000,
                    base_fee_per_gas: Some(0),
                    parent_beacon_block_root: Some(B256::ZERO),
                    ..Default::default()
                },
                body: BlockBody { transactions, ommers: Default::default(), withdrawals: None },
            },
            senders,
        );

        let outcome = execute_derived_block(
            &config,
            &parent_header,
            &derived_block,
            db_with_contracts(&[(TEST_CALLER, 0)]),
        )
        .expect("derived block execution should skip invalid nonce tx");

        assert_eq!(outcome.committed_transactions.len(), 2);

        let filtered_block = assemble_filtered_block(
            &config,
            &parent_header,
            &derived_block,
            outcome.committed_transactions,
            &outcome.execution_result,
            outcome.finalized_block_zk_gas,
            B256::ZERO,
        )
        .expect("filtered block should assemble");

        assert_eq!(filtered_block.body().transactions().count(), 2);
    }
}
