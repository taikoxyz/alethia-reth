use alloy_consensus::{Transaction, TxReceipt};
use alloy_eips::{Encodable2718, eip7685::Requests};
use alloy_evm::{
    Database, FromRecoveredTx, FromTxWithEncoded, eth::receipt_builder::ReceiptBuilder,
};
use alloy_primitives::{Address, Bytes, Uint};
use reth::{
    primitives::Log,
    revm::{
        State,
        context::{
            Block as _,
            result::{ExecutionResult, ResultAndState},
        },
    },
};
use reth_evm::{
    Evm, OnStateHook,
    block::{
        BlockExecutionError, BlockExecutor, BlockValidationError, CommitChanges, ExecutableTx,
        InternalBlockExecutionError, StateChangeSource, SystemCaller,
    },
    eth::receipt_builder::ReceiptBuilderCtx,
};
use reth_provider::BlockExecutionResult;
use revm_database_interface::DatabaseCommit;

use crate::factory::TaikoBlockExecutionCtx;
use alethia_reth_chainspec::spec::TaikoExecutorSpec;
use alethia_reth_evm::{
    alloy::TAIKO_GOLDEN_TOUCH_ADDRESS,
    handler::get_treasury_address,
};

/// Block executor for Taiko network.
pub struct TaikoBlockExecutor<'a, Evm, Spec, R: ReceiptBuilder> {
    /// Reference to the specification object.
    spec: Spec,

    /// Context for block execution.
    pub ctx: TaikoBlockExecutionCtx<'a>,
    /// Inner EVM.
    evm: Evm,
    /// Utility to call system smart contracts.
    system_caller: SystemCaller<Spec>,
    /// Receipt builder.
    receipt_builder: R,

    /// Receipts of executed transactions.
    receipts: Vec<R::Receipt>,
    /// Total gas used by transactions in this block.
    gas_used: u64,
    /// Flag indicating whether the executor has been initialized with the anchor transaction info
    /// in `apply_pre_execution_changes`.
    evm_extra_execution_ctx_initialized: bool,
}

impl<'a, Evm, Spec, R> TaikoBlockExecutor<'a, Evm, Spec, R>
where
    Spec: Clone,
    R: ReceiptBuilder,
{
    /// Creates a new [`TaikoBlockExecutor`]
    pub fn new(evm: Evm, ctx: TaikoBlockExecutionCtx<'a>, spec: Spec, receipt_builder: R) -> Self {
        Self {
            evm,
            ctx,
            receipts: Vec::new(),
            gas_used: 0,
            system_caller: SystemCaller::new(spec.clone()),
            spec,
            receipt_builder,
            evm_extra_execution_ctx_initialized: false,
        }
    }
}

impl<'db, DB, E, Spec, R> BlockExecutor for TaikoBlockExecutor<'_, E, Spec, R>
where
    DB: Database + 'db,
    E: Evm<
            DB = &'db mut State<DB>,
            Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
        >,
    Spec: TaikoExecutorSpec,
    R: ReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt<Log = Log>>,
{
    /// Input transaction type.
    type Transaction = R::Transaction;
    /// Receipt type this executor produces.
    type Receipt = R::Receipt;
    /// EVM used by the executor.
    type Evm = E;

    /// Applies any necessary changes before executing the block's transactions.
    /// NOTE: Here we use a system call to set the Anchor transact sender account information and
    /// decode the base fee share percentage from the block's extra data.
    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        // Set state clear flag if the block is after the Spurious Dragon hardfork.
        let state_clear_flag =
            self.spec.is_spurious_dragon_active_at_block(self.evm.block().number().to());
        self.evm.db_mut().set_state_clear_flag(state_clear_flag);

        self.system_caller.apply_blockhashes_contract_call(self.ctx.parent_hash, &mut self.evm)?;
        self.system_caller
            .apply_beacon_root_contract_call(self.ctx.parent_beacon_block_root, &mut self.evm)?;

        // Initialize the golden touch address nonce if it is not already set.
        if !self.evm_extra_execution_ctx_initialized {
            let account_info = self
                .evm
                .db_mut()
                .load_cache_account(Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS))
                .map_err(|e| {
                    BlockExecutionError::Internal(InternalBlockExecutionError::Other(e.into()))
                })?
                .account_info();

            // Decode the base fee share percentage from the block's extra data.
            let base_fee_share_pgtg =
                if self.spec.is_shasta_active(self.evm.block().timestamp().to()) {
                    let (pctg, _) = decode_post_shasta_extra_data(self.ctx.extra_data.clone());
                    pctg
                } else if self.spec.is_ontake_active_at_block(self.evm.block().number().to()) {
                    decode_post_ontake_extra_data(self.ctx.extra_data.clone())
                } else {
                    0
                };

            self.evm
                .transact_system_call(
                    Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS),
                    get_treasury_address(self.evm().chain_id()),
                    encode_anchor_system_call_data(
                        base_fee_share_pgtg,
                        account_info.map_or(0, |account| account.nonce),
                    ),
                )
                .map_err(|e| {
                    BlockExecutionError::Internal(InternalBlockExecutionError::Other(e.into()))
                })?;

            self.evm_extra_execution_ctx_initialized = true;
        }

        Ok(())
    }

    /// Executes a single transaction and applies execution result to internal state. Invokes the
    /// given closure with an internal [`ExecutionResult`] produced by the EVM, and commits the
    /// transaction to the state on [`CommitChanges::Yes`].
    ///
    /// Returns [`None`] if transaction was skipped via [`CommitChanges::No`].
    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx<Self>,
        f: impl FnOnce(&ExecutionResult<<Self::Evm as Evm>::HaltReason>) -> CommitChanges,
    ) -> Result<Option<u64>, BlockExecutionError> {
        // The sum of the transaction's gas limit, Tg, and the gas utilized in this block prior,
        // must be no greater than the block's gasLimit.
        let block_available_gas = self.evm.block().gas_limit() - self.gas_used;

        if tx.tx().gas_limit() > block_available_gas {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: tx.tx().gas_limit(),
                block_available_gas,
            }
            .into());
        }

        let output = self.execute_transaction_without_commit(&tx)?;

        if !f(&output.result).should_commit() {
            return Ok(None);
        }

        let gas_used = self.commit_transaction(output, tx)?;
        Ok(Some(gas_used))
    }

    /// Executes a transaction and returns the resulting state diff without persisting it; the
    /// caller decides whether to commit.
    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<ResultAndState<<Self::Evm as Evm>::HaltReason>, BlockExecutionError> {
        // Run the transaction but leave state buffered so the caller can decide whether to commit.
        self.evm.transact(&tx).map_err(|err| BlockExecutionError::evm(err, tx.tx().trie_hash()))
    }

    /// Commits a previously executed transaction: updates receipts, gas accounting, and writes the
    /// buffered state changes to the database.
    fn commit_transaction(
        &mut self,
        output: ResultAndState<<Self::Evm as Evm>::HaltReason>,
        tx: impl ExecutableTx<Self>,
    ) -> Result<u64, BlockExecutionError> {
        let ResultAndState { result, state } = output;

        self.system_caller.on_state(StateChangeSource::Transaction(self.receipts.len()), &state);

        let gas_used = result.gas_used();

        // append gas used
        self.gas_used += gas_used;

        // Push transaction changeset and calculate header bloom filter for receipt.
        self.receipts.push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
            tx: tx.tx(),
            evm: &self.evm,
            result,
            state: &state,
            cumulative_gas_used: self.gas_used,
        }));

        // Commit the state changes.
        self.evm.db_mut().commit(state);

        Ok(gas_used)
    }

    /// Applies any necessary changes after executing the block's transactions, completes execution
    /// and returns the underlying EVM along with execution result.
    fn finish(self) -> Result<(Self::Evm, BlockExecutionResult<R::Receipt>), BlockExecutionError> {
        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests: Requests::default(),
                gas_used: self.gas_used,
                blob_gas_used: 0,
            },
        ))
    }

    /// Sets a hook to be called after each state change during execution.
    fn set_state_hook(&mut self, hook: Option<Box<dyn OnStateHook>>) {
        self.system_caller.with_state_hook(hook);
    }

    /// Exposes mutable reference to EVM.
    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    /// Exposes immutable reference to EVM.
    fn evm(&self) -> &Self::Evm {
        &self.evm
    }

    /// Executes all transactions in a block, applying pre and post execution changes.
    /// NOTE: For proving system, we skip the invalid transactions directly inside this function.
    #[cfg(feature = "prover")]
    fn execute_block(
        mut self,
        transactions: impl IntoIterator<Item = impl ExecutableTx<Self>>,
    ) -> Result<BlockExecutionResult<Self::Receipt>, BlockExecutionError>
    where
        Self: Sized,
    {
        self.apply_pre_execution_changes()?;

        for (idx, tx) in transactions.into_iter().enumerate() {
            let is_anchor_transaction = idx == 0;
            // Check transaction signature at first, if invalid, skip it directly.
            if !is_anchor_transaction && *tx.signer() == Address::ZERO {
                continue;
            }
            // Execute transaction, if invalid, skip it directly.
            self.execute_transaction(tx).map(|_| ()).or_else(|err| match err {
                BlockExecutionError::Validation(BlockValidationError::InvalidTx { .. }) |
                BlockExecutionError::Validation(
                    BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. },
                ) if !is_anchor_transaction => Ok(()),
                _ => Err(err),
            })?;
        }

        self.apply_post_execution_changes()
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{TaikoEvmConfig, TaikoNextBlockEnvAttributes};
    use alethia_reth_chainspec::spec::TaikoChainSpec;
    use alethia_reth_evm::jumpdest_limiter::{
        DEFAULT_BLOCK_JUMPDEST_LIMIT, DEFAULT_TX_JUMPDEST_LIMIT, LimitingInspector,
    };
    use alethia_reth_evm::zk_gas::JUMPDEST_ZK_GAS;
    use alloy_consensus::{Header, Signed, TxLegacy};
    use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
    use reth::{
        primitives::Recovered,
        revm::{
            bytecode::Bytecode,
            db::InMemoryDB,
            inspector::NoOpInspector,
            primitives::KECCAK_EMPTY,
            state::AccountInfo,
            State,
        },
    };
    use reth_ethereum::TransactionSigned;
    use reth_evm::{ConfigureEvm, execute::BlockBuilder};
    use reth_primitives_traits::SealedHeader;
    use reth_storage_api::noop::NoopProvider;
    use std::sync::{Arc, atomic::AtomicU64};

    fn create_bytecode_with_jumpdests(count: usize) -> Bytes {
        let mut code = vec![0x5b; count];
        code.push(0x00); // STOP to end execution
        Bytes::from(code)
    }

    fn legacy_tx(to: Address, gas_limit: u64, gas_price: u128, chain_id: u64) -> TransactionSigned {
        let tx = TxLegacy {
            chain_id: Some(chain_id),
            nonce: 0,
            gas_price,
            gas_limit,
            to: TxKind::Call(to),
            value: U256::ZERO,
            input: Bytes::default(),
        };
        let signature = Signature::new(U256::ZERO, U256::ZERO, false);
        let signed = Signed::new_unhashed(tx, signature);
        signed.into()
    }

    #[test]
    fn test_block_difficulty_contains_zk_gas() {
        let chain_spec = Arc::new(TaikoChainSpec::default());
        let evm_config = TaikoEvmConfig::new(chain_spec.clone());
        let block_gas_limit = 30_000_000u64;
        let base_fee = 1u64;

        let parent_header = Header {
            number: 0,
            gas_limit: block_gas_limit,
            base_fee_per_gas: Some(base_fee),
            timestamp: 1,
            beneficiary: Address::ZERO,
            ..Default::default()
        };
        let parent = SealedHeader::new_unhashed(parent_header);

        let next_ctx = TaikoNextBlockEnvAttributes {
            timestamp: 2,
            suggested_fee_recipient: Address::ZERO,
            prev_randao: B256::ZERO,
            gas_limit: block_gas_limit,
            extra_data: Bytes::default(),
            base_fee_per_gas: base_fee,
        };

        let evm_env = evm_config
            .next_evm_env(parent.header(), &next_ctx)
            .expect("evm env");
        let mut ctx = evm_config
            .context_for_next_block(&parent, next_ctx)
            .expect("execution ctx");

        let zk_gas_counter = Arc::new(AtomicU64::new(0));
        ctx.zk_gas_counter = Some(zk_gas_counter.clone());

        let jumpdest_count = 10usize;
        let contract_address = Address::random();
        let sender = Address::random();

        let mut db = InMemoryDB::default();
        let code = create_bytecode_with_jumpdests(jumpdest_count);
        let bytecode = Bytecode::new_raw_checked(code).unwrap();
        db.insert_account_info(
            contract_address,
            AccountInfo {
                balance: U256::from(1_000_000u64),
                nonce: 0,
                code_hash: bytecode.hash_slow(),
                code: Some(bytecode),
            },
        );
        db.insert_account_info(
            sender,
            AccountInfo {
                balance: U256::from(10_000_000u64),
                nonce: 0,
                code_hash: KECCAK_EMPTY,
                code: None,
            },
        );

        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let evm = evm_config.evm_with_env_and_inspector(
            &mut state,
            evm_env,
            LimitingInspector::new(
                DEFAULT_TX_JUMPDEST_LIMIT,
                DEFAULT_BLOCK_JUMPDEST_LIMIT,
                NoOpInspector {},
            )
            .with_zk_gas_counter(zk_gas_counter),
        );

        let mut builder = evm_config.create_block_builder(evm, &parent, ctx);
        builder.apply_pre_execution_changes().expect("pre-execution changes");

        let chain_id = chain_spec.inner.chain().id();
        let tx_signed = legacy_tx(contract_address, 1_000_000, base_fee as u128, chain_id);
        let recovered = Recovered::new_unchecked(tx_signed, sender);
        builder.execute_transaction(recovered).expect("execute tx");

        let outcome = builder.finish(NoopProvider::default()).expect("finish block");
        let header = outcome.block.sealed_block().header();
        let expected = U256::from(jumpdest_count as u64 * JUMPDEST_ZK_GAS);
        assert_eq!(header.difficulty, expected);
    }
}
// Encode the anchor system call data for the Anchor contract sender account information
// and the base fee share percentage.
fn encode_anchor_system_call_data(base_fee_share_pctg: u64, caller_nonce: u64) -> Bytes {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&base_fee_share_pctg.to_be_bytes());
    buf[8..].copy_from_slice(&caller_nonce.to_be_bytes());
    Bytes::copy_from_slice(&buf)
}

// Decode the extra data from the post Ontake block to extract the base fee share percentage,
// which is stored in the first 32 bytes of the extra data.
fn decode_post_ontake_extra_data(extradata: Bytes) -> u64 {
    let value = Uint::<256, 4>::from_be_slice(&extradata);
    value.as_limbs()[0]
}

// Decode the extra data from the Shasta block to extract the base fee sharing percentage and
// designated prover flag.
fn decode_post_shasta_extra_data(extradata: Bytes) -> (u64, bool) {
    let bytes = extradata.as_ref();
    let base_fee_share_pctg = bytes.first().copied().unwrap_or_default() as u64;
    let is_low_bond_proposal = bytes.get(1).map(|b| b & 0x01 != 0).unwrap_or(false);
    (base_fee_share_pctg, is_low_bond_proposal)
}

#[cfg(test)]
mod test {
    use alloy_primitives::{U64, U256};

    use alethia_reth_evm::alloy::decode_anchor_system_call_data;

    use super::*;

    #[test]
    fn test_encode_anchor_system_call_data() {
        let base_fee_share_pctg = U64::random().to::<u64>();
        let caller_nonce = U64::random().to::<u64>();
        let encoded_data = encode_anchor_system_call_data(base_fee_share_pctg, caller_nonce);
        assert_eq!(encoded_data.len(), 16);
        assert_eq!(&encoded_data[..8], &base_fee_share_pctg.to_be_bytes());
        assert_eq!(&encoded_data[8..], &caller_nonce.to_be_bytes());

        let decoded_data = decode_anchor_system_call_data(&encoded_data);
        assert_eq!(decoded_data.unwrap().0, base_fee_share_pctg);
        assert_eq!(decoded_data.unwrap().1, caller_nonce);
    }

    #[test]
    fn test_decode_post_ontake_extra_data() {
        let base_fee_share_pctg = U64::random().to::<u64>();

        assert_eq!(
            decode_post_ontake_extra_data(Bytes::copy_from_slice(
                &U256::from_limbs([base_fee_share_pctg, 0, 0, 0]).to_be_bytes::<32>(),
            )),
            base_fee_share_pctg
        );
    }
}
