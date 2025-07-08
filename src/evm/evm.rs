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

#[derive(Default, Debug, Clone, Copy)]
pub struct TaikoEvmExtraContext {
    basefee_share_pctg: u64,
    anchor_caller_address: Option<Address>,
    anchor_caller_nonce: Option<u64>,
}

impl TaikoEvmExtraContext {
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

    pub fn basefee_share_pctg(&self) -> u64 {
        self.basefee_share_pctg
    }

    pub fn anchor_caller_address(&self) -> Option<Address> {
        self.anchor_caller_address
    }

    pub fn anchor_caller_nonce(&self) -> Option<u64> {
        self.anchor_caller_nonce
    }
}

/// Custom EVM for Taiko.
pub struct TaikoEvm<CTX, INSP> {
    pub inner: RevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PrecompilesMap>,
    pub extra_context: TaikoEvmExtraContext,
}

impl<CTX: ContextTr, INSP> TaikoEvm<CTX, INSP> {
    pub fn new(
        inner: RevmEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PrecompilesMap>,
        extra_context: TaikoEvmExtraContext,
    ) -> Self {
        Self {
            inner,
            extra_context,
        }
    }

    pub fn basefee_share_pctg(&self) -> u64 {
        self.extra_context.basefee_share_pctg()
    }

    pub fn anchor_caller_address(&self) -> Option<Address> {
        self.extra_context.anchor_caller_address()
    }

    pub fn anchor_caller_nonce(&self) -> Option<u64> {
        self.extra_context.anchor_caller_nonce()
    }

    pub fn extra_context(&self) -> TaikoEvmExtraContext {
        self.extra_context
    }
}

impl<CTX: ContextTr, INSP> EvmTr for TaikoEvm<CTX, INSP>
where
    CTX: ContextTr,
{
    type Context = CTX;
    type Instructions = EthInstructions<EthInterpreter, CTX>;
    type Precompiles = PrecompilesMap;

    fn ctx(&mut self) -> &mut Self::Context {
        &mut self.inner.ctx
    }

    fn ctx_ref(&self) -> &Self::Context {
        self.inner.ctx_ref()
    }

    fn ctx_instructions(&mut self) -> (&mut Self::Context, &mut Self::Instructions) {
        self.inner.ctx_instructions()
    }

    fn run_interpreter(
        &mut self,
        interpreter: &mut Interpreter<
            <Self::Instructions as InstructionProvider>::InterpreterTypes,
        >,
    ) -> <<Self::Instructions as InstructionProvider>::InterpreterTypes as InterpreterTypes>::Output
    {
        self.inner.run_interpreter(interpreter)
    }

    fn ctx_precompiles(&mut self) -> (&mut Self::Context, &mut Self::Precompiles) {
        self.inner.ctx_precompiles()
    }
}
