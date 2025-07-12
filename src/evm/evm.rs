use alloy_primitives::Address;
use reth::revm::{
    context::{ContextTr, Evm as RevmEvm},
    handler::{
        EvmTr,
        instructions::{EthInstructions, InstructionProvider},
    },
    interpreter::{Interpreter, InterpreterTypes, interpreter::EthInterpreter},
};
use reth_evm::precompiles::PrecompilesMap;

/// Custom EVM for Taiko, we extend the RevmEvm with
/// [`TaikoEvmExtraContext`] to provide additional context
/// for Anchor transaction pre-execution checks and base fee sharing.
pub struct TaikoEvm<CTX, INSP> {
    pub inner: RevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PrecompilesMap>,
    pub extra_execution_ctx: Option<TaikoEvmExtraExecutionCtx>,
}

impl<CTX: ContextTr, INSP> TaikoEvm<CTX, INSP> {
    /// Creates a new instance of [`TaikoEvm`].
    pub fn new(
        inner: RevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PrecompilesMap>,
    ) -> Self {
        Self {
            inner,
            extra_execution_ctx: None,
        }
    }

    pub fn with_extra_execution_context(
        &mut self,
        basefee_share_pctg: u64,
        anchor_caller_address: Address,
        anchor_caller_nonce: u64,
    ) {
        self.extra_execution_ctx = Some(TaikoEvmExtraExecutionCtx {
            basefee_share_pctg,
            anchor_caller_address,
            anchor_caller_nonce,
        });
    }
}

/// A trait that integrates context, instruction set, and precompiles to create an EVM struct,
/// here we use the same implementation as `RevmEvm`.
impl<CTX: ContextTr, INSP> EvmTr for TaikoEvm<CTX, INSP>
where
    CTX: ContextTr,
{
    /// The context type that implements ContextTr to provide access to execution state
    type Context = CTX;
    /// The instruction set type that implements InstructionProvider to define available operations
    type Instructions = EthInstructions<EthInterpreter, CTX>;
    /// The type containing the available precompiled contracts
    type Precompiles = PrecompilesMap;

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

    /// Executes the interpreter loop for the given interpreter instance.
    /// Returns either a completion status or the next interpreter action to take.
    fn run_interpreter(
        &mut self,
        interpreter: &mut Interpreter<
            <Self::Instructions as InstructionProvider>::InterpreterTypes,
        >,
    ) -> <<Self::Instructions as InstructionProvider>::InterpreterTypes as InterpreterTypes>::Output
    {
        self.inner.run_interpreter(interpreter)
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
    basefee_share_pctg: u64,
    anchor_caller_address: Address,
    anchor_caller_nonce: u64,
}

impl TaikoEvmExtraExecutionCtx {
    /// Creates a new instance of [`TaikoEvmExecutionExtraCtx`].
    pub fn new(
        basefee_share_pctg: u64,
        anchor_caller_address: Address,
        anchor_caller_nonce: u64,
    ) -> Self {
        Self {
            basefee_share_pctg,
            anchor_caller_address,
            anchor_caller_nonce,
        }
    }

    /// Returns the base fee share percentage.
    pub fn basefee_share_pctg(&self) -> u64 {
        self.basefee_share_pctg
    }

    /// Returns the anchor caller address.
    pub fn anchor_caller_address(&self) -> Address {
        self.anchor_caller_address
    }

    /// Returns the anchor caller nonce.
    pub fn anchor_caller_nonce(&self) -> u64 {
        self.anchor_caller_nonce
    }
}
