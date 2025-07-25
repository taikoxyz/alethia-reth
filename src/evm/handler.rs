use std::str::FromStr;

use reth::revm::{
    Database, Inspector,
    context::{
        Block, Cfg, ContextTr, JournalTr, Transaction,
        result::{HaltReason, InvalidTransaction},
    },
    handler::{
        EvmTr, EvmTrError, FrameResult, Handler, PrecompileProvider,
        instructions::InstructionProvider, pre_execution::validate_account_nonce_and_code,
    },
    inspector::{InspectorEvmTr, InspectorHandler},
    interpreter::{Gas, InterpreterResult, interpreter::EthInterpreter},
    primitives::{Address, U256},
};
use reth_revm::{
    handler::{EthFrame, FrameTr},
    interpreter::interpreter_action::FrameInit,
    state::EvmState,
};
use tracing::debug;

use crate::evm::evm::TaikoEvmExtraExecutionCtx;

/// Handler for Taiko EVM, it implements the `Handler` trait
/// and provides methods to handle the execution of transactions and the
/// reward for the beneficiary.
#[derive(Default, Debug, Clone)]
pub struct TaikoEvmHandler<CTX, ERROR, FRAME> {
    pub _phantom: core::marker::PhantomData<(CTX, ERROR, FRAME)>,
    // This field might will be `None` when during some execution simulation calls
    // like `eth_call`.
    extra_execution_ctx: Option<TaikoEvmExtraExecutionCtx>,
}

impl<CTX, ERROR, FRAME> TaikoEvmHandler<CTX, ERROR, FRAME> {
    /// Creates a new instance of [`TaikoEvmHandler`] with the given extra context.
    pub fn new(ctx: Option<TaikoEvmExtraExecutionCtx>) -> Self {
        Self { _phantom: core::marker::PhantomData, extra_execution_ctx: ctx }
    }
}

/// The implementation of Taiko network transaction execution.
impl<EVM, ERROR, FRAME> Handler for TaikoEvmHandler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<
            Context: ContextTr<Journal: JournalTr<State = EvmState>>,
            Precompiles: PrecompileProvider<EVM::Context, Output = InterpreterResult>,
            Instructions: InstructionProvider<
                Context = EVM::Context,
                InterpreterTypes = EthInterpreter,
            >,
            Frame = FRAME,
        >,
    ERROR: EvmTrError<EVM>,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    /// The EVM type containing Context, Instruction, and Precompiles implementations.
    type Evm = EVM;
    /// The error type returned by this handler.
    type Error = ERROR;
    /// The halt reason type included in the output
    type HaltReason = HaltReason;

    /// Transfers transaction fees to the block beneficiary's account, also transfers the base fee
    /// income to the network treasury and then share the base fee income with the coinbase
    /// based on the configuration.
    fn reward_beneficiary(
        &self,
        _evm: &mut Self::Evm,
        _exec_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        reward_beneficiary(_evm.ctx(), _exec_result.gas_mut(), self.extra_execution_ctx.clone())
            .map_err(From::from)
    }

    #[inline]
    fn reimburse_caller(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        reimburse_caller(evm.ctx(), exec_result.gas(), U256::ZERO, self.extra_execution_ctx.clone())
            .map_err(From::from)
    }

    /// Prepares the EVM state for execution.
    ///
    /// Loads the beneficiary account (EIP-3651: Warm COINBASE) and all accounts/storage from the
    /// access list (EIP-2929).
    ///
    /// Deducts the maximum possible fee from the caller's balance.
    ///
    /// For EIP-7702 transactions, applies the authorization list and delegates successful
    /// authorizations. Returns the gas refund amount from EIP-7702. Authorizations are applied
    /// before execution begins. NOTE: for Anchor transaction, the balance check is disabled, so
    /// the caller's balance is not deducted.
    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<(), Self::Error> {
        validate_against_state_and_deduct_caller(evm.ctx(), self.extra_execution_ctx.clone())
    }
}

/// Trait that extends [`Handler`] with inspection functionality, here we just use the default
/// implementation.
impl<EVM, ERROR> InspectorHandler for TaikoEvmHandler<EVM, ERROR, EthFrame<EthInterpreter>>
where
    EVM: InspectorEvmTr<
            Inspector: Inspector<<<Self as Handler>::Evm as EvmTr>::Context, EthInterpreter>,
            Context: ContextTr<Journal: JournalTr<State = EvmState>>,
            Precompiles: PrecompileProvider<EVM::Context, Output = InterpreterResult>,
            Instructions: InstructionProvider<
                Context = EVM::Context,
                InterpreterTypes = EthInterpreter,
            >,
            Frame = EthFrame<EthInterpreter>,
        >,
    ERROR: EvmTrError<EVM>,
{
    type IT = EthInterpreter;
}

/// Transfers transaction fees to the block beneficiary's account, also transfers the base fee
/// income to the network treasury and then share the base fee income with the coinbase based on the
/// configuration.
#[inline]
fn reward_beneficiary<CTX: ContextTr>(
    context: &mut CTX,
    gas: &mut Gas,
    extra_execution_ctx: Option<TaikoEvmExtraExecutionCtx>,
) -> Result<(), <CTX::Db as Database>::Error> {
    let block_number = context.block().number();
    let beneficiary = context.block().beneficiary();
    let basefee = context.block().basefee() as u128;
    let effective_gas_price = context.tx().effective_gas_price(basefee);
    let coinbase_gas_price = effective_gas_price.saturating_sub(basefee);
    // Get the caller address and nonce from the transaction, to check if it is an anchor
    // transaction.
    let tx_caller: Address = context.tx().caller();
    let tx_nonce = context.tx().nonce();
    // Reward beneficiary.
    context.journal_mut().balance_incr(
        beneficiary,
        U256::from(coinbase_gas_price * gas.spent().saturating_sub(gas.refunded() as u64) as u128),
    )?;

    // If the extra execution context is provided, which means we are building an L2 block,
    // we share the base fee income with the coinbase and treasury.
    if let Some(ctx) = extra_execution_ctx {
        debug!(
            target: "taiko_evm", "Rewarding beneficiary, sender account: {:?} nonce: {:?} at block: {:?}",
            tx_caller, tx_nonce, block_number
        );

        // If the transaction is not an anchor transaction, we share the base fee income with the
        // coinbase and treasury.
        if ctx.anchor_caller_address() != tx_caller || ctx.anchor_caller_nonce() != tx_nonce {
            // Total base fee income.
            let total_fee = U256::from(basefee * (gas.spent() - gas.refunded() as u64) as u128);

            // Share the base fee income with the coinbase and treasury.
            let fee_coinbase =
                total_fee.saturating_mul(U256::from(ctx.basefee_share_pctg())) / U256::from(100u64);
            let fee_treasury = total_fee.saturating_sub(fee_coinbase);

            context.journal_mut().balance_incr(beneficiary, fee_coinbase)?;

            let chain_id = context.cfg().chain_id();
            context.journal_mut().balance_incr(get_treasury_address(chain_id), fee_treasury)?;

            debug!(
                target: "taiko_evm",
                "Share basefee with coinbase: {} and treasury: {}, share percentage: {} at block: {:?}",
                fee_coinbase, fee_treasury, ctx.basefee_share_pctg(), block_number
            );
        } else {
            // If the transaction is an anchor transaction, we do not share the base fee income.
            debug!(
                target: "taiko_evm", "Anchor transaction detected, no rewrad, sender account: {:?} nonce: {:?} at block: {:?}",
                tx_caller, tx_nonce, block_number
            );
        }
    }

    Ok(())
}

/// Prepares the EVM state for execution.
/// NOTE: for Anchor transaction, the balance check is disabled, so the caller's balance is not
/// deducted.
#[inline]
pub fn validate_against_state_and_deduct_caller<
    CTX: ContextTr,
    ERROR: From<InvalidTransaction> + From<<CTX::Db as Database>::Error>,
>(
    context: &mut CTX,
    extra_execution_ctx: Option<TaikoEvmExtraExecutionCtx>,
) -> Result<(), ERROR> {
    let basefee = context.block().basefee() as u128;
    let blob_price = context.block().blob_gasprice().unwrap_or_default();
    let is_balance_check_disabled = context.cfg().is_balance_check_disabled();
    let is_eip3607_disabled = context.cfg().is_eip3607_disabled();
    let is_nonce_check_disabled = context.cfg().is_nonce_check_disabled();
    let block = context.block().number();

    let (tx, journal) = context.tx_journal_mut();

    // If the extra execution context is provided, which means we are building an L2 block,
    // we skip the balance check for the anchor transaction.
    debug!(target: "taiko_evm", "Validating state, sender account: {:?} nonce: {:?} at block: {:?}", tx.caller(), tx.nonce(), block);

    // Load caller's account.
    let caller_account = journal.load_account_code(tx.caller())?.data;

    validate_account_nonce_and_code(
        &mut caller_account.info,
        tx.nonce(),
        is_eip3607_disabled,
        is_nonce_check_disabled,
    )?;

    let is_anchor_transaction = extra_execution_ctx.as_ref().is_some_and(|ctx| {
        ctx.anchor_caller_address() == tx.caller() && ctx.anchor_caller_nonce() == tx.nonce()
    });

    // If the transaction is an anchor transaction, we disable the balance check.
    if is_anchor_transaction {
        debug!(target: "taiko_evm", "Anchor transaction detected, disabling balance check, sender account: {:?} nonce: {:?} at block: {:?}", tx.caller(), tx.nonce(), block);
    } else {
        debug!(target: "taiko_evm", "Anchor transaction not detected, balance check enabled, sender account: {:?} nonce: {:?} at block: {:?}", tx.caller(), tx.nonce(), block);
    }

    // Bump the nonce for calls. Nonce for CREATE will be bumped in `make_create_frame`.
    if tx.kind().is_call() {
        // Nonce is already checked
        caller_account.info.nonce = caller_account.info.nonce.saturating_add(1);
    }

    let max_balance_spending =
        if is_anchor_transaction { U256::ZERO } else { tx.max_balance_spending()? };

    // Check if account has enough balance for `gas_limit * max_fee`` and value transfer.
    // Transfer will be done inside `*_inner` functions.
    if max_balance_spending > caller_account.info.balance && !is_balance_check_disabled {
        return Err(InvalidTransaction::LackOfFundForMaxFee {
            fee: Box::new(max_balance_spending),
            balance: Box::new(caller_account.info.balance),
        }
        .into());
    }

    let effective_balance_spending = tx
        .effective_balance_spending(basefee, blob_price)
        .expect("effective balance is always smaller than max balance so it can't overflow");

    // subtracting max balance spending with value that is going to be deducted later in the call.
    let gas_balance_spending =
        if is_anchor_transaction { U256::ZERO } else { effective_balance_spending - tx.value() };

    let mut new_balance = caller_account.info.balance.saturating_sub(gas_balance_spending);

    if is_balance_check_disabled {
        // Make sure the caller's balance is at least the value of the transaction.
        new_balance = new_balance.max(tx.value());
    }

    let old_balance = caller_account.info.balance;
    // Touch account so we know it is changed.
    caller_account.mark_touch();
    caller_account.info.balance = new_balance;

    journal.caller_accounting_journal_entry(tx.caller(), old_balance, tx.kind().is_call());
    Ok(())
}

/// Reimburses the caller for unused gas.
#[inline]
pub fn reimburse_caller<CTX: ContextTr>(
    context: &mut CTX,
    gas: &Gas,
    additional_refund: U256,
    extra_execution_ctx: Option<TaikoEvmExtraExecutionCtx>,
) -> Result<(), <CTX::Db as Database>::Error> {
    let basefee = context.block().basefee() as u128;
    let caller = context.tx().caller();
    let effective_gas_price = context.tx().effective_gas_price(basefee);

    if let Some(ctx) = extra_execution_ctx {
        if ctx.anchor_caller_address() == context.tx().caller() &&
            ctx.anchor_caller_nonce() == context.tx().nonce()
        {
            debug!(
                target: "taiko_evm",
                "Anchor transaction detected, no reimbursement, sender account: {:?} nonce: {:?}",
                caller,
                context.tx().nonce()
            );
            return Ok(());
        }
    }

    debug!(
        target: "taiko_evm",
        "Reimbursing caller, sender account: {:?} nonce: {:?}, gas remaining: {}, gas refunded: {}, additional refund: {}",
        caller,
        context.tx().nonce(),
        gas.remaining(),
        gas.refunded(),
        additional_refund
    );

    // Return balance of not spend gas.
    context.journal_mut().balance_incr(
        caller,
        U256::from(
            effective_gas_price.saturating_mul((gas.remaining() + gas.refunded() as u64) as u128),
        ) + additional_refund,
    )?;

    Ok(())
}

/// Generates the network treasury address based on the chain ID.
#[inline]
pub fn get_treasury_address(chain_id: u64) -> Address {
    let prefix = chain_id.to_string();
    let suffix = "10001";

    let total_len = 40;
    let padding_len = total_len - prefix.len() - suffix.len();
    let padding = "0".repeat(padding_len);

    let hex_str = format!("0x{prefix}{padding}{suffix}");

    Address::from_str(&hex_str).unwrap()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_get_treasury_address() {
        let treasury = get_treasury_address(167000);
        assert_eq!(
            treasury,
            Address::from_str("0x1670000000000000000000000000000000010001").unwrap()
        );
    }
}
