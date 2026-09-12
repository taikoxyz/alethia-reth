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
#[cfg(all(feature = "execution-observer", feature = "prover"))]
use alethia_reth_evm::zk_gas::observer::{ExecutionEvent, SharedExecutionObserver};

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
    let hashed_state = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state());

    Ok(DerivedBlockExecutionOutcome {
        committed_transactions: execution_outcome.committed_transactions,
        execution_result: execution_outcome.execution_result,
        hashed_state,
        finalized_block_zk_gas: finalized_zk_gas.load(std::sync::atomic::Ordering::Relaxed),
    })
}

/// Executes a candidate derived block through the feature-gated observer EVM path.
///
/// The observer is infallible and cannot influence receipt, state, or zk gas results. Normal
/// callers continue to use [`execute_derived_block`], whose construction path remains observer-free.
#[cfg(all(feature = "execution-observer", feature = "prover"))]
pub fn execute_derived_block_with_observer<DB>(
    evm_config: &TaikoEvmConfig,
    parent_header: &SealedHeader,
    derived_block: &RecoveredBlock<Block>,
    db: DB,
    block_index: u64,
    observer: SharedExecutionObserver,
) -> Result<DerivedBlockExecutionOutcome, BlockExecutionError>
where
    DB: Database + std::fmt::Debug,
{
    let mut state = State::builder().with_database(db).with_bundle_update().build();
    let attributes = attributes_from_derived_block(derived_block)?;
    let evm_env =
        evm_config.next_evm_env(parent_header, &attributes).map_err(BlockExecutionError::other)?;
    let zk_gas_schedule = alethia_reth_evm::zk_gas::schedule::schedule_for(evm_env.cfg_env.spec);
    let evm = evm_config.evm_factory().create_evm_with_execution_observer(
        &mut state,
        evm_env,
        observer.clone(),
    );
    let execution_ctx = evm_config
        .context_for_next_block(parent_header, attributes)
        .map_err(BlockExecutionError::other)?;
    observer.on_event(ExecutionEvent::BlockStart {
        block_index,
        block_number: derived_block.header().number,
        expected_difficulty: execution_ctx
            .expected_difficulty()
            .map(|difficulty| difficulty.to_be_bytes::<32>()),
        block_limit: zk_gas_schedule.map_or(0, |schedule| schedule.block_limit),
        recovered_tx_count: u64::try_from(derived_block.body().transactions().count())
            .expect("transaction count must fit u64"),
    });
    let finalized_zk_gas = execution_ctx.finalized_block_zk_gas.clone();
    let executor = TaikoBlockExecutor::new_with_execution_observer(
        evm,
        execution_ctx,
        evm_config.executor_factory.spec().clone(),
        evm_config.executor_factory.receipt_builder(),
        observer.clone(),
    );

    let execution_outcome = executor
        .execute_block_with_committed_transactions(derived_block.transactions_recovered())?;
    state.merge_transitions(BundleRetention::Reverts);

    let bundle_state = state.take_bundle();
    let hashed_state = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state());
    let finalized_block_zk_gas = finalized_zk_gas.load(std::sync::atomic::Ordering::Relaxed);
    observer.on_event(ExecutionEvent::BlockEnd { finalized_current_zkgas: finalized_block_zk_gas });

    Ok(DerivedBlockExecutionOutcome {
        committed_transactions: execution_outcome.committed_transactions,
        execution_result: execution_outcome.execution_result,
        hashed_state,
        finalized_block_zk_gas,
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
        None,
    ))?;

    Ok(RecoveredBlock::new_unhashed(block, senders))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use alloy_consensus::{Header, Signed, TxLegacy, transaction::Recovered};
    use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U256};
    use reth_ethereum_primitives::{Block, BlockBody};
    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    use reth_revm::{
        Database,
        db::InMemoryDB,
        state::{AccountInfo, Bytecode, bytecode::opcode},
    };

    use super::*;
    use crate::{
        config::TaikoEvmConfig,
        testutil::{BENCH_SUCCESS_TARGET, db_with_contracts, unzen_chain_spec},
    };
    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    use alethia_reth_evm::zk_gas::observer::{ExecutionEvent, ExecutionObserver};

    const TEST_CALLER: Address = Address::with_last_byte(0x30);

    /// Counts normal database account reads so the observer A/B test can prove it adds none.
    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[derive(Debug)]
    struct CountingDb {
        inner: InMemoryDB,
        basic_calls: Arc<AtomicUsize>,
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    impl CountingDb {
        fn new(inner: InMemoryDB, basic_calls: Arc<AtomicUsize>) -> Self {
            Self { inner, basic_calls }
        }
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    impl Database for CountingDb {
        type Error = core::convert::Infallible;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            self.basic_calls.fetch_add(1, Ordering::Relaxed);
            self.inner.basic(address)
        }

        fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
            self.inner.code_by_hash(code_hash)
        }

        fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
            self.inner.storage(address, index)
        }

        fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
            self.inner.block_hash(number)
        }
    }

    /// Fails on the first account read beyond the normal execution's observed budget.
    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[derive(Debug)]
    struct FailAfterBasicDb {
        inner: InMemoryDB,
        allowed_basic_reads: usize,
        basic_reads: usize,
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    impl Database for FailAfterBasicDb {
        type Error = revm_database_interface::ErasedError;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            if self.basic_reads == self.allowed_basic_reads {
                return Err(revm_database_interface::ErasedError::new(std::io::Error::other(
                    "unexpected observer database read",
                )));
            }
            self.basic_reads += 1;
            Ok(self.inner.basic(address).expect("in-memory database is infallible"))
        }

        fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(self.inner.code_by_hash(code_hash).expect("in-memory database is infallible"))
        }

        fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
            Ok(self.inner.storage(address, index).expect("in-memory database is infallible"))
        }

        fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
            Ok(self.inner.block_hash(number).expect("in-memory database is infallible"))
        }
    }

    fn test_transaction_to(
        chain_id: u64,
        nonce: u64,
        target: Address,
    ) -> Recovered<TransactionSigned> {
        let tx = TxLegacy {
            chain_id: Some(ChainId::from(chain_id)),
            nonce,
            gas_price: 1,
            gas_limit: 5_000_000,
            to: TxKind::Call(target),
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

    fn test_transaction(chain_id: u64, nonce: u64) -> Recovered<TransactionSigned> {
        test_transaction_to(chain_id, nonce, BENCH_SUCCESS_TARGET)
    }

    #[test]
    fn execute_derived_block_skips_invalid_nonce_transaction_and_records_committed_txs() {
        let chain_spec = Arc::new(unzen_chain_spec());
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

    /// Records observer events so the test can assert the public derived-block tracing contract.
    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[derive(Default)]
    struct RecordingObserver {
        events: Mutex<Vec<ExecutionEvent>>,
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    impl ExecutionObserver for RecordingObserver {
        fn on_event(&self, event: ExecutionEvent) {
            self.events.lock().expect("observer lock should not be poisoned").push(event);
        }
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[test]
    fn observer_execution_matches_normal_derived_block_execution() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let chain_id = chain_spec.inner.chain().id();
        let config = TaikoEvmConfig::new(chain_spec);
        let parent_header = SealedHeader::seal_slow(Header::default());
        let anchor_transaction = test_transaction(chain_id, 0);
        let valid_transaction = test_transaction(chain_id, 1);
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
                body: BlockBody {
                    transactions: vec![
                        anchor_transaction.clone_inner(),
                        valid_transaction.clone_inner(),
                    ],
                    ommers: Default::default(),
                    withdrawals: None,
                },
            },
            vec![anchor_transaction.signer(), valid_transaction.signer()],
        );

        let normal = execute_derived_block(
            &config,
            &parent_header,
            &derived_block,
            db_with_contracts(&[(TEST_CALLER, 0)]),
        )
        .expect("normal derived execution should succeed");
        let observer = Arc::new(RecordingObserver::default());
        let observed = execute_derived_block_with_observer(
            &config,
            &parent_header,
            &derived_block,
            db_with_contracts(&[(TEST_CALLER, 0)]),
            7,
            observer.clone(),
        )
        .expect("observed derived execution should succeed");

        assert_eq!(observed.committed_transactions, normal.committed_transactions);
        assert_eq!(observed.execution_result, normal.execution_result);
        assert_eq!(observed.hashed_state, normal.hashed_state);
        assert_eq!(observed.finalized_block_zk_gas, normal.finalized_block_zk_gas);

        let events = observer.events.lock().expect("observer lock should not be poisoned");
        assert!(matches!(events.first(), Some(ExecutionEvent::BlockStart { block_index: 7, .. })));
        assert!(matches!(events.last(), Some(ExecutionEvent::BlockEnd { .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::PhaseStart {
                phase: alethia_reth_evm::zk_gas::observer::ExecutionPhase::Transactions
            }
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ExecutionEvent::TransactionStart { .. }))
                .count(),
            2,
            "each recovered transaction must open exactly one transaction buffer"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::ChargeAttempt { operation_id: None, .. }
        )));
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[test]
    fn observer_classification_adds_no_database_basic_reads() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let chain_id = chain_spec.inner.chain().id();
        let config = TaikoEvmConfig::new(chain_spec);
        let anchor = test_transaction(chain_id, 0);
        let call = test_transaction(chain_id, 1);
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
                body: BlockBody {
                    transactions: vec![anchor.clone_inner(), call.clone_inner()],
                    ommers: Default::default(),
                    withdrawals: None,
                },
            },
            vec![anchor.signer(), call.signer()],
        );
        let parent_header = SealedHeader::seal_slow(Header::default());
        let normal_calls = Arc::new(AtomicUsize::new(0));
        let normal = execute_derived_block(
            &config,
            &parent_header,
            &derived_block,
            CountingDb::new(db_with_contracts(&[(TEST_CALLER, 0)]), normal_calls.clone()),
        )
        .expect("normal execution should succeed");
        let observed_calls = Arc::new(AtomicUsize::new(0));
        let observed = execute_derived_block_with_observer(
            &config,
            &parent_header,
            &derived_block,
            CountingDb::new(db_with_contracts(&[(TEST_CALLER, 0)]), observed_calls.clone()),
            0,
            Arc::new(RecordingObserver::default()),
        )
        .expect("observer execution should succeed");

        assert_eq!(observed.committed_transactions, normal.committed_transactions);
        assert_eq!(observed.execution_result, normal.execution_result);
        assert_eq!(
            observed_calls.load(Ordering::Relaxed),
            normal_calls.load(Ordering::Relaxed),
            "observer classification must not add a database basic read"
        );

        let observer = Arc::new(RecordingObserver::default());
        execute_derived_block_with_observer(
            &config,
            &parent_header,
            &derived_block,
            FailAfterBasicDb {
                inner: db_with_contracts(&[(TEST_CALLER, 0)]),
                allowed_basic_reads: normal_calls.load(Ordering::Relaxed),
                basic_reads: 0,
            },
            1,
            observer,
        )
        .expect("the observer must not consume a one-shot database error before normal execution");
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[test]
    fn observer_marks_first_unattempted_tail_after_zk_gas_truncation() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let chain_id = chain_spec.inner.chain().id();
        let config = TaikoEvmConfig::new(chain_spec);
        let parent_header = SealedHeader::seal_slow(Header::default());
        let transactions = vec![
            test_transaction(chain_id, 0),
            test_transaction_to(chain_id, 1, crate::testutil::BENCH_LIMIT_TARGET),
            test_transaction(chain_id, 2),
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
                body: BlockBody {
                    transactions: transactions.iter().map(Recovered::clone_inner).collect(),
                    ommers: Default::default(),
                    withdrawals: None,
                },
            },
            transactions.iter().map(Recovered::signer).collect(),
        );
        let observer = Arc::new(RecordingObserver::default());
        execute_derived_block_with_observer(
            &config,
            &parent_header,
            &derived_block,
            db_with_contracts(&[(TEST_CALLER, 0)]),
            0,
            observer.clone(),
        )
        .expect("truncating candidate should remain a successful filtered block");
        let events = observer.events.lock().expect("observer lock should not be poisoned");
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::BlockStop {
                reason: alethia_reth_evm::zk_gas::observer::BlockStopReason::ZkGasTruncated,
                first_unattempted_tx_index: Some(2)
            }
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ExecutionEvent::TransactionStart { .. }))
                .count(),
            2
        );
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[test]
    fn observer_omits_tail_index_when_the_final_transaction_truncates() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let chain_id = chain_spec.inner.chain().id();
        let config = TaikoEvmConfig::new(chain_spec);
        let transactions = vec![
            test_transaction(chain_id, 0),
            test_transaction_to(chain_id, 1, crate::testutil::BENCH_LIMIT_TARGET),
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
                body: BlockBody {
                    transactions: transactions.iter().map(Recovered::clone_inner).collect(),
                    ommers: Default::default(),
                    withdrawals: None,
                },
            },
            transactions.iter().map(Recovered::signer).collect(),
        );
        let observer = Arc::new(RecordingObserver::default());
        execute_derived_block_with_observer(
            &config,
            &SealedHeader::seal_slow(Header::default()),
            &derived_block,
            db_with_contracts(&[(TEST_CALLER, 0)]),
            0,
            observer.clone(),
        )
        .expect("final truncating candidate should remain a successful filtered block");
        assert!(observer.events.lock().expect("observer lock should not be poisoned").iter().any(
            |event| matches!(
                event,
                ExecutionEvent::BlockStop {
                    reason: alethia_reth_evm::zk_gas::observer::BlockStopReason::ZkGasTruncated,
                    first_unattempted_tx_index: None,
                }
            ),
        ));
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[test]
    fn observer_marks_committed_revert_transaction_end() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let chain_id = chain_spec.inner.chain().id();
        let config = TaikoEvmConfig::new(chain_spec);
        let parent_header = SealedHeader::seal_slow(Header::default());
        let reverting_target = Address::with_last_byte(0x24);
        let transactions =
            vec![test_transaction(chain_id, 0), test_transaction_to(chain_id, 1, reverting_target)];
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
                body: BlockBody {
                    transactions: transactions.iter().map(Recovered::clone_inner).collect(),
                    ommers: Default::default(),
                    withdrawals: None,
                },
            },
            transactions.iter().map(Recovered::signer).collect(),
        );
        let mut db = db_with_contracts(&[(TEST_CALLER, 0)]);
        crate::testutil::insert_contract(
            &mut db,
            reverting_target,
            Bytecode::new_raw(Bytes::from(vec![
                opcode::PUSH1,
                0x00,
                opcode::PUSH1,
                0x00,
                opcode::REVERT,
            ])),
        );
        let observer = Arc::new(RecordingObserver::default());
        let outcome = execute_derived_block_with_observer(
            &config,
            &parent_header,
            &derived_block,
            db,
            0,
            observer.clone(),
        )
        .expect("reverted transactions must remain committed derived-block transactions");

        assert_eq!(outcome.committed_transactions.len(), 2);
        let events = observer.events.lock().expect("observer lock should not be poisoned");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ExecutionEvent::TransactionStart { tx_index: 1, .. }))
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                ExecutionEvent::TransactionEnd {
                    tx_index: 1,
                    disposition:
                        alethia_reth_evm::zk_gas::observer::TransactionDisposition::CommittedRevert,
                    execution_class:
                        alethia_reth_evm::zk_gas::observer::TransactionExecutionClass::ContractCall,
                    ..
                }
            )),
            "the normal top-level frame repairs the cold contract's provisional start class"
        );
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[test]
    fn observer_records_each_precompile_body_once_before_its_linked_charge() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let chain_id = chain_spec.inner.chain().id();
        let config = TaikoEvmConfig::new(chain_spec);
        let parent_header = SealedHeader::seal_slow(Header::default());
        let transactions = vec![
            test_transaction(chain_id, 0),
            test_transaction_to(chain_id, 1, Address::with_last_byte(0x04)),
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
                body: BlockBody {
                    transactions: transactions.iter().map(Recovered::clone_inner).collect(),
                    ommers: Default::default(),
                    withdrawals: None,
                },
            },
            transactions.iter().map(Recovered::signer).collect(),
        );
        let observer = Arc::new(RecordingObserver::default());
        execute_derived_block_with_observer(
            &config,
            &parent_header,
            &derived_block,
            db_with_contracts(&[(TEST_CALLER, 0)]),
            0,
            observer.clone(),
        )
        .expect("precompile candidate should execute");

        let events = observer.events.lock().expect("observer lock should not be poisoned");
        let (precompile_operation, precompile_operation_id) = events
            .iter()
            .enumerate()
            .find_map(|(index, event)| match event {
                ExecutionEvent::OperationExecuted {
                    operation_id,
                    component:
                        alethia_reth_evm::zk_gas::observer::OperationComponent::Precompile {
                            address,
                            ..
                        },
                    ..
                } if *address == Address::with_last_byte(0x04).into_array() => {
                    Some((index, *operation_id))
                }
                _ => None,
            })
            .expect("precompile body must be recorded");
        let precompile_charges: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                matches!(
                    event,
                    ExecutionEvent::ChargeAttempt {
                        component: alethia_reth_evm::zk_gas::observer::ChargeComponent::Precompile {
                            address,
                        },
                        ..
                    } if *address == Address::with_last_byte(0x04).into_array()
                )
            })
            .collect();
        assert_eq!(precompile_charges.len(), 1, "precompile body must not be double-charged");
        assert!(
            precompile_operation < precompile_charges[0].0,
            "completed precompile work precedes its linked charge attempt"
        );
        assert!(matches!(
            precompile_charges[0].1,
            ExecutionEvent::ChargeAttempt { operation_id: Some(id), .. } if *id == precompile_operation_id
        ));
    }
}
