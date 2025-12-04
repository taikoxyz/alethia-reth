use reth::revm::{
    Inspector,
    context::{ContextError, ContextTr, JournalTr},
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, InstructionResult, Interpreter,
        InterpreterAction, interpreter::EthInterpreter,
        interpreter_types::{Jumps, LoopControl},
    },
    primitives::{Address, Log, U256},
};

/// Inspector that aborts execution once it executes too many `JUMPDEST` opcodes.
/// - Per-transaction limit ensures a single tx cannot dominate execution.
/// - Per-block limit caps aggregate `JUMPDEST` hits across all txs in the block.
///
/// Defaults: 100 per transaction, 500 per block.
pub const JUMPDEST_TX_LIMIT_ERR: &str = "jumpdest tx limit exceeded";
pub const JUMPDEST_BLOCK_LIMIT_ERR: &str = "jumpdest block limit exceeded";
pub const DEFAULT_TX_JUMPDEST_LIMIT: u64 = 100;
pub const DEFAULT_BLOCK_JUMPDEST_LIMIT: u64 = 500;

#[derive(Debug, Clone)]
pub struct JumpdestLimiter {
    tx_limit: u64,
    block_limit: u64,
    tx_count: u64,
    block_count: u64,
}

impl JumpdestLimiter {
    pub const fn new(tx_limit: u64, block_limit: u64) -> Self {
        Self { tx_limit, block_limit, tx_count: 0, block_count: 0 }
    }
}

impl<CTX: ContextTr> Inspector<CTX, EthInterpreter> for JumpdestLimiter {
    fn initialize_interp(&mut self, _interp: &mut Interpreter<EthInterpreter>, ctx: &mut CTX) {
        // Reset only when entering the outermost frame of a new transaction.
        if ctx.journal().depth() == 1 {
            self.tx_count = 0;
        }
    }

    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, ctx: &mut CTX) {
        const JUMPDEST: u8 = 0x5b;
        if interp.bytecode.opcode() == JUMPDEST {
            self.tx_count += 1;
            self.block_count += 1;

            let (limit_hit, message) = if self.tx_count > self.tx_limit {
                (true, JUMPDEST_TX_LIMIT_ERR)
            } else if self.block_count > self.block_limit {
                (true, JUMPDEST_BLOCK_LIMIT_ERR)
            } else {
                (false, "")
            };

            if limit_hit {
                let err_slot = ctx.error();
                if err_slot.is_ok() {
                    *err_slot = Err(ContextError::Custom(message.to_string()));
                }
                // Halt execution immediately; upstream will surface the custom error.
                interp.bytecode.set_action(InterpreterAction::new_halt(
                    InstructionResult::FatalExternalError,
                    interp.gas,
                ));
            }
        }
    }
}

/// Wraps another inspector with a `JumpdestLimiter`.
#[derive(Debug, Clone)]
pub struct LimitingInspector<I> {
    limiter: JumpdestLimiter,
    inner: I,
}

impl<I> LimitingInspector<I> {
    pub fn new(tx_limit: u64, block_limit: u64, inner: I) -> Self {
        Self { limiter: JumpdestLimiter::new(tx_limit, block_limit), inner }
    }
}

impl<CTX: ContextTr, I: Inspector<CTX, EthInterpreter>> Inspector<CTX, EthInterpreter>
    for LimitingInspector<I>
{
    fn initialize_interp(&mut self, interp: &mut Interpreter<EthInterpreter>, ctx: &mut CTX) {
        self.limiter.initialize_interp(interp, ctx);
        self.inner.initialize_interp(interp, ctx);
    }

    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, ctx: &mut CTX) {
        self.limiter.step(interp, ctx);
        self.inner.step(interp, ctx);
    }

    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, ctx: &mut CTX) {
        self.inner.step_end(interp, ctx);
    }

    fn log(&mut self, interp: &mut Interpreter<EthInterpreter>, ctx: &mut CTX, log: Log) {
        self.inner.log(interp, ctx, log);
    }

    fn call(&mut self, ctx: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.inner.call(ctx, inputs)
    }

    fn call_end(&mut self, ctx: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        self.inner.call_end(ctx, inputs, outcome);
    }

    fn create(&mut self, ctx: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.inner.create(ctx, inputs)
    }

    fn create_end(&mut self, ctx: &mut CTX, inputs: &CreateInputs, outcome: &mut CreateOutcome) {
        self.inner.create_end(ctx, inputs, outcome);
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.inner.selfdestruct(contract, target, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::TaikoEvmFactory;
    use crate::spec::TaikoSpecId;
    use reth::revm::{
        bytecode::Bytecode,
        context::{BlockEnv, CfgEnv, TxEnv},
        db::InMemoryDB,
        inspector::NoOpInspector,
        primitives::{Address, Bytes, U256},
        state::AccountInfo,
    };
    use alloy_evm::{Evm, EvmEnv, EvmFactory};

    /// Creates bytecode with a specified number of JUMPDEST opcodes.
    fn create_bytecode_with_jumpdests(count: usize) -> Bytes {
        let mut code = vec![0x5b; count];
        code.push(0x00); // STOP to end execution
        Bytes::from(code)
    }

    fn caller_with_jumpdests_and_call(jumpdest_count: usize, callee: Address) -> Bytes {
        let mut code = vec![0x5b; jumpdest_count];
        // PUSH1 0 (out size)
        // PUSH1 0 (out offset)
        // PUSH1 0 (in size)
        // PUSH1 0 (in offset)
        // PUSH1 0 (value)
        // PUSH20 <callee>
        // PUSH2 0xffff (gas)
        // CALL
        // STOP
        code.extend([0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00]);
        code.push(0x73);
        code.extend_from_slice(callee.as_slice());
        code.extend([0x61, 0xff, 0xff, 0xf1, 0x00]);
        Bytes::from(code)
    }

    fn block_env_with_gas_limit() -> BlockEnv {
        BlockEnv { gas_limit: 30_000_000, ..Default::default() }
    }

    fn evm_env() -> EvmEnv<TaikoSpecId, BlockEnv> {
        let mut cfg_env = CfgEnv::<TaikoSpecId>::default();
        cfg_env.disable_nonce_check = true;
        EvmEnv { cfg_env, block_env: block_env_with_gas_limit() }
    }

    #[test]
    fn test_limiter_allows_within_limit() {
        let mut db = InMemoryDB::default();
        let contract_address = Address::random();
        
        // Create bytecode with 50 JUMPDESTs (well below default limit)
        let code = create_bytecode_with_jumpdests(50);
        let bytecode = Bytecode::new_raw_checked(code).unwrap();
        
        db.insert_account_info(
            contract_address,
            AccountInfo {
                balance: U256::from(1_000_000),
                code_hash: bytecode.hash_slow(),
                code: Some(bytecode),
                nonce: 0,
            },
        );
        
        let evm_env = evm_env();
        let mut evm = TaikoEvmFactory.create_evm_with_inspector(
            db,
            evm_env,
            LimitingInspector::new(
                DEFAULT_TX_JUMPDEST_LIMIT,
                DEFAULT_BLOCK_JUMPDEST_LIMIT,
                NoOpInspector {},
            ),
        );
        
        let result = evm.transact(
            TxEnv::builder()
                .gas_limit(1_000_000)
                .gas_price(0)
                .caller(Address::random())
                .call(contract_address)
                .build()
                .unwrap(),
        );
        
        // Should succeed - within limit
        assert!(result.is_ok(), "Transaction should succeed with JUMPDESTs within limit");
    }

    #[test]
    fn test_limiter_blocks_over_limit() {
        let mut db = InMemoryDB::default();
        let contract_address = Address::random();
        
        // Create bytecode with 150 JUMPDESTs (over default limit)
        let code = create_bytecode_with_jumpdests(150);
        let bytecode = Bytecode::new_raw_checked(code).unwrap();
        
        db.insert_account_info(
            contract_address,
            AccountInfo {
                balance: U256::from(1_000_000),
                code_hash: bytecode.hash_slow(),
                code: Some(bytecode),
                nonce: 0,
            },
        );
        
        let evm_env = evm_env();
        let mut evm = TaikoEvmFactory.create_evm_with_inspector(
            db,
            evm_env,
            LimitingInspector::new(
                DEFAULT_TX_JUMPDEST_LIMIT,
                DEFAULT_BLOCK_JUMPDEST_LIMIT,
                NoOpInspector {},
            ),
        );
        
        let result = evm.transact(
            TxEnv::builder()
                .gas_limit(10_000_000)
                .gas_price(0)
                .caller(Address::random())
                .call(contract_address)
                .build()
                .unwrap(),
        );

        // Should fail - over limit
        match result {
            Err(err) => {
                let err_str = format!("{:?}", err);
                assert!(err_str.contains(JUMPDEST_TX_LIMIT_ERR), "Expected jumpdest limit error, got: {}", err_str);
            }
            Ok(result) => panic!("Transaction should fail when exceeding JUMPDEST limit, got: {:?}", result),
        }
    }

    #[test]
    fn test_limiter_counts_across_subcalls() {
        let mut db = InMemoryDB::default();
        let caller = Address::random();
        let callee = Address::random();

        let callee_code = Bytecode::new_raw_checked(create_bytecode_with_jumpdests(60)).unwrap();
        db.insert_account_info(
            callee,
            AccountInfo {
                balance: U256::from(1_000_000),
                code_hash: callee_code.hash_slow(),
                code: Some(callee_code),
                nonce: 0,
            },
        );

        let caller_code =
            Bytecode::new_raw_checked(caller_with_jumpdests_and_call(60, callee)).unwrap();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(1_000_000),
                code_hash: caller_code.hash_slow(),
                code: Some(caller_code),
                nonce: 0,
            },
        );

        let evm_env = evm_env();
        let mut evm = TaikoEvmFactory.create_evm_with_inspector(
            db,
            evm_env,
            LimitingInspector::new(
                DEFAULT_TX_JUMPDEST_LIMIT,
                DEFAULT_BLOCK_JUMPDEST_LIMIT,
                NoOpInspector {},
            ),
        );

        let result = evm.transact(
            TxEnv::builder()
                .gas_limit(5_000_000)
                .gas_price(0)
                .caller(Address::random())
                .call(caller)
                .build()
                .unwrap(),
        );

        match result {
            Err(err) => {
                assert!(format!("{err:?}").contains(JUMPDEST_TX_LIMIT_ERR), "Expected combined jumpdest limit hit, got: {err:?}");
            }
            Ok(result) => panic!("Transaction should fail when combined jumpdests exceed limit, got: {result:?}"),
        }
    }

    #[test]
    fn test_block_limit_across_transactions() {
        let mut db = InMemoryDB::default();
        let contract_address = Address::random();

        // 90 JUMPDESTs keeps each tx under the per-tx limit but will exceed block limit after 5 txs.
        let code = create_bytecode_with_jumpdests(90);
        let bytecode = Bytecode::new_raw_checked(code).unwrap();

        db.insert_account_info(
            contract_address,
            AccountInfo {
                balance: U256::from(1_000_000),
                code_hash: bytecode.hash_slow(),
                code: Some(bytecode),
                nonce: 0,
            },
        );

        let evm_env = evm_env();
        let mut evm = TaikoEvmFactory.create_evm_with_inspector(
            db,
            evm_env,
            LimitingInspector::new(
                DEFAULT_TX_JUMPDEST_LIMIT,
                DEFAULT_BLOCK_JUMPDEST_LIMIT,
                NoOpInspector {},
            ),
        );

        let caller = Address::random();

        for nonce in 0..5 {
            let res = evm.transact(
                TxEnv::builder()
                    .gas_limit(5_000_000)
                    .gas_price(0)
                    .caller(caller)
                    .call(contract_address)
                    .nonce(nonce)
                    .build()
                    .unwrap(),
            );
            assert!(res.is_ok(), "Transaction {nonce} should succeed, got: {res:?}");
        }

        let final_res = evm.transact(
            TxEnv::builder()
                .gas_limit(5_000_000)
                .gas_price(0)
                .caller(caller)
                .call(contract_address)
                .nonce(5)
                .build()
                .unwrap(),
        );

        match final_res {
            Err(err) => {
                assert!(format!("{err:?}").contains(JUMPDEST_BLOCK_LIMIT_ERR), "Expected block jumpdest limit hit, got: {err:?}");
            }
            Ok(result) => panic!("Transaction should fail once block jumpdest limit is exceeded, got: {result:?}"),
        }
    }

    #[test]
    fn test_limiter_resets_per_transaction() {
        let mut db = InMemoryDB::default();
        let contract_address = Address::random();
        
        // Create bytecode with 50 JUMPDESTs
        let code = create_bytecode_with_jumpdests(50);
        let bytecode = Bytecode::new_raw_checked(code).unwrap();
        
        db.insert_account_info(
            contract_address,
            AccountInfo {
                balance: U256::from(1_000_000),
                code_hash: bytecode.hash_slow(),
                code: Some(bytecode),
                nonce: 0,
            },
        );
        
        let evm_env = evm_env();
        let mut evm = TaikoEvmFactory.create_evm_with_inspector(
            db,
            evm_env,
            LimitingInspector::new(
                DEFAULT_TX_JUMPDEST_LIMIT,
                DEFAULT_BLOCK_JUMPDEST_LIMIT,
                NoOpInspector {},
            ),
        );
        
        // First transaction - should succeed
        let result1 = evm.transact(
            TxEnv::builder()
                .gas_limit(1_000_000)
                .gas_price(0)
                .caller(Address::random())
                .call(contract_address)
                .nonce(0)
                .build()
                .unwrap(),
        );
        assert!(result1.is_ok(), "First transaction should succeed: {:?}", result1);
        
        // Second transaction - should also succeed (counter reset)
        let result2 = evm.transact(
            TxEnv::builder()
                .gas_limit(1_000_000)
                .gas_price(0)
                .caller(Address::random())
                .call(contract_address)
                .nonce(1)
                .build()
                .unwrap(),
        );
        assert!(
            result2.is_ok(),
            "Second transaction should succeed (counter should reset): {:?}",
            result2
        );
    }

    #[test]
    fn test_jumpdest_limiter_exactly_at_limit() {
        let mut db = InMemoryDB::default();
        let contract_address = Address::random();
        
        // Create bytecode with exactly the default limit of JUMPDESTs
        let code = create_bytecode_with_jumpdests(DEFAULT_TX_JUMPDEST_LIMIT as usize);
        let bytecode = Bytecode::new_raw_checked(code).unwrap();
        
        db.insert_account_info(
            contract_address,
            AccountInfo {
                balance: U256::from(1_000_000),
                code_hash: bytecode.hash_slow(),
                code: Some(bytecode),
                nonce: 0,
            },
        );
        
        let evm_env = evm_env();
        let mut evm = TaikoEvmFactory.create_evm_with_inspector(
            db,
            evm_env,
            LimitingInspector::new(
                DEFAULT_TX_JUMPDEST_LIMIT,
                DEFAULT_BLOCK_JUMPDEST_LIMIT,
                NoOpInspector {},
            ),
        );
        
        let result = evm.transact(
            TxEnv::builder()
                .gas_limit(10_000_000)
                .gas_price(0)
                .caller(Address::random())
                .call(contract_address)
                .build()
                .unwrap(),
        );
        
        // Should succeed - exactly at limit
        assert!(result.is_ok(), "Transaction should succeed at exactly the limit");
    }
}
