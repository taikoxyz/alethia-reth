use reth::revm::{
    Inspector,
    interpreter::{
        interpreter::EthInterpreter, Gas, Interpreter, InterpreterAction, InstructionResult,
    },
};

/// Inspector that aborts a transaction once it executes more than `limit` `JUMPDEST` opcodes.
///
/// Usage sketch (leave commented out until you want to enable it):
/// - In `TaikoEvmFactory::create_evm_with_inspector`, swap the `NoOpInspector` for
///   `JumpdestLimiter::new(100)` and set the wrapper's `inspect` flag to `true` so every tx is
///   executed with this inspector.
/// - In payload building (`crates/payload/src/builder.rs`) or block execution, match on the EVM
///   error and `continue` to drop the offending tx instead of failing the whole payload:
///   ```
///   // if let Err(BlockExecutionError::Evm { error, .. }) = builder.execute_transaction(tx.clone()) {
///   //     if error.is_fatal_external_error() {
///   //         // JUMPDEST limit hit; skip this tx
///   //         continue;
///   //     }
///   // }
///   ```
#[derive(Debug, Clone)]
pub struct JumpdestLimiter {
    limit: u64,
    count: u64,
}

impl JumpdestLimiter {
    pub const fn new(limit: u64) -> Self {
        Self { limit, count: 0 }
    }
}

impl<CTX> Inspector<CTX, EthInterpreter> for JumpdestLimiter {
    fn initialize_interp(&mut self, _interp: &mut Interpreter<EthInterpreter>, _ctx: &mut CTX) {
        // Reset per top-level transaction.
        self.count = 0;
    }

    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _ctx: &mut CTX) {
        const JUMPDEST: u8 = 0x5b;
        if interp.bytecode.opcode() == JUMPDEST {
            self.count += 1;
            if self.count > self.limit {
                // Halt execution immediately; upstream sees a fatal external error for this tx.
                interp.bytecode.set_action(InterpreterAction::new_halt(
                    InstructionResult::FatalExternalError,
                    Gas::new(0),
                ));
            }
        }
    }
}
