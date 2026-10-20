//! Prover helpers for executing derived candidate blocks.

use alloy_consensus::transaction::Recovered;
use alloy_primitives::{B256, keccak256};
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
use alethia_reth_evm::zk_gas::pricing::ZkGasShadowReport;

/// Execution artifacts produced by prover-mode derived block execution.
#[derive(Debug)]
pub struct DerivedBlockExecutionOutcome {
    /// Transactions that were actually committed by block execution.
    pub committed_transactions: Vec<Recovered<TransactionSigned>>,
    /// Execution result for the committed transactions.
    pub execution_result: BlockExecutionResult<Receipt>,
    /// Hashed post-state derived from the execution bundle.
    ///
    /// Built with [`HashedPostState::from_bundle_state`], so it carries only the slots execution
    /// wrote. The companion [`Self::pre_existing_destroyed_accounts`] list preserves the storage
    /// wipe identities that reth's hashed post-state representation does not carry.
    pub hashed_state: HashedPostState,
    /// Hashed identities of pre-existing accounts destroyed by execution, sorted ascending by
    /// hashed address.
    ///
    /// Consumers must wipe each account's parent storage before applying [`Self::hashed_state`],
    /// even when `hashed_state.storages` has no entry for that account. The predicate mirrors
    /// reth's `zero_destroyed_account_storage` and must be re-checked when the reth revision
    /// moves. This list can be non-empty after Unzen for harmless empty-parent-storage cases
    /// such as an EIP-161 touch or a pre-funded address created and destroyed in one
    /// transaction; consumers must not use fork activation to assert that it is empty.
    pub pre_existing_destroyed_accounts: Vec<B256>,
    /// Finalized zk gas accumulated by committed transactions.
    pub finalized_block_zk_gas: u64,
    /// Non-enforcing block-local pricing report when Shadow mode is enabled.
    pub zk_gas_shadow_report: Option<ZkGasShadowReport>,
    /// Non-enforcing report across scopes begun before revm validation.
    ///
    /// Includes validation failures and declared access lists, but excludes executor rejections
    /// for remaining native block gas or intrinsic zk gas, which occur before the scope begins.
    pub attempted_zk_gas_shadow_report: Option<ZkGasShadowReport>,
    /// Number of attempted transaction scopes, including the anchor transaction.
    ///
    /// Zero when Shadow mode is disabled or its collector is unavailable.
    pub attempted_transaction_count: u64,
}

/// Returns canonical hashed identities for pre-existing accounts destroyed in this bundle.
///
/// Consumers that execute a derived block through a custom executor must preserve this metadata
/// before converting the bundle into a [`HashedPostState`], which omits storage-wipe markers.
/// The result is sorted ascending by hashed address for deterministic stateless consumption.
pub fn pre_existing_destroyed_account_hashes(bundle_state: &BundleState) -> Vec<B256> {
    let mut destroyed = bundle_state
        .state()
        .iter()
        .filter(|(_, account)| account.status.was_destroyed() && account.original_info.is_some())
        .map(|(address, _)| keccak256(address))
        .collect::<Vec<_>>();
    destroyed.sort_unstable();
    destroyed
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
    let executor = TaikoBlockExecutor::new_with_zk_gas_mode(
        evm,
        execution_ctx,
        evm_config.executor_factory.spec().clone(),
        evm_config.executor_factory.receipt_builder(),
        evm_config.zk_gas_mode(),
    );

    let execution_outcome = executor
        .execute_block_with_committed_transactions(derived_block.transactions_recovered())?;
    state.merge_transitions(BundleRetention::Reverts);

    let bundle_state = state.take_bundle();
    let pre_existing_destroyed_accounts = pre_existing_destroyed_account_hashes(&bundle_state);
    let hashed_state = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state());

    Ok(DerivedBlockExecutionOutcome {
        committed_transactions: execution_outcome.committed_transactions,
        execution_result: execution_outcome.execution_result,
        hashed_state,
        pre_existing_destroyed_accounts,
        finalized_block_zk_gas: finalized_zk_gas.load(std::sync::atomic::Ordering::Relaxed),
        zk_gas_shadow_report: execution_outcome.zk_gas_shadow_report,
        attempted_zk_gas_shadow_report: execution_outcome.attempted_zk_gas_shadow_report,
        attempted_transaction_count: execution_outcome.attempted_transaction_count,
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
    use std::sync::Arc;

    use alloy_consensus::{Header, Signed, TxLegacy, transaction::Recovered};
    use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U256};
    use reth_ethereum_primitives::{Block, BlockBody};
    use reth_revm::{
        db::{AccountStatus, BundleAccount},
        state::{AccountInfo, Bytecode, bytecode::opcode},
    };

    use super::*;
    use crate::{
        config::TaikoEvmConfig,
        testutil::{BENCH_SUCCESS_TARGET, db_with_contracts, insert_contract, unzen_chain_spec},
    };
    use alethia_reth_chainspec::{
        TAIKO_DEVNET,
        spec::{TaikoChainSpec, TaikoDevnetConfigExt},
    };
    use alethia_reth_evm::zk_gas::pricing::{ZkGasIncompleteness, ZkGasMode, ZkGasShadowStatus};

    const TEST_CALLER: Address = Address::with_last_byte(0x30);

    #[test]
    fn pre_existing_destroyed_account_hashes_are_sorted_and_filtered() {
        let destroyed = (1..=8).map(Address::with_last_byte).collect::<Vec<_>>();
        let newly_created = Address::with_last_byte(0x20);
        let unchanged = Address::with_last_byte(0x21);
        let mut bundle = BundleState::default();
        for (index, address) in destroyed.iter().copied().enumerate() {
            let status = if index % 2 == 0 {
                AccountStatus::Destroyed
            } else {
                AccountStatus::DestroyedChanged
            };
            bundle.state.insert(
                address,
                BundleAccount::new(
                    Some(AccountInfo::default()),
                    Some(AccountInfo::default()),
                    Default::default(),
                    status,
                ),
            );
        }
        bundle.state.insert(
            newly_created,
            BundleAccount::new(
                None,
                Some(AccountInfo::default()),
                Default::default(),
                AccountStatus::DestroyedChanged,
            ),
        );
        bundle.state.insert(
            unchanged,
            BundleAccount::new(
                Some(AccountInfo::default()),
                Some(AccountInfo::default()),
                Default::default(),
                AccountStatus::Changed,
            ),
        );

        let actual = pre_existing_destroyed_account_hashes(&bundle);
        assert_eq!(actual.len(), destroyed.len());
        assert!(actual.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(destroyed.iter().all(|address| actual.contains(&keccak256(address))));
    }

    fn test_transaction(chain_id: u64, nonce: u64) -> Recovered<TransactionSigned> {
        test_transaction_with_value(chain_id, nonce, BENCH_SUCCESS_TARGET, U256::ZERO)
    }

    fn test_transaction_with_value(
        chain_id: u64,
        nonce: u64,
        to: Address,
        value: U256,
    ) -> Recovered<TransactionSigned> {
        let tx = TxLegacy {
            chain_id: Some(ChainId::from(chain_id)),
            nonce,
            gas_price: 1,
            gas_limit: 5_000_000,
            to: TxKind::Call(to),
            value,
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
        let chain_spec = Arc::new(unzen_chain_spec());
        let chain_id = chain_spec.inner.chain().id();
        let config = TaikoEvmConfig::new_with_zk_gas_mode(chain_spec, ZkGasMode::Shadow);
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
        assert_eq!(outcome.attempted_transaction_count, 3);
        assert_ne!(outcome.attempted_zk_gas_shadow_report, outcome.zk_gas_shadow_report);
        assert!(matches!(
            outcome
                .zk_gas_shadow_report
                .expect("shadow execution publishes a typed report")
                .status(),
            ZkGasShadowStatus::Incomplete {
                missing: ZkGasIncompleteness::ExactBlockResourcesAndMptSelectorsUnavailable,
                ..
            }
        ));

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
    fn execute_derived_block_reports_pre_existing_destroyed_account_after_refund() {
        let contract = Address::with_last_byte(0x41);
        let beneficiary = Address::with_last_byte(0x42);
        let mut selfdestruct_code = vec![opcode::PUSH20];
        selfdestruct_code.extend_from_slice(beneficiary.as_slice());
        selfdestruct_code.push(opcode::SELFDESTRUCT);

        let chain_spec = Arc::new(
            TAIKO_DEVNET
                .as_ref()
                .clone_with_devnet_unzen_timestamp(1_000_000)
                .expect("devnet supports a pre-Unzen test window"),
        );
        let chain_id = chain_spec.inner.chain().id();
        let config = TaikoEvmConfig::new_with_zk_gas_mode(chain_spec, ZkGasMode::Disabled);
        let parent_header = SealedHeader::seal_slow(Header::default());
        let destroy = test_transaction_with_value(chain_id, 0, contract, U256::ZERO);
        let refund = test_transaction_with_value(chain_id, 1, contract, U256::from(1_u64));
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
                    transactions: vec![destroy.clone_inner(), refund.clone_inner()],
                    ommers: Default::default(),
                    withdrawals: None,
                },
            },
            vec![destroy.signer(), refund.signer()],
        );
        let mut db = db_with_contracts(&[(TEST_CALLER, 0)]);
        insert_contract(&mut db, contract, Bytecode::new_raw(Bytes::from(selfdestruct_code)));
        db.insert_account_storage(contract, U256::ZERO, U256::from(7_u64))
            .expect("pre-existing contract storage inserts");

        let outcome = execute_derived_block(&config, &parent_header, &derived_block, db)
            .expect("pre-Unzen destroy-then-refund block executes");

        assert_eq!(outcome.committed_transactions.len(), 2);
        assert_eq!(outcome.pre_existing_destroyed_accounts, vec![keccak256(contract)]);
    }

    #[test]
    fn shadow_mode_preserves_derived_state_and_assembled_header() {
        fn run(
            mode: ZkGasMode,
            chain_spec: Arc<TaikoChainSpec>,
            parent_header: &SealedHeader,
            derived_block: &RecoveredBlock<Block>,
        ) -> (DerivedBlockExecutionOutcome, RecoveredBlock<Block>) {
            let config = TaikoEvmConfig::new_with_zk_gas_mode(chain_spec, mode);
            let outcome = execute_derived_block(
                &config,
                parent_header,
                derived_block,
                db_with_contracts(&[(TEST_CALLER, 0)]),
            )
            .expect("derived block executes");
            let assembled = assemble_filtered_block(
                &config,
                parent_header,
                derived_block,
                outcome.committed_transactions.clone(),
                &outcome.execution_result,
                outcome.finalized_block_zk_gas,
                B256::with_last_byte(0x55),
            )
            .expect("filtered block assembles");
            (outcome, assembled)
        }

        let chain_spec = Arc::new(unzen_chain_spec());
        let chain_id = chain_spec.inner.chain().id();
        let parent_header = SealedHeader::seal_slow(Header::default());
        let anchor_transaction = test_transaction(chain_id, 0);
        let regular_transaction = test_transaction(chain_id, 1);
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
                        regular_transaction.clone_inner(),
                    ],
                    ommers: Default::default(),
                    withdrawals: None,
                },
            },
            vec![anchor_transaction.signer(), regular_transaction.signer()],
        );

        let (disabled, disabled_block) =
            run(ZkGasMode::Disabled, chain_spec.clone(), &parent_header, &derived_block);
        let (shadow, shadow_block) =
            run(ZkGasMode::Shadow, chain_spec, &parent_header, &derived_block);

        assert!(disabled.zk_gas_shadow_report.is_none());
        assert!(disabled.attempted_zk_gas_shadow_report.is_none());
        assert_eq!(disabled.attempted_transaction_count, 0);
        assert!(matches!(
            shadow.zk_gas_shadow_report.expect("shadow report").status(),
            ZkGasShadowStatus::Incomplete {
                missing: ZkGasIncompleteness::ExactBlockResourcesAndMptSelectorsUnavailable,
                ..
            }
        ));
        assert_eq!(shadow.committed_transactions, disabled.committed_transactions);
        assert_eq!(shadow.execution_result.receipts, disabled.execution_result.receipts);
        assert_eq!(shadow.execution_result.requests, disabled.execution_result.requests);
        assert_eq!(shadow.execution_result.gas_used, disabled.execution_result.gas_used);
        assert_eq!(shadow.execution_result.blob_gas_used, disabled.execution_result.blob_gas_used);
        assert_eq!(shadow.hashed_state, disabled.hashed_state);
        assert_eq!(shadow.finalized_block_zk_gas, disabled.finalized_block_zk_gas);
        assert_eq!(shadow_block.header(), disabled_block.header());
        assert_eq!(shadow_block.body(), disabled_block.body());
        assert_eq!(shadow_block.header().state_root, B256::with_last_byte(0x55));
    }
}
