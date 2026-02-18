//! Taiko-specific EVM wrapper and extra execution context.
use alloy_primitives::Address;
use reth_revm::{
    context::{ContextError, ContextTr, Evm as RevmEvm, FrameStack},
    handler::{EthFrame, EvmTr, FrameInitOrResult, FrameTr, ItemOrResult, PrecompileProvider},
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
};
use revm_database_interface::Database;

use crate::handler::instructions::TaikoInstructions;

/// Custom EVM for Taiko, we extend the RevmEvm with
/// [`TaikoEvmExtraContext`] to provide additional context
/// for Anchor transaction pre-execution checks and base fee sharing.
pub struct TaikoEvm<CTX, INSP, P> {
    /// Inner revm engine with Taiko instruction wiring.
    pub inner:
        RevmEvm<CTX, INSP, TaikoInstructions<EthInterpreter, CTX>, P, EthFrame<EthInterpreter>>,
    /// Optional per-block context captured from pre-executed anchor system calls.
    pub extra_execution_ctx: Option<TaikoEvmExtraExecutionCtx>,
}

impl<CTX: ContextTr, INSP, P> TaikoEvm<CTX, INSP, P> {
    /// Creates a new instance of [`TaikoEvm`].
    pub fn new(
        inner: RevmEvm<
            CTX,
            INSP,
            TaikoInstructions<EthInterpreter, CTX>,
            P,
            EthFrame<EthInterpreter>,
        >,
    ) -> Self {
        Self { inner, extra_execution_ctx: None }
    }

    #[inline]
    /// Set extra execution context derived from the anchor system call.
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

    /// Consumed self and returns new Evm type with given Inspector.
    pub fn with_inspector<OINSP>(self, inspector: OINSP) -> TaikoEvm<CTX, OINSP, P> {
        TaikoEvm {
            inner: self.inner.with_inspector(inspector),
            extra_execution_ctx: self.extra_execution_ctx,
        }
    }

    /// Consumes self and returns new Evm type with given Precompiles.
    pub fn with_precompiles<OP>(self, precompiles: OP) -> TaikoEvm<CTX, INSP, OP> {
        TaikoEvm {
            inner: self.inner.with_precompiles(precompiles),
            extra_execution_ctx: self.extra_execution_ctx,
        }
    }

    /// Consumes self and returns inner Inspector.
    pub fn into_inspector(self) -> INSP {
        self.inner.into_inspector()
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
    type Instructions = TaikoInstructions<EthInterpreter, CTX>;
    /// The type containing the available precompiled contracts
    type Precompiles = P;
    type Frame = EthFrame<EthInterpreter>;

    /// Returns shared references to context, instructions, precompiles, and frame stack.
    fn all(
        &self,
    ) -> (&Self::Context, &Self::Instructions, &Self::Precompiles, &FrameStack<Self::Frame>) {
        self.inner.all()
    }

    /// Returns mutable references to context, instructions, precompiles, and frame stack.
    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
    ) {
        self.inner.all_mut()
    }

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
    /// Base-fee share percentage paid to coinbase.
    base_fee_share_pctg: u64,
    /// Anchor caller address used to detect anchor transactions.
    anchor_caller_address: Address,
    /// Anchor caller nonce used to detect anchor transactions.
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
    use reth_revm::{Context, ExecuteEvm, context::TxEnv, db::InMemoryDB, state::AccountInfo};

    use crate::{
        alloy::TAIKO_GOLDEN_TOUCH_ADDRESS,
        context::{TaikoContext, TaikoEvmBuilder},
    };

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

        let mut taiko_evm = Context::taiko_mainnet().with_db(db).build_taiko_mainnet();

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
    }
}
