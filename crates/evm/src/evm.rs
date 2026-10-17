//! Taiko-specific EVM wrapper and extra execution context.
use alloy_primitives::{Address, U256};
use reth_revm::{
    context::{ContextError, ContextTr, Evm as RevmEvm, FrameStack},
    handler::{
        EthFrame, EvmTr, FrameInitOrResult, FrameResult, FrameTr, ItemOrResult, PrecompileProvider,
        instructions::{EthInstructions, InstructionProvider},
    },
    interpreter::{
        FrameInput, InterpreterResult, interpreter::EthInterpreter,
        interpreter_types::LegacyBytecode,
    },
};
use revm_database_interface::Database;

use crate::zk_gas::{
    meter::{ZkGasMeter, ZkGasOutcome},
    runtime::{V2ExecutionHooks, run_metered_plain, set_custom_error},
    schedule::ZkGasSchedule,
    v2::{StateAccess, ZkGasV2Accumulator, ZkGasV2AccumulatorError, ZkGasV2Error},
};

/// Custom EVM for Taiko, extended with Taiko execution context and metering hooks.
///
/// Schema-9 transaction scoping is owned by [`crate::alloy::TaikoEvmWrapper`]. Calling revm
/// handlers directly on this inner type bypasses begin/reset/commit and is therefore unsupported
/// for V2 quotes; all production, replay, and RPC entry points must use the Alloy
/// [`alloy_evm::Evm`] wrapper.
/// Pre-block system calls intentionally use their separate wrapper method and are not collected.
pub struct TaikoEvm<CTX, INSP, P> {
    /// Inner revm engine with Taiko instruction wiring.
    pub inner:
        RevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, P, EthFrame<EthInterpreter>>,
    /// Optional per-block context captured from pre-executed anchor system calls.
    pub extra_execution_ctx: Option<TaikoEvmExtraExecutionCtx>,
    /// Production-path zk gas meter used when execution does not need an external inspector.
    zk_gas_meter: Option<ZkGasMeter<'static>>,
    /// Optional schema-9 selector collector used by shadow/admission integration.
    zk_gas_v2: Option<ZkGasV2Accumulator>,
    /// Concrete context hooks installed together with the V2 collector.
    zk_gas_v2_hooks: Option<V2ExecutionHooks<CTX>>,
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
        Self {
            inner,
            extra_execution_ctx: None,
            zk_gas_meter: None,
            zk_gas_v2: None,
            zk_gas_v2_hooks: None,
        }
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

    /// Returns the schema-9 selector collector, when enabled.
    pub(crate) const fn zk_gas_v2(&self) -> Option<&ZkGasV2Accumulator> {
        self.zk_gas_v2.as_ref()
    }

    /// Returns the mutable schema-9 selector collector, when enabled.
    pub(crate) fn zk_gas_v2_mut(&mut self) -> Option<&mut ZkGasV2Accumulator> {
        self.zk_gas_v2.as_mut()
    }

    /// Removes and returns the schema-9 collector without changing its committed prefix.
    pub(crate) fn take_zk_gas_v2(&mut self) -> Option<ZkGasV2Accumulator> {
        self.zk_gas_v2_hooks = None;
        self.zk_gas_v2.take()
    }

    #[inline]
    /// Set extra execution context derived from the anchor system call.
    pub fn with_extra_execution_context(
        &mut self,
        base_fee_share_pctg: u64,
        anchor_caller_address: Address,
        anchor_caller_nonce: u64,
    ) {
        self.extra_execution_ctx = Some(TaikoEvmExtraExecutionCtx::new(
            base_fee_share_pctg,
            anchor_caller_address,
            anchor_caller_nonce,
        ));
    }
}

impl<DB, I, P>
    TaikoEvm<crate::alloy::TaikoEvmContext<DB>, crate::zk_gas::adapter::ZkGasInspector<I>, P>
where
    DB: Database,
{
    /// Installs an existing schema-9 collector on the plain execution path.
    pub(crate) fn install_zk_gas_v2(&mut self, accumulator: ZkGasV2Accumulator) {
        self.zk_gas_v2 = Some(accumulator);
        self.zk_gas_v2_hooks = Some(V2ExecutionHooks::TAIKO);
    }

    /// Enables schema-9 transactional selector collection on the concrete Taiko runtime.
    pub fn with_zk_gas_v2(mut self, enabled: bool) -> Self {
        self.set_zk_gas_v2_enabled(enabled);
        self
    }

    /// Enables or disables schema-9 transactional selector collection in place.
    pub(crate) fn set_zk_gas_v2_enabled(&mut self, enabled: bool) {
        if enabled {
            if self.zk_gas_v2.is_none() {
                self.install_zk_gas_v2(ZkGasV2Accumulator::default());
            }
        } else {
            self.take_zk_gas_v2();
        }
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
                let work_units =
                    (inputs.bytecode_address == Address::with_last_byte(0x07)).then(|| {
                        super::zk_gas::runtime::bn254_work_units(
                            &inputs.input.bytes(&self.inner.ctx),
                        )
                    });
                Some((
                    inputs.gas_limit,
                    inputs.bytecode_address,
                    inputs.caller,
                    inputs.target_address,
                    work_units,
                ))
            }
            FrameInput::Create(_) | FrameInput::Empty => None,
        };
        // Reaching frame dispatch confirms the deferred spawn wrapper. Record it before frame
        // initialization so a database/context error cannot leave a stale pending spawn; a
        // transaction-level error will discard the whole in-flight scope.
        if let Some(accumulator) = self.zk_gas_v2.as_mut() &&
            let Err(error) = accumulator.confirm_spawn()
        {
            accumulator.retain_diagnostic(Some(error));
        }

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

        if let Some(accumulator) = self.zk_gas_v2.as_mut() &&
            let Some((_, bytecode_address, caller, target, _)) = precompile_metering
        {
            for address in [caller, target, bytecode_address] {
                let _ = accumulator.observe_state_access(StateAccess::Account(address));
            }
        }

        if let ItemOrResult::Result(FrameResult::Call(outcome)) = &result &&
            outcome.was_precompile_called &&
            let Some((gas_limit, bytecode_address, _, _, _)) = precompile_metering &&
            let Some(meter) = self.zk_gas_meter.as_mut()
        {
            let gas_used = gas_limit.saturating_sub(outcome.result.gas.remaining());
            if let Err(ZkGasOutcome::LimitExceeded) =
                meter.charge_precompile(&bytecode_address, gas_used)
            {
                set_custom_error(&mut self.inner.ctx);
            }
        }

        if let ItemOrResult::Result(FrameResult::Call(outcome)) = &result &&
            outcome.was_precompile_called &&
            let Some((gas_limit, bytecode_address, _, _, work_units)) = precompile_metering &&
            let Some(accumulator) = self.zk_gas_v2.as_mut()
        {
            let native_gas = gas_limit.saturating_sub(outcome.result.gas.remaining());
            let address = U256::from_be_slice(bytecode_address.as_slice());
            match u16::try_from(address) {
                Ok(address) => {
                    let _ = accumulator.observe_precompile(address, native_gas, work_units);
                }
                Err(_) => accumulator.retain_diagnostic(Some(ZkGasV2AccumulatorError::Pricing(
                    ZkGasV2Error::InvalidInput("precompile_address"),
                ))),
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
        if self.zk_gas_meter.is_none() && self.zk_gas_v2.is_none() {
            return self.inner.frame_run();
        }

        let frame = self.inner.frame_stack.get();
        let context = &mut self.inner.ctx;
        let instructions = &mut self.inner.instruction;

        if let Some(accumulator) = self.zk_gas_v2.as_mut() {
            for address in
                [frame.interpreter.input.caller_address, frame.interpreter.input.target_address]
            {
                let _ = accumulator.observe_state_access(StateAccess::Account(address));
            }
            if let Some(address) = frame.interpreter.input.bytecode_address {
                let _ = accumulator.observe_state_access(StateAccess::Account(address));
            }
            let code_hash = frame.interpreter.bytecode.get_or_calculate_hash();
            let byte_length = frame.interpreter.bytecode.bytecode_slice().len() as u64;
            let _ = accumulator
                .observe_state_access(StateAccess::Code { hash: code_hash, byte_length });
        }

        let action = run_metered_plain(
            context,
            &mut frame.interpreter,
            instructions.instruction_table(),
            instructions.gas_table(),
            self.zk_gas_meter.as_mut(),
            self.zk_gas_v2.as_mut(),
            self.zk_gas_v2_hooks,
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
    /// Whether this context was installed by the authoritative anchor system call issued by the
    /// block executor. A `false` value marks a context derived on the fly for replay-style
    /// execution (RPC tracing), where the basefee-share percentage from block extra data is
    /// unknown and basefee sharing must therefore stay disabled.
    from_anchor_system_call: bool,
}

impl TaikoEvmExtraExecutionCtx {
    /// Creates the authoritative context installed by the anchor system call.
    pub fn new(
        base_fee_share_pctg: u64,
        anchor_caller_address: Address,
        anchor_caller_nonce: u64,
    ) -> Self {
        Self {
            base_fee_share_pctg,
            anchor_caller_address,
            anchor_caller_nonce,
            from_anchor_system_call: true,
        }
    }

    /// Creates a context derived from database state for replay-style execution paths that
    /// never issue the anchor system call (RPC tracing and transaction replay).
    ///
    /// The derived context carries the anchor caller identity needed for the anchor
    /// exemptions, but no basefee-share percentage: that value comes from block extra data,
    /// which replay paths do not have. [`Self::is_from_anchor_system_call`] returns `false` so
    /// fee-sharing logic can tell the two apart.
    pub fn derived(anchor_caller_address: Address, anchor_caller_nonce: u64) -> Self {
        Self {
            base_fee_share_pctg: 0,
            anchor_caller_address,
            anchor_caller_nonce,
            from_anchor_system_call: false,
        }
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

    /// Returns whether this context was installed by the authoritative anchor system call
    /// (as opposed to being derived from database state for replay-style execution).
    #[inline]
    pub fn is_from_anchor_system_call(&self) -> bool {
        self.from_anchor_system_call
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
