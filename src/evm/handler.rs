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

use crate::evm::evm::TaikoEvmExtraContext;

#[derive(Default, Debug, Clone)]
pub struct TaikoEvmHandler<CTX, ERROR, FRAME> {
    pub _phantom: core::marker::PhantomData<(CTX, ERROR, FRAME)>,
    extra_context: TaikoEvmExtraContext,
}

impl<CTX, ERROR, FRAME> TaikoEvmHandler<CTX, ERROR, FRAME> {
    pub fn new(extra_context: TaikoEvmExtraContext) -> Self {
        Self {
            _phantom: core::marker::PhantomData,
            extra_context,
        }
    }
}

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
    type Evm = EVM;
    type Error = ERROR;
    type Frame = FRAME;
    type HaltReason = HaltReason;

    fn reward_beneficiary(
        &self,
        _evm: &mut Self::Evm,
        _exec_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        reward_beneficiary(_evm.ctx(), _exec_result.gas_mut(), &self.extra_context)
            .map_err(From::from)
    }

    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<(), Self::Error> {
        validate_against_state_and_deduct_caller(evm.ctx(), &self.extra_context)
    }
}

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

#[inline]
pub fn reward_beneficiary<CTX: ContextTr>(
    context: &mut CTX,
    gas: &mut Gas,
    extra_context: &TaikoEvmExtraContext,
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
    let total_fee = U256::from(coinbase_gas_price * (spent - refunded as u64) as u128);
    // Get the caller address and nonce from the transaction, to check if it is an anchor transaction.
    let tx_caller: Address = tx.caller();
    let tx_nonce = tx.nonce();

    let coinbase_account = context.journal().load_account(beneficiary)?;

    debug!(target: "taiko-evm", "Sender account: {:?} nonce: {:?}", tx_caller, tx_nonce);
    coinbase_account.data.mark_touch();
    if extra_context.anchor_caller_address() != Some(tx_caller)
        || extra_context.anchor_caller_nonce() != Some(tx_nonce)
    {
        debug!(target: "taiko-evm", "Share basefee with coinbase and treasury.");
        let fee_coinbase = total_fee.saturating_mul(U256::from(extra_context.basefee_share_pctg()))
            / U256::from(100u64);
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

        treasury_account.data.info.balance = treasury_account
            .data
            .info
            .balance
            .saturating_add(fee_treasury);
    } else {
        debug!(target: "taiko-evm", "Anchor transaction detected, no reward and basefee sharing.");
    }
    Ok(())
}

#[inline]
pub fn validate_against_state_and_deduct_caller<
    CTX: ContextTr,
    ERROR: From<InvalidTransaction> + From<<CTX::Db as Database>::Error>,
>(
    context: &mut CTX,
    extra_context: &TaikoEvmExtraContext,
) -> Result<(), ERROR> {
    let basefee = context.block().basefee() as u128;
    let blob_price = context.block().blob_gasprice().unwrap_or_default();
    let mut is_balance_check_disabled = context.cfg().is_balance_check_disabled();
    let is_eip3607_disabled = context.cfg().is_eip3607_disabled();
    let is_nonce_check_disabled = context.cfg().is_nonce_check_disabled();

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

    debug!(target: "taiko-evm", "Sender account: {:?} nonce: {:?}", tx.caller(), tx.nonce());
    // If the transaction is an anchor transaction, we disable the balance check.
    if extra_context.anchor_caller_address() == Some(tx.caller())
        && extra_context.anchor_caller_nonce() == Some(tx.nonce())
    {
        debug!(target: "taiko-evm", "Anchor transaction detected, disabling balance check.");
        is_balance_check_disabled = true;
    } else {
        debug!(target: "taiko-evm", "Anchor transaction not detected, balance check enabled.");
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

fn get_treasury_address(chain_id: u64) -> Address {
    let prefix = chain_id.to_string();
    let suffix = "10001";

    let total_len = 40;
    let padding_len = total_len - prefix.len() - suffix.len();
    let padding = "0".repeat(padding_len);

    let hex_str = format!("0x{}{}{}", prefix, padding, suffix);

    Address::from_str(&hex_str).expect("invalid address string")
}
