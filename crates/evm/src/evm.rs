//! Taiko-specific EVM wrapper and extra execution context.
use alloy_primitives::Address;
use reth_revm::{
    context::{ContextError, ContextTr, Evm as RevmEvm, FrameStack},
    handler::{
        EthFrame, EvmTr, FrameInitOrResult, FrameResult, FrameTr, ItemOrResult, PrecompileProvider,
        instructions::{EthInstructions, InstructionProvider},
    },
    interpreter::{FrameInput, InterpreterResult, interpreter::EthInterpreter},
};
use revm_database_interface::Database;

use crate::zk_gas::{
    meter::{ZkGasMeter, ZkGasOutcome},
    runtime::{run_metered_plain, set_custom_error},
    schedule::ZkGasSchedule,
};

/// Custom EVM for Taiko, we extend the RevmEvm with
/// [`TaikoEvmExtraContext`] to provide additional context
/// for Anchor transaction pre-execution checks and base fee sharing.
pub struct TaikoEvm<CTX, INSP, P> {
    /// Inner revm engine with Taiko instruction wiring.
    pub inner:
        RevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, P, EthFrame<EthInterpreter>>,
    /// Optional per-block context captured from pre-executed anchor system calls.
    pub extra_execution_ctx: Option<TaikoEvmExtraExecutionCtx>,
    /// Production-path zk gas meter used when execution does not need an external inspector.
    zk_gas_meter: Option<ZkGasMeter<'static>>,
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
        Self { inner, extra_execution_ctx: None, zk_gas_meter: None }
    }

    /// Installs a production-path zk gas meter for the provided schedule.
    pub fn with_zk_gas_schedule(mut self, schedule: Option<&'static ZkGasSchedule>) -> Self {
        self.zk_gas_meter = schedule.map(ZkGasMeter::new);
        self
    }

    /// Returns the production-path zk gas meter, if one is installed.
    pub(crate) const fn zk_gas_meter(&self) -> Option<&ZkGasMeter<'static>> {
        self.zk_gas_meter.as_ref()
    }

    /// Returns the mutable production-path zk gas meter, if one is installed.
    pub(crate) fn zk_gas_meter_mut(&mut self) -> Option<&mut ZkGasMeter<'static>> {
        self.zk_gas_meter.as_mut()
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
        let precompile_metering = match &frame_input.frame_input {
            FrameInput::Call(inputs) => {
                Some((inputs.gas_limit, inputs.bytecode_address.as_slice()[19]))
            }
            FrameInput::Create(_) | FrameInput::Empty => None,
        };

        let is_first_init = self.inner.frame_stack.index().is_none();
        let new_frame = if is_first_init {
            self.inner.frame_stack.start_init()
        } else {
            self.inner.frame_stack.get_next()
        };

        let result = Self::Frame::init_with_context(
            new_frame,
            &mut self.inner.ctx,
            &mut self.inner.precompiles,
            frame_input,
        )?;

        if let ItemOrResult::Result(FrameResult::Call(outcome)) = &result &&
            outcome.was_precompile_called &&
            let Some((gas_limit, address_low_byte)) = precompile_metering &&
            let Some(meter) = self.zk_gas_meter.as_mut()
        {
            let gas_used = gas_limit.saturating_sub(outcome.result.gas.remaining());
            if let Err(ZkGasOutcome::LimitExceeded) =
                meter.charge_precompile(address_low_byte, gas_used)
            {
                set_custom_error(&mut self.inner.ctx);
            }
        }

        Ok(result.map_item(|token| {
            if is_first_init {
                unsafe { self.inner.frame_stack.end_init(token) };
            } else {
                unsafe { self.inner.frame_stack.push(token) };
            }
            self.inner.frame_stack.get()
        }))
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
        let Some(meter) = self.zk_gas_meter.as_mut() else {
            return self.inner.frame_run();
        };

        let frame = self.inner.frame_stack.get();
        let context = &mut self.inner.ctx;
        let instructions = &mut self.inner.instruction;

        let action = run_metered_plain(
            context,
            &mut frame.interpreter,
            instructions.instruction_table(),
            meter,
        );

        frame.process_next_action(context, action).inspect(|i| {
            if i.is_result() {
                frame.set_finished(true);
            }
        })
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
    use alloy_primitives::U256;
    use reth_revm::{
        Context, ExecuteEvm, MainBuilder, MainContext, context::TxEnv, db::InMemoryDB,
        state::AccountInfo,
    };

    use crate::alloy::TAIKO_GOLDEN_TOUCH_ADDRESS;

    use super::*;

    #[test]
    fn test_transact_one_with_extra_execution_context() {
        let golden_touch_address = Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS);
        let nonce = 1_u64;
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
    }
}
