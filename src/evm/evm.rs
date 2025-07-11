use reth::revm::{
    context::{ContextTr, Evm as RevmEvm},
    handler::{
        EvmTr,
        instructions::{EthInstructions, InstructionProvider},
    },
    interpreter::{Interpreter, InterpreterTypes, interpreter::EthInterpreter},
    primitives::Address,
};
use reth_evm::precompiles::PrecompilesMap;

/// Extra context for Taiko EVM execution, for
/// Anchor transaction pre-execution checks and base fee sharing.
#[derive(Default, Debug, Clone, Copy)]
pub struct TaikoEvmExtraContext {
    basefee_share_pctg: u64,
    anchor_caller_address: Option<Address>,
    anchor_caller_nonce: Option<u64>,
}

impl TaikoEvmExtraContext {
    /// Creates a new instance of [`TaikoEvmExtraContext`].
    pub fn new(
        basefee_share_pctg: u64,
        anchor_caller_address: Option<Address>,
        anchor_caller_nonce: Option<u64>,
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

    /// Returns the address of the Anchor transaction caller, if any.
    pub fn anchor_caller_address(&self) -> Option<Address> {
        self.anchor_caller_address
    }

    /// Returns the nonce of the Anchor transaction caller, if any.
    pub fn anchor_caller_nonce(&self) -> Option<u64> {
        self.anchor_caller_nonce
    }
}

/// Custom EVM for Taiko, we extend the RevmEvm with
/// [`TaikoEvmExtraContext`] to provide additional context
/// for Anchor transaction pre-execution checks and base fee sharing.
pub struct TaikoEvm<CTX, INSP> {
    pub inner: RevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PrecompilesMap>,
    pub extra_context: TaikoEvmExtraContext,
}

impl<CTX: ContextTr, INSP> TaikoEvm<CTX, INSP> {
    /// Creates a new instance of [`TaikoEvm`].
    pub fn new(
        inner: RevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PrecompilesMap>,
        extra_context: TaikoEvmExtraContext,
    ) -> Self {
        Self {
            inner,
            extra_context,
        }
    }

    /// Returns the extra context for the given [`TaikoEVM`].
    pub fn extra_context(&self) -> TaikoEvmExtraContext {
        self.extra_context
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
