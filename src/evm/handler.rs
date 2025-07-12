use std::str::FromStr;

use reth::revm::{
    Database, Inspector,
    context::{
        Block, Cfg, ContextTr, JournalOutput, JournalTr, Transaction,
        result::{HaltReason, InvalidTransaction},
    },
    handler::{
        EvmTr, EvmTrError, Frame, FrameResult, Handler, PrecompileProvider,
        instructions::InstructionProvider, pre_execution::validate_account_nonce_and_code,
    },
    inspector::{InspectorEvmTr, InspectorFrame, InspectorHandler},
    interpreter::{FrameInput, Gas, InterpreterResult, interpreter::EthInterpreter},
    primitives::{Address, U256},
};
use tracing::debug;

use crate::evm::evm::TaikoEvmExtraExecutionCtx;

/// Handler for Taiko EVM, it implements the `Handler` trait
/// and provides methods to handle the execution of transactions and the
/// reward for the beneficiary.
#[derive(Default, Debug, Clone)]
pub struct TaikoEvmHandler<CTX, ERROR, FRAME> {
    pub _phantom: core::marker::PhantomData<(CTX, ERROR, FRAME)>,
    extra_execution_ctx: Option<TaikoEvmExtraExecutionCtx>,
}

impl<CTX, ERROR, FRAME> TaikoEvmHandler<CTX, ERROR, FRAME> {
    /// Creates a new instance of [`TaikoEvmHandler`] with the given extra context.
    pub fn new(ctx: Option<TaikoEvmExtraExecutionCtx>) -> Self {
        Self {
            _phantom: core::marker::PhantomData,
            extra_execution_ctx: ctx,
        }
    }
}

/// The implementation of Taiko network transaction execution.
impl<EVM, ERROR, FRAME> Handler for TaikoEvmHandler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<
            Context: ContextTr<Journal: JournalTr<FinalOutput = JournalOutput>>,
            Precompiles: PrecompileProvider<EVM::Context, Output = InterpreterResult>,
            Instructions: InstructionProvider<
                Context = EVM::Context,
                InterpreterTypes = EthInterpreter,
            >,
        >,
    ERROR: EvmTrError<EVM>,
    FRAME: Frame<Evm = EVM, Error = ERROR, FrameResult = FrameResult, FrameInit = FrameInput>,
{
    /// The EVM type containing Context, Instruction, and Precompiles implementations.
    type Evm = EVM;
    /// The error type returned by this handler.
    type Error = ERROR;
    /// The Frame type containing data for frame execution. Supports Call, Create and EofCreate frames.
    type Frame = FRAME;
    /// The halt reason type included in the output
    type HaltReason = HaltReason;

    /// Transfers transaction fees to the block beneficiary's account, also transfers the base fee income
    /// to the network treasury and then share the base fee income with the coinbase based on the
    /// configuration.
    fn reward_beneficiary(
        &self,
        _evm: &mut Self::Evm,
        _exec_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        let extra_ctx = self.extra_execution_ctx.clone().unwrap_or_default();
        reward_beneficiary(
            _evm.ctx(),
            _exec_result.gas_mut(),
            extra_ctx.basefee_share_pctg(),
            extra_ctx.anchor_caller_address(),
            extra_ctx.anchor_caller_nonce(),
        )
        .map_err(From::from)
    }

    /// Prepares the EVM state for execution.
    ///
    /// Loads the beneficiary account (EIP-3651: Warm COINBASE) and all accounts/storage from the access list (EIP-2929).
    ///
    /// Deducts the maximum possible fee from the caller's balance.
    ///
    /// For EIP-7702 transactions, applies the authorization list and delegates successful authorizations.
    /// Returns the gas refund amount from EIP-7702. Authorizations are applied before execution begins.
    /// NOTE: for Anchor transaction, the balance check is disabled, so the caller's balance is not deducted.
    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<(), Self::Error> {
        let extra_ctx = self.extra_execution_ctx.clone().unwrap_or_default();
        validate_against_state_and_deduct_caller(
            evm.ctx(),
            extra_ctx.anchor_caller_address(),
            extra_ctx.anchor_caller_nonce(),
        )
    }
}

/// Trait that extends [`Handler`] with inspection functionality, here we just use the default implementation.
impl<EVM, ERROR, FRAME> InspectorHandler for TaikoEvmHandler<EVM, ERROR, FRAME>
where
    EVM: InspectorEvmTr<
            Inspector: Inspector<<<Self as Handler>::Evm as EvmTr>::Context, EthInterpreter>,
            Context: ContextTr<Journal: JournalTr<FinalOutput = JournalOutput>>,
            Precompiles: PrecompileProvider<EVM::Context, Output = InterpreterResult>,
            Instructions: InstructionProvider<
                Context = EVM::Context,
                InterpreterTypes = EthInterpreter,
            >,
        >,
    ERROR: EvmTrError<EVM>,
    FRAME: Frame<Evm = EVM, Error = ERROR, FrameResult = FrameResult, FrameInit = FrameInput>
        + InspectorFrame<IT = EthInterpreter>,
{
    type IT = EthInterpreter;
}

/// Transfers transaction fees to the block beneficiary's account, also transfers the base fee income
/// to the network treasury and then share the base fee income with the coinbase based on the
/// configuration.
#[inline]
fn reward_beneficiary<CTX: ContextTr>(
    context: &mut CTX,
    gas: &mut Gas,
    basefee_share_pctg: u64,
    anchor_caller_address: Address,
    anchor_caller_nonce: u64,
) -> Result<(), <CTX::Db as Database>::Error> {
    let block = context.block();
    let tx = context.tx();
    let beneficiary = block.beneficiary();
    let basefee = block.basefee() as u128;
    let effective_gas_price = tx.effective_gas_price(basefee);
    // Transfer fee to coinbase/beneficiary.
    // EIP-1559 discard basefee for coinbase transfer. Basefee amount of gas is discarded.
    let coinbase_gas_price = effective_gas_price.saturating_sub(basefee);
    let spent = gas.spent();
    let refunded = gas.refunded();
    // Get the caller address and nonce from the transaction, to check if it is an anchor transaction.
    let tx_caller: Address = tx.caller();
    let tx_nonce = tx.nonce();
    let block_number = context.block().number();
    let coinbase_account = context.journal().load_account(beneficiary)?;
    coinbase_account.data.mark_touch();
    coinbase_account.data.info.balance =
        coinbase_account
            .data
            .info
            .balance
            .saturating_add(U256::from(
                coinbase_gas_price * (gas.spent() - gas.refunded() as u64) as u128,
            ));

    debug!(
        target: "taiko-evm", "Rewarding beneficiary, sender account: {:?} nonce: {:?} at block: {:?}",
        tx_caller, tx_nonce, block_number
    );

    // If the transaction is not an anchor transaction, we share the base fee income with the coinbase and treasury.
    if anchor_caller_address != tx_caller || anchor_caller_nonce != tx_nonce {
        // Total base fee income.
        let total_fee = U256::from(basefee * (spent - refunded as u64) as u128);

        // Share the base fee income with the coinbase and treasury.
        let fee_coinbase =
            total_fee.saturating_mul(U256::from(basefee_share_pctg)) / U256::from(100u64);
        let fee_treasury = total_fee.saturating_sub(fee_coinbase);
        coinbase_account.data.info.balance = coinbase_account
            .data
            .info
            .balance
            .saturating_add(fee_coinbase);

        let chain_id = context.cfg().chain_id();
        let treasury_account = context
            .journal()
            .load_account(get_treasury_address(chain_id))?;

        treasury_account.data.mark_touch();
        treasury_account.data.info.balance = treasury_account
            .data
            .info
            .balance
            .saturating_add(fee_treasury);
        debug!(
            target: "taiko-evm",
            "Share basefee with coinbase: {} and treasury: {}, share percentage: {} at block: {:?}",
            fee_coinbase, fee_treasury, basefee_share_pctg, block_number
        );
    } else {
        // If the transaction is an anchor transaction, we do not share the base fee income.
        debug!(
            target: "taiko-evm", "Anchor transaction detected, no rewrad, sender account: {:?} nonce: {:?} at block: {:?}",
            tx_caller, tx_nonce, block_number
        );
    }
    Ok(())
}

/// Prepares the EVM state for execution.
/// NOTE: for Anchor transaction, the balance check is disabled, so the caller's balance is not deducted.
#[inline]
pub fn validate_against_state_and_deduct_caller<
    CTX: ContextTr,
    ERROR: From<InvalidTransaction> + From<<CTX::Db as Database>::Error>,
>(
    context: &mut CTX,
    anchor_caller_address: Address,
    anchor_caller_nonce: u64,
) -> Result<(), ERROR> {
    let basefee = context.block().basefee() as u128;
    let blob_price = context.block().blob_gasprice().unwrap_or_default();
    let mut is_balance_check_disabled = context.cfg().is_balance_check_disabled();
    let is_eip3607_disabled = context.cfg().is_eip3607_disabled();
    let is_nonce_check_disabled = context.cfg().is_nonce_check_disabled();
    let block = context.block().number();

    let (tx, journal) = context.tx_journal();

    // Load caller's account.
    let caller_account = journal.load_account_code(tx.caller())?.data;

    validate_account_nonce_and_code(
        &mut caller_account.info,
        tx.nonce(),
        tx.kind().is_call(),
        is_eip3607_disabled,
        is_nonce_check_disabled,
    )?;

    debug!(target: "taiko-evm", "Validating state, sender account: {:?} nonce: {:?} at block: {:?}", tx.caller(), tx.nonce(), block);
    // If the transaction is an anchor transaction, we disable the balance check.
    if anchor_caller_address == tx.caller() && anchor_caller_nonce == tx.nonce() {
        debug!(target: "taiko-evm", "Anchor transaction detected, disabling balance check, sender account: {:?} nonce: {:?} at block: {:?}", tx.caller(), tx.nonce(), block);
        is_balance_check_disabled = true;
    } else {
        debug!(target: "taiko-evm", "Anchor transaction not detected, balance check enabled, sender account: {:?} nonce: {:?} at block: {:?}", tx.caller(), tx.nonce(), block);
    }

    let max_balance_spending = tx.max_balance_spending()?;

    // Check if account has enough balance for `gas_limit * max_fee`` and value transfer.
    // Transfer will be done inside `*_inner` functions.
    if is_balance_check_disabled {
        // Make sure the caller's balance is at least the value of the transaction.
        caller_account.info.balance = caller_account.info.balance.max(tx.value());
    } else if max_balance_spending > caller_account.info.balance {
        return Err(InvalidTransaction::LackOfFundForMaxFee {
            fee: Box::new(max_balance_spending),
            balance: Box::new(caller_account.info.balance),
        }
        .into());
    } else {
        let effective_balance_spending = tx
            .effective_balance_spending(basefee, blob_price)
            .expect("effective balance is always smaller than max balance so it can't overflow");

        // subtracting max balance spending with value that is going to be deducted later in the call.
        let gas_balance_spending = effective_balance_spending - tx.value();

        caller_account.info.balance = caller_account
            .info
            .balance
            .saturating_sub(gas_balance_spending);
    }

    // Touch account so we know it is changed.
    caller_account.mark_touch();
    Ok(())
}

/// Generates the network treasury address based on the chain ID.
pub fn get_treasury_address(chain_id: u64) -> Address {
    let prefix = chain_id.to_string();
    let suffix = "10001";

    let total_len = 40;
    let padding_len = total_len - prefix.len() - suffix.len();
    let padding = "0".repeat(padding_len);

    let hex_str = format!("0x{}{}{}", prefix, padding, suffix);

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
