//! Taiko block executor integrating anchor pre-execution and tx filtering.
use alloy_consensus::{Transaction, TransactionEnvelope, TxReceipt};
use alloy_eips::{Encodable2718, eip7685::Requests};
use alloy_evm::{
    FromRecoveredTx, FromTxWithEncoded, RecoveredTx,
    eth::{EthTxResult, receipt_builder::ReceiptBuilder},
};
use alloy_primitives::{Address, Bytes, Log, Uint};
use reth_evm::{
    Evm, OnStateHook,
    block::{
        BlockExecutionError, BlockExecutor, BlockValidationError, ExecutableTx,
        InternalBlockExecutionError, StateChangeSource, StateDB, SystemCaller,
    },
    eth::receipt_builder::ReceiptBuilderCtx,
};
use reth_execution_types::BlockExecutionResult;
use reth_revm::context::{Block as _, result::ResultAndState};
use revm_database_interface::{Database, DatabaseCommit};

use crate::factory::TaikoBlockExecutionCtx;
use alethia_reth_chainspec::spec::TaikoExecutorSpec;
use alethia_reth_evm::{alloy::TAIKO_GOLDEN_TOUCH_ADDRESS, handler::get_treasury_address};
use alethia_reth_primitives::decode_shasta_basefee_sharing_pctg;

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
        let tx_count_hint = ctx.tx_count_hint.unwrap_or_default();
        Self {
            evm,
            ctx,
            receipts: Vec::with_capacity(tx_count_hint),
            gas_used: 0,
            system_caller: SystemCaller::new(spec.clone()),
            spec,
            receipt_builder,
            evm_extra_execution_ctx_initialized: false,
        }
    }
}

impl<E, Spec, R> BlockExecutor for TaikoBlockExecutor<'_, E, Spec, R>
where
    E: Evm<DB: StateDB, Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>>,
    Spec: TaikoExecutorSpec,
    R: ReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt<Log = Log>>,
{
    /// Input transaction type.
    type Transaction = R::Transaction;
    /// Receipt type this executor produces.
    type Receipt = R::Receipt;
    /// EVM used by the executor.
    type Evm = E;
    /// Result of transaction execution.
    type Result = EthTxResult<E::HaltReason, <R::Transaction as TransactionEnvelope>::TxType>;

    /// Applies any necessary changes before executing the block's transactions.
    /// NOTE: Here we use a system call to set the Anchor transact sender account information and
    /// decode the base fee share percentage from the block's extra data.
    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.system_caller.apply_blockhashes_contract_call(self.ctx.parent_hash, &mut self.evm)?;
        self.system_caller
            .apply_beacon_root_contract_call(self.ctx.parent_beacon_block_root, &mut self.evm)?;

        // Initialize the golden touch address nonce if it is not already set.
        if !self.evm_extra_execution_ctx_initialized {
            let account_info =
                self.evm.db_mut().basic(Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS)).map_err(
                    |e| BlockExecutionError::Internal(InternalBlockExecutionError::Other(e.into())),
                )?;

            // Decode the base fee share percentage from the block's extra data.
            let base_fee_share_pgtg =
                if self.spec.is_shasta_active(self.evm.block().timestamp().to()) {
                    decode_shasta_basefee_sharing_pctg(self.ctx.extra_data.as_ref()) as u64
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

    /// Executes a transaction and returns the resulting state diff without persisting it; the
    /// caller decides whether to commit.
    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        let (tx_env, tx) = tx.into_parts();

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

        let result = self
            .evm
            .transact(tx_env)
            .map_err(|err| BlockExecutionError::evm(err, tx.tx().trie_hash()))?;

        Ok(EthTxResult {
            result,
            blob_gas_used: tx.tx().blob_gas_used().unwrap_or_default(),
            tx_type: tx.tx().tx_type(),
        })
    }

    /// Commits a previously executed transaction: updates receipts, gas accounting, and writes the
    /// buffered state changes to the database.
    fn commit_transaction(&mut self, output: Self::Result) -> Result<u64, BlockExecutionError> {
        let EthTxResult { result: ResultAndState { result, state }, tx_type, .. } = output;

        self.system_caller.on_state(StateChangeSource::Transaction(self.receipts.len()), &state);

        let gas_used = result.gas_used();

        // append gas used
        self.gas_used += gas_used;

        // Push transaction changeset and calculate header bloom filter for receipt.
        self.receipts.push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
            tx_type,
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

    /// Returns a reference to all recorded receipts.
    fn receipts(&self) -> &[Self::Receipt] {
        &self.receipts
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
            let (tx_env, tx) = tx.into_parts();
            // Check transaction signature at first, if invalid, skip it directly.
            if !is_anchor_transaction && *tx.signer() == Address::ZERO {
                continue;
            }
            // Execute transaction, if invalid, skip it directly.
            self.execute_transaction((tx_env, tx)).map(|_| ()).or_else(|err| match err {
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

/// Encode anchor pre-execution context for the treasury system call.
///
/// The payload is a fixed 16-byte big-endian blob (`u64 base_fee_share_pctg || u64 caller_nonce`)
/// that the EVM-side system-call hook decodes before transaction execution.
fn encode_anchor_system_call_data(base_fee_share_pctg: u64, caller_nonce: u64) -> Bytes {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&base_fee_share_pctg.to_be_bytes());
    buf[8..].copy_from_slice(&caller_nonce.to_be_bytes());
    Bytes::copy_from_slice(&buf)
}

/// Decode post-Ontake `extra_data` into the configured base-fee sharing percentage.
///
/// Ontake+ blocks store the percentage as a `uint256`; only the low 64 bits are consumed here.
fn decode_post_ontake_extra_data(extradata: Bytes) -> u64 {
    let value = Uint::<256, 4>::from_be_slice(&extradata);
    value.as_limbs()[0]
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use alloy_consensus::{Signed, TxLegacy};
    use alloy_evm::EvmFactory;
    use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U64, U256};
    use reth_ethereum_primitives::TransactionSigned;
    use reth_evm::{ConfigureEvm, block::BlockExecutor};
    use reth_evm_ethereum::RethReceiptBuilder;
    use reth_primitives_traits::SignedTransaction;
    use reth_revm::{
        db::{CacheDB, EmptyDB},
        state::AccountInfo,
    };

    use alethia_reth_evm::alloy::decode_anchor_system_call_data;

    use super::*;
    use crate::{
        config::{TaikoEvmConfig, TaikoNextBlockEnvAttributes},
        factory::TaikoBlockExecutionCtx,
    };
    use alethia_reth_chainspec::spec::TaikoChainSpec;

    #[test]
    fn test_encode_anchor_system_call_data() {
        let base_fee_share_pctg = U64::random().to::<u64>();
        let caller_nonce = U64::random().to::<u64>();
        let encoded_data = encode_anchor_system_call_data(base_fee_share_pctg, caller_nonce);
        assert_eq!(encoded_data.len(), 16);
        assert_eq!(&encoded_data[..8], &base_fee_share_pctg.to_be_bytes());
        assert_eq!(&encoded_data[8..], &caller_nonce.to_be_bytes());

        let (decoded_pctg, decoded_nonce) =
            decode_anchor_system_call_data(&encoded_data).expect("decoding should succeed");
        assert_eq!(decoded_pctg, base_fee_share_pctg);
        assert_eq!(decoded_nonce, caller_nonce);
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

    #[test]
    fn test_apply_pre_execution_changes_initializes_anchor_context_from_account_nonce() {
        let chain_spec = Arc::new(TaikoChainSpec::default());
        let config = TaikoEvmConfig::new(chain_spec.clone());
        let golden_touch = Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS);
        let treasury = get_treasury_address(chain_spec.inner.chain().id());
        let nonce = 7;

        let make_executor = || {
            let mut db = CacheDB::<EmptyDB>::default();
            db.insert_account_info(
                golden_touch,
                AccountInfo { nonce, balance: U256::ZERO, ..Default::default() },
            );

            let evm_env = config
                .next_evm_env(
                    &alloy_consensus::Header::default(),
                    &TaikoNextBlockEnvAttributes {
                        timestamp: 1,
                        suggested_fee_recipient: Address::ZERO,
                        prev_randao: B256::ZERO,
                        gas_limit: 30_000_000,
                        extra_data: Bytes::new(),
                        base_fee_per_gas: 1,
                    },
                )
                .expect("next block env should build");
            let evm = config.evm_factory.create_evm(db, evm_env);

            TaikoBlockExecutor::new(
                evm,
                TaikoBlockExecutionCtx {
                    parent_hash: B256::ZERO,
                    parent_beacon_block_root: None,
                    ommers: &[],
                    withdrawals: None,
                    basefee_per_gas: 1,
                    extra_data: Bytes::new(),
                    tx_count_hint: Some(1),
                },
                chain_spec.clone(),
                RethReceiptBuilder::default(),
            )
        };

        let tx = TxLegacy {
            chain_id: Some(ChainId::from(chain_spec.inner.chain().id())),
            nonce,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(treasury),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let signature = Signature::new(U256::from(1), U256::from(2), false);
        let signed: TransactionSigned = Signed::new_unchecked(tx, signature, B256::ZERO).into();
        let anchor_tx = signed.with_signer(golden_touch);

        let mut executor_without_init = make_executor();
        assert!(
            executor_without_init.execute_transaction(anchor_tx.clone()).is_err(),
            "without pre-execution initialization, the zero-balance golden-touch tx must not be treated as anchor"
        );

        let mut executor_with_init = make_executor();
        executor_with_init
            .apply_pre_execution_changes()
            .expect("pre-execution changes should succeed");
        assert!(
            executor_with_init.execute_transaction(anchor_tx).is_ok(),
            "pre-execution initialization should seed anchor detection from the golden-touch account nonce"
        );
    }
}
