use alloy_primitives::Address;
use reth::revm::{
    context::{ContextTr, Evm as RevmEvm},
    handler::{EvmTr, instructions::EthInstructions},
    interpreter::interpreter::EthInterpreter,
};
use reth_revm::{
    context::{ContextError, FrameStack},
    handler::{EthFrame, FrameInitOrResult, FrameTr, ItemOrResult, PrecompileProvider},
    interpreter::InterpreterResult,
};
use revm_database_interface::Database;

/// Custom EVM for Taiko, we extend the RevmEvm with
/// [`TaikoEvmExtraContext`] to provide additional context
/// for Anchor transaction pre-execution checks and base fee sharing.
pub struct TaikoEvm<CTX, INSP, P> {
    pub inner:
        RevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, P, EthFrame<EthInterpreter>>,
    pub extra_execution_ctx: Option<TaikoEvmExtraExecutionCtx>,
}

impl<CTX: ContextTr, INSP, P> TaikoEvm<CTX, INSP, P> {
    /// Creates a new instance of [`TaikoEvm`].
    pub fn new(
        inner: RevmEvm<
            CTX,
            INSP,
            EthInstructions<EthInterpreter, CTX>,
            P,
            EthFrame<EthInterpreter>,
        >,
    ) -> Self {
        Self { inner, extra_execution_ctx: None }
    }

    #[inline]
    pub fn with_extra_execution_context(
        &mut self,
        base_fee_share_pctg: u64,
        anchor_caller_address: Address,
        anchor_caller_nonce: u64,
    ) {
        self.extra_execution_ctx = Some(TaikoEvmExtraExecutionCtx {
            base_fee_share_pctg,
            anchor_caller_address,
            anchor_caller_nonce,
        });
    }
}

/// A trait that integrates context, instruction set, and precompiles to create an EVM struct,
/// here we use the same implementation as `RevmEvm`.
impl<CTX: ContextTr, INSP, P> EvmTr for TaikoEvm<CTX, INSP, P>
where
    CTX: ContextTr,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    /// The context type that implements ContextTr to provide access to execution state
    type Context = CTX;
    /// The instruction set type that implements InstructionProvider to define available operations
    type Instructions = EthInstructions<EthInterpreter, CTX>;
    /// The type containing the available precompiled contracts
    type Precompiles = P;
    type Frame = EthFrame<EthInterpreter>;

    /// Returns a mutable reference to the execution context
    fn ctx(&mut self) -> &mut Self::Context {
        &mut self.inner.ctx
    }

    /// Returns an immutable reference to the execution context
    fn ctx_ref(&self) -> &Self::Context {
        self.inner.ctx_ref()
    }

    /// Returns mutable references to both the context and instruction set.
    /// This enables atomic access to both components when needed.
    fn ctx_instructions(&mut self) -> (&mut Self::Context, &mut Self::Instructions) {
        self.inner.ctx_instructions()
    }

    /// Returns a mutable reference to the frame stack.
    fn frame_stack(&mut self) -> &mut FrameStack<Self::Frame> {
        self.inner.frame_stack()
    }

    /// Initializes the frame for the given frame input. Frame is pushed to the frame stack.
    fn frame_init(
        &mut self,
        frame_input: <Self::Frame as FrameTr>::FrameInit,
    ) -> Result<
        ItemOrResult<&mut Self::Frame, <Self::Frame as FrameTr>::FrameResult>,
        ContextError<<<Self::Context as ContextTr>::Db as Database>::Error>,
    > {
        self.inner.frame_init(frame_input)
    }

    /// Run the frame from the top of the stack. Returns the frame init or result.
    ///
    /// If frame has returned result it would mark it as finished.
    fn frame_run(
        &mut self,
    ) -> Result<
        FrameInitOrResult<Self::Frame>,
        ContextError<<<Self::Context as ContextTr>::Db as Database>::Error>,
    > {
        self.inner.frame_run()
    }

    /// Returns the result of the frame to the caller. Frame is popped from the frame stack.
    /// Consumes the frame result or returns it if there is more frames to run.
    fn frame_return_result(
        &mut self,
        frame_result: <Self::Frame as FrameTr>::FrameResult,
    ) -> Result<
        Option<<Self::Frame as FrameTr>::FrameResult>,
        ContextError<<<Self::Context as ContextTr>::Db as Database>::Error>,
    > {
        self.inner.frame_return_result(frame_result)
    }

    /// Returns mutable references to both the context and precompiles.
    /// This enables atomic access to both components when needed.
    fn ctx_precompiles(&mut self) -> (&mut Self::Context, &mut Self::Precompiles) {
        self.inner.ctx_precompiles()
    }
}

/// Extra context for Taiko EVM execution, used to provide additional
/// context for Anchor transaction pre-execution checks and base fee sharing.
#[derive(Debug, Clone, Default)]
pub struct TaikoEvmExtraExecutionCtx {
    base_fee_share_pctg: u64,
    anchor_caller_address: Address,
    anchor_caller_nonce: u64,
}

impl TaikoEvmExtraExecutionCtx {
    /// Creates a new instance of [`TaikoEvmExecutionExtraCtx`].
    pub fn new(
        base_fee_share_pctg: u64,
        anchor_caller_address: Address,
        anchor_caller_nonce: u64,
    ) -> Self {
        Self { base_fee_share_pctg, anchor_caller_address, anchor_caller_nonce }
    }

    /// Returns the base fee share percentage.
    #[inline]
    pub fn base_fee_share_pctg(&self) -> u64 {
        self.base_fee_share_pctg
    }

    /// Returns the anchor caller address.
    #[inline]
    pub fn anchor_caller_address(&self) -> Address {
        self.anchor_caller_address
    }

    /// Returns the anchor caller nonce.
    #[inline]
    pub fn anchor_caller_nonce(&self) -> u64 {
        self.anchor_caller_nonce
    }
}

#[cfg(test)]
mod test {
    use alloy_primitives::{U64, U256};
    use reth_revm::{
        Context, ExecuteEvm, MainBuilder, MainContext, context::TxEnv, db::InMemoryDB,
        interpreter::Host, state::AccountInfo,
    };

    use crate::{alloy::TAIKO_GOLDEN_TOUCH_ADDRESS, handler::get_treasury_address};

    use super::*;

    #[test]
    fn test_transact_one_with_extra_execution_context() {
        let golden_touch_address = Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS);
        let nonce = U64::random().to::<u64>();
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            golden_touch_address,
            AccountInfo { nonce, balance: U256::from(0), ..Default::default() },
        );

        let mut taiko_evm = TaikoEvm::new(Context::mainnet().with_db(db).build_mainnet());

        let mut state = taiko_evm.transact_one(
            TxEnv::builder()
                .gas_limit(1_000_000)
                .caller(golden_touch_address)
                .nonce(nonce - 1)
                .build()
                .unwrap(),
        );
        assert!(state.is_err());

        state = taiko_evm.transact_one(
            TxEnv::builder()
                .gas_limit(1_000_000)
                .gas_price(1)
                .caller(golden_touch_address)
                .to(golden_touch_address)
                .nonce(nonce)
                .build()
                .unwrap(),
        );
        assert!(state.is_err());

        taiko_evm.extra_execution_ctx =
            Some(TaikoEvmExtraExecutionCtx::new(50, golden_touch_address, nonce + 1));
        state = taiko_evm.transact_one(
            TxEnv::builder()
                .gas_limit(1_000_000)
                .gas_price(1)
                .caller(golden_touch_address)
                .to(get_treasury_address(taiko_evm.ctx_ref().chain_id().to()))
                .nonce(nonce + 1)
                .build()
                .unwrap(),
        );
        assert!(state.is_ok());
    }
}
