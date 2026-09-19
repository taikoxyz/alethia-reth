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
use reth_trie_common::{HashedPostState, KeccakKeyHasher, KeyHasher};

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
        parent_beacon_block_root: header.parent_beacon_block_root,
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

    let execution_outcome = executor
        .execute_block_with_committed_transactions(derived_block.transactions_recovered())?;
    state.merge_transitions(BundleRetention::Reverts);

    let bundle_state = state.take_bundle();
    let hashed_state = hashed_post_state(&bundle_state);

    Ok(DerivedBlockExecutionOutcome {
        committed_transactions: execution_outcome.committed_transactions,
        execution_result: execution_outcome.execution_result,
        hashed_state,
        finalized_block_zk_gas: finalized_zk_gas.load(std::sync::atomic::Ordering::Relaxed),
    })
}

/// Hashes the execution bundle into a [`HashedPostState`], marking every destroyed account's
/// storage as wiped.
///
/// Since reth v2.5, [`HashedPostState::from_bundle_state`] no longer derives `wiped` from the
/// account status: reth's state providers instead expand a destroyed account's parent slots into
/// explicit zeroes, which needs parent-state access this helper does not have. The prover's
/// stateless trie clears a storage trie only when `wiped` is set, so the flag is restored here;
/// otherwise an account destroyed under pre-Unzen (pre-EIP-6780) SELFDESTRUCT semantics and
/// re-created in the same block would keep its pre-block slots.
fn hashed_post_state(bundle_state: &BundleState) -> HashedPostState {
    let mut hashed_state =
        HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state());
    for (address, account) in bundle_state.state() {
        if account.status.was_destroyed() {
            hashed_state.storages.entry(KeccakKeyHasher::hash_key(address)).or_default().wiped =
                true;
        }
    }
    hashed_state
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
        None,
    ))?;

    Ok(RecoveredBlock::new_unhashed(block, senders))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{Header, Signed, TxLegacy, transaction::Recovered};
    use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U256, keccak256};
    use reth_ethereum_primitives::{Block, BlockBody};
    use reth_revm::state::{Bytecode, bytecode::opcode};

    use super::*;
    use crate::{
        config::TaikoEvmConfig,
        testutil::{
            BENCH_SUCCESS_TARGET, db_with_contracts, insert_contract, recovered_tx_with_chain_id,
        },
    };
    use alethia_reth_chainspec::spec::TaikoChainSpec;

    const TEST_CALLER: Address = Address::with_last_byte(0x30);

    /// Pre-existing contract with storage whose code is `CALLER SELFDESTRUCT`.
    const SELF_DESTRUCT_TARGET: Address = Address::with_last_byte(0x40);

    fn test_transaction(chain_id: u64, nonce: u64) -> Recovered<TransactionSigned> {
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
        let anchor_transaction = test_transaction(chain_id, 0);
        let valid_transaction = test_transaction(chain_id, 1);
        let invalid_transaction = test_transaction(chain_id, 99);
        let transactions = vec![
            anchor_transaction.clone_inner(),
            valid_transaction.clone_inner(),
            invalid_transaction.clone_inner(),
        ];
        let senders = vec![
            anchor_transaction.signer(),
            valid_transaction.signer(),
            invalid_transaction.signer(),
        ];
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

    #[test]
    fn execute_derived_block_marks_self_destructed_storage_as_wiped() {
        // No Taiko fork is scheduled, so the block executes with SHANGHAI semantics (every
        // pre-Unzen fork does), where SELFDESTRUCT deletes a pre-existing contract together
        // with its storage. The prover's stateless trie clears a storage trie only when the
        // hashed storage is marked `wiped`, so the outcome must carry that flag.
        let chain_spec = Arc::new(TaikoChainSpec::default());
        let chain_id = chain_spec.inner.chain().id();
        let config = TaikoEvmConfig::new(chain_spec);
        let parent_header = SealedHeader::seal_slow(Header::default());

        let mut db = db_with_contracts(&[(TEST_CALLER, 0)]);
        insert_contract(
            &mut db,
            SELF_DESTRUCT_TARGET,
            Bytecode::new_raw(Bytes::from(vec![opcode::CALLER, opcode::SELFDESTRUCT])),
        );
        db.insert_account_storage(SELF_DESTRUCT_TARGET, U256::from(1_u64), U256::from(42_u64))
            .expect("in-memory storage insert cannot fail");

        let anchor_transaction = test_transaction(chain_id, 0);
        let destruct_transaction =
            recovered_tx_with_chain_id(TEST_CALLER, SELF_DESTRUCT_TARGET, 1, 1, chain_id);
        let derived_block = RecoveredBlock::new_unhashed(
            Block {
                header: Header {
                    number: 1,
                    timestamp: 1,
                    gas_limit: 30_000_000,
                    base_fee_per_gas: Some(0),
                    ..Default::default()
                },
                body: BlockBody {
                    transactions: vec![
                        anchor_transaction.clone_inner(),
                        destruct_transaction.clone_inner(),
                    ],
                    ommers: Default::default(),
                    withdrawals: None,
                },
            },
            vec![anchor_transaction.signer(), destruct_transaction.signer()],
        );

        let outcome = execute_derived_block(&config, &parent_header, &derived_block, db)
            .expect("derived block execution should succeed");

        assert_eq!(outcome.committed_transactions.len(), 2);
        let hashed_address = keccak256(SELF_DESTRUCT_TARGET);
        assert_eq!(
            outcome.hashed_state.accounts.get(&hashed_address),
            Some(&None),
            "the self-destructed contract must be removed from state"
        );
        let storage = outcome
            .hashed_state
            .storages
            .get(&hashed_address)
            .expect("a destroyed account must carry a hashed storage entry");
        assert!(storage.wiped, "a destroyed account's storage must be marked as wiped");
    }
}
