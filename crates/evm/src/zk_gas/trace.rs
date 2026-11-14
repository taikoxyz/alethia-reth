//! zk gas traces for the shared Unzen vector corpus.
//!
//! Test-only tooling, compiled for this crate's tests and with the `zk-gas-trace` feature. The
//! meter and its call sites report every charge to a [`ZkGasTraceSink`];
//! [`TraceFactsInspector`] records the state each step starts from on the inspector path; the
//! functions here turn both into the lines of trace schema v1 (taiko-mono
//! `testdata/zk_gas_vectors/README.md`, "Trace schema v1"), one JSON object per line with its
//! fields in schema order.
//!
//! The charges of a trace come from the plain (production) path. The facts come from an inspector
//! run of the same input, which must report exactly the same charges; any difference is a
//! "reth plain/inspector trace divergence".

use std::{
    fmt::Write as _,
    fs, mem,
    path::Path,
    sync::{Arc, Mutex, PoisonError},
};

use alloy_primitives::{Address, B256, KECCAK256_EMPTY, U256};
use reth_revm::{
    DatabaseRef, Inspector,
    context::{ContextTr, JournalEntry, JournalInner, JournalTr},
    interpreter::{
        Interpreter,
        interpreter::EthInterpreter,
        interpreter_types::{InputsTr, Jumps, MemoryTr, ReturnData, RuntimeFlag},
    },
    state::{AccountInfo, Bytecode, bytecode::opcode},
};
use revm_database_interface::Database;

use crate::alloy::TaikoEvmContext;

/// One zk gas event of a metered execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZkGasTraceEvent {
    /// An interpreter step was charged.
    Step {
        /// Journal call depth of the step's frame (a tx's top frame is 1).
        depth: usize,
        /// Program counter of the step.
        pc: usize,
        /// Opcode of the step.
        opcode: u8,
        /// The frame's remaining gas before the step.
        gas_before: u64,
        /// The frame's remaining gas after the step, as fed to the meter.
        gas_after: u64,
        /// Whether the step opened a new frame and was charged its spawn estimate.
        spawned: bool,
        /// The zk gas charged, or the amount the meter refused when `ok` is false.
        zk_charge: u64,
        /// Whether the meter accepted the charge.
        ok: bool,
    },
    /// A precompile run was charged.
    Precompile {
        /// The call's bytecode address, which selects the precompile and its multiplier.
        address: Address,
        /// The gas the precompile was given.
        supplied: u64,
        /// The gas it had left.
        left: u64,
        /// Whether it returned without halting.
        success: bool,
        /// The zk gas charged, or the amount the meter refused when `ok` is false.
        zk_charge: u64,
        /// Whether the meter accepted the charge.
        ok: bool,
    },
    /// The per-tx intrinsic zk gas was charged.
    TxIntrinsic {
        /// The zk gas charged, or the amount the meter refused when `ok` is false.
        zk_charge: u64,
        /// Whether the meter accepted the charge.
        ok: bool,
    },
    /// The in-flight tx zk gas was committed into the block total.
    TxCommit {
        /// The committed tx total, intrinsic included.
        tx_zk: u64,
    },
    /// The in-flight tx zk gas was discarded.
    TxReset,
}

/// Receives the zk gas events of a meter (see `ZkGasMeter::set_trace_sink`).
pub trait ZkGasTraceSink: Send {
    /// Handles one event, in execution order.
    fn on_event(&mut self, event: ZkGasTraceEvent);
}

/// The state a step starts from, read before it runs and without side effects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepPre {
    /// Journal call depth of the frame (a tx's top frame is 1).
    pub depth: usize,
    /// Program counter.
    pub pc: usize,
    /// Opcode.
    pub opcode: u8,
    /// The frame's remaining gas.
    pub gas: u64,
    /// Whether the frame is static.
    pub is_static: bool,
    /// Stack length.
    pub stack_len: usize,
    /// The top `min(stack_len, inputs(opcode))` stack items, top first.
    pub stack: Vec<U256>,
    /// Memory size in bytes.
    pub memory_size: usize,
    /// The state facts the opcode records.
    pub facts: StepFacts,
}

/// The state facts of one step (trace schema v1, "Facts").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum StepFacts {
    /// The opcode records no facts, or its stack underflows.
    #[default]
    None,
    /// JUMP, JUMPI.
    Jump {
        /// Whether `stack[0]` is a valid JUMPDEST.
        jumpdest_valid: bool,
    },
    /// RETURNDATACOPY.
    ReturnDataCopy {
        /// Size of the return data buffer.
        returndata_size: usize,
    },
    /// SLOAD.
    Sload {
        /// Whether the slot is warm.
        slot_warm: bool,
    },
    /// SSTORE.
    Sstore {
        /// Whether the slot is warm.
        slot_warm: bool,
        /// The slot's value at the start of the tx.
        original: U256,
        /// The slot's current value.
        current: U256,
    },
    /// BALANCE, EXTCODESIZE, EXTCODEHASH, EXTCODECOPY.
    Account {
        /// Whether the account at `stack[0]` is warm.
        account_warm: bool,
    },
    /// CALL, CALLCODE, DELEGATECALL, STATICCALL.
    Call {
        /// Whether the target account (`stack[1]`) is warm.
        account_warm: bool,
        /// Whether the target account is empty.
        account_empty: bool,
        /// Whether the target's EIP-7702 delegate is warm; `None` without a delegation.
        delegate_warm: Option<bool>,
        /// Whether the delegate address equals the target; its load becomes warm after target
        /// loading.
        delegate_is_target: bool,
    },
    /// SELFDESTRUCT.
    SelfDestruct {
        /// Whether the beneficiary (`stack[0]`) is warm.
        account_warm: bool,
        /// Whether the beneficiary is empty.
        account_empty: bool,
        /// The balance of the destroyed contract.
        self_balance: U256,
    },
}

/// One entry of a [`TraceLog`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TraceRecord {
    /// The state a step starts from, from [`TraceFactsInspector`].
    Pre(StepPre),
    /// A zk gas event, from a meter.
    Event(ZkGasTraceEvent),
}

/// Shared, ordered record of the facts and zk gas events of one execution.
///
/// Clones share the same log, so one clone can be a meter's sink while another feeds a
/// [`TraceFactsInspector`] and a third reads the records.
#[derive(Clone, Debug, Default)]
pub struct TraceLog(Arc<Mutex<Vec<TraceRecord>>>);

impl TraceLog {
    /// Appends `record`.
    fn push(&self, record: TraceRecord) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push(record);
    }

    /// Removes and returns every record so far, in order.
    pub fn take(&self) -> Vec<TraceRecord> {
        mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl ZkGasTraceSink for TraceLog {
    fn on_event(&mut self, event: ZkGasTraceEvent) {
        self.push(TraceRecord::Event(event));
    }
}

/// Inner inspector that records the state each step starts from.
///
/// It reads in `step`, which runs before the opcode, and only reads: the interpreter, the
/// journal's state and warm sets, and [`DatabaseRef`] for what the journal has not loaded. It
/// never calls a [`Database`] method, which would fill the database cache.
#[derive(Clone, Debug)]
pub struct TraceFactsInspector {
    /// Where the facts go.
    log: TraceLog,
}

impl TraceFactsInspector {
    /// Creates an inspector that records into `log`.
    pub const fn new(log: TraceLog) -> Self {
        Self { log }
    }
}

impl<DB> Inspector<TaikoEvmContext<DB>, EthInterpreter> for TraceFactsInspector
where
    DB: Database + DatabaseRef,
{
    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut TaikoEvmContext<DB>,
    ) {
        self.log.push(TraceRecord::Pre(read_pre(interp, context)));
    }
}

/// Reads the state step `interp` starts from.
fn read_pre<DB: Database + DatabaseRef>(
    interp: &Interpreter<EthInterpreter>,
    context: &TaikoEvmContext<DB>,
) -> StepPre {
    let opcode = interp.bytecode.opcode();
    let data = interp.stack.data();
    let inputs = opcode::OpCode::new(opcode).map_or(0, |op| usize::from(op.inputs()));
    let stack: Vec<U256> = data.iter().rev().take(inputs).copied().collect();
    let facts = if data.len() < inputs {
        // The step halts on stack underflow before reading any state.
        StepFacts::None
    } else {
        let state = StateView {
            journal: &context.journaled_state.inner,
            db: &context.journaled_state.database,
        };
        read_facts(opcode, &stack, interp, &state)
    };
    StepPre {
        depth: context.journal_ref().depth(),
        pc: interp.bytecode.pc(),
        opcode,
        gas: interp.gas.remaining(),
        is_static: interp.runtime_flag.is_static(),
        stack_len: data.len(),
        stack,
        memory_size: interp.memory.size(),
        facts,
    }
}

/// Reads the facts of `opcode`, whose stack inputs (top first) are `stack`.
fn read_facts<DB: DatabaseRef>(
    opcode: u8,
    stack: &[U256],
    interp: &Interpreter<EthInterpreter>,
    state: &StateView<'_, DB>,
) -> StepFacts {
    let word_address = |word: U256| Address::from_word(B256::from(word.to_be_bytes::<32>()));
    match opcode {
        opcode::JUMP | opcode::JUMPI => {
            // revm saturates the target to `usize` and looks it up in the jump table.
            let target = stack[0].saturating_to::<usize>();
            let jumpdest_valid =
                interp.bytecode.legacy_jump_table().is_some_and(|table| table.is_valid(target));
            StepFacts::Jump { jumpdest_valid }
        }
        opcode::RETURNDATACOPY => {
            StepFacts::ReturnDataCopy { returndata_size: interp.return_data.buffer().len() }
        }
        opcode::SLOAD => {
            let (slot_warm, _, _) = state.slot(interp.input.target_address(), stack[0]);
            StepFacts::Sload { slot_warm }
        }
        opcode::SSTORE => {
            let (slot_warm, original, current) =
                state.slot(interp.input.target_address(), stack[0]);
            StepFacts::Sstore { slot_warm, original, current }
        }
        opcode::BALANCE | opcode::EXTCODESIZE | opcode::EXTCODEHASH | opcode::EXTCODECOPY => {
            StepFacts::Account { account_warm: state.account_warm(word_address(stack[0])) }
        }
        opcode::CALL | opcode::CALLCODE | opcode::DELEGATECALL | opcode::STATICCALL => {
            let target = word_address(stack[1]);
            let (info, account_empty) = state.account(target);
            let delegate = state.code(&info).eip7702_address();
            let delegate_warm = delegate.map(|delegate| state.account_warm(delegate));
            StepFacts::Call {
                account_warm: state.account_warm(target),
                account_empty,
                delegate_warm,
                delegate_is_target: delegate == Some(target),
            }
        }
        opcode::SELFDESTRUCT => {
            let beneficiary = word_address(stack[0]);
            let (_, account_empty) = state.account(beneficiary);
            let (contract, _) = state.account(interp.input.target_address());
            StepFacts::SelfDestruct {
                account_warm: state.account_warm(beneficiary),
                account_empty,
                self_balance: contract.balance,
            }
        }
        _ => StepFacts::None,
    }
}

/// Read-only view of the journal and the database behind it, mirroring how revm-context 43's
/// journal decides warmth and loads values (`JournalInner::load_account_mut_optional` and
/// `JournaledAccount::sload_concrete_error`) without changing either.
struct StateView<'a, DB> {
    /// The journal of the current tx.
    journal: &'a JournalInner<JournalEntry>,
    /// The database the journal loads from.
    db: &'a DB,
}

impl<DB: DatabaseRef> StateView<'_, DB> {
    /// Whether loading `address` now would be a warm access.
    fn account_warm(&self, address: Address) -> bool {
        match self.journal.state.get(&address) {
            Some(account) => {
                !(account.is_cold_transaction_id(self.journal.transaction_id) &&
                    self.journal.warm_addresses.is_cold(&address))
            }
            None => self.journal.warm_addresses.is_warm(&address),
        }
    }

    /// The account info `address` would load with now, and whether it is empty (EIP-161 aware).
    fn account(&self, address: Address) -> (AccountInfo, bool) {
        match self.journal.state.get(&address) {
            // A cold load of an account self-destructed in an earlier tx clears it first.
            Some(account)
                if account.is_cold_transaction_id(self.journal.transaction_id) &&
                    account.is_selfdestructed_locally() =>
            {
                (AccountInfo::default(), true)
            }
            Some(account) => {
                (account.info.clone(), account.state_clear_aware_is_empty(self.journal.cfg.spec))
            }
            None => match self.db.basic_ref(address) {
                Ok(Some(info)) => {
                    assert!(
                        info.account_id.is_none(),
                        "trace facts: account ids (block access lists) are not supported"
                    );
                    let empty = info.is_empty();
                    (info, empty)
                }
                Ok(None) => (AccountInfo::default(), true),
                Err(_) => panic!("trace facts: database read of account {address} failed"),
            },
        }
    }

    /// The code of `info`.
    fn code(&self, info: &AccountInfo) -> Bytecode {
        if let Some(code) = &info.code {
            return code.clone();
        }
        if info.code_hash == KECCAK256_EMPTY {
            return Bytecode::default();
        }
        self.db.code_by_hash_ref(info.code_hash).unwrap_or_else(|_| {
            panic!("trace facts: database read of code {} failed", info.code_hash)
        })
    }

    /// Whether loading slot `key` of the executing contract `address` now would be a warm access,
    /// with the slot's original (tx start) and current values.
    fn slot(&self, address: Address, key: U256) -> (bool, U256, U256) {
        let journal = self.journal;
        let tid = journal.transaction_id;
        let account = journal
            .state
            .get(&address)
            .expect("trace facts: the executing contract is loaded in the journal");
        assert!(
            !account.is_cold_transaction_id(tid),
            "trace facts: the executing contract {address} is not warm in the current tx"
        );
        let in_access_list = journal.warm_addresses.is_storage_warm(&address, &key);
        match account.storage.get(&key) {
            Some(slot) => {
                let warm = !slot.is_cold_transaction_id(tid) || in_access_list;
                // A slot last touched in an earlier tx is re-baselined when it is loaded.
                let original = if slot.transaction_id == tid {
                    slot.original_value
                } else {
                    slot.present_value
                };
                (warm, original, slot.present_value)
            }
            None => {
                let value = if account.is_created() {
                    U256::ZERO
                } else {
                    assert!(
                        account.info.account_id.is_none(),
                        "trace facts: account ids (block access lists) are not supported"
                    );
                    self.db.storage_ref(address, key).unwrap_or_else(|_| {
                        panic!("trace facts: database read of slot {key} of {address} failed")
                    })
                };
                (in_access_list, value, value)
            }
        }
    }
}

/// How a block tx ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxEndStatus {
    /// Committed into the block.
    Committed,
    /// A charge exceeded the block zk gas limit; the block ends here.
    Aborted,
    /// Invalid for another reason; its charges are discarded.
    Skipped,
}

impl TxEndStatus {
    /// The status as a trace spells it.
    const fn name(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Aborted => "aborted",
            Self::Skipped => "skipped",
        }
    }
}

/// One line of a trace file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TraceLine {
    /// `intrinsic`: a block tx starts.
    Intrinsic {
        /// The zk gas charged or refused.
        zk: u64,
        /// Whether the meter accepted the charge.
        ok: bool,
    },
    /// `step`: one interpreter step.
    Step {
        /// The state the step started from.
        pre: StepPre,
        /// The frame's gas after the step, as fed to the meter.
        gas_after: u64,
        /// Whether the step opened a new frame.
        spawn: bool,
        /// The zk gas charged or refused.
        zk: u64,
        /// Whether the meter accepted the charge.
        ok: bool,
    },
    /// `precompile`: one precompile run.
    Precompile {
        /// The call's bytecode address.
        address: Address,
        /// The gas the precompile was given.
        supplied: u64,
        /// The gas it had left.
        left: u64,
        /// Whether it returned without halting.
        success: bool,
        /// The zk gas charged or refused.
        zk: u64,
        /// Whether the meter accepted the charge.
        ok: bool,
    },
    /// `tx_end`: a block tx ends.
    TxEnd {
        /// Input index of the tx.
        index: usize,
        /// How it ended.
        status: TxEndStatus,
        /// The committed total; `None` unless committed.
        tx_zk: Option<u64>,
    },
    /// `frame_end`: a harness run ends.
    FrameEnd {
        /// The vector status (`success`, `revert`, `halt` or `zk_limit`).
        status: &'static str,
        /// The frame's total; `None` for `zk_limit`.
        frame_zk: Option<U256>,
    },
}

impl TraceLine {
    /// The line as trace schema v1 writes it: one JSON object, fields in schema order, integers
    /// as canonical 0x hex except `d`, `pc`, `op`, `sl` and `index`, addresses in lowercase.
    pub fn to_json(&self) -> String {
        match self {
            Self::Intrinsic { zk, ok } => {
                format!(r#"{{"e":"intrinsic","zk":"{zk:#x}","ok":{ok}}}"#)
            }
            Self::Step { pre, gas_after, spawn, zk, ok } => {
                let stack: Vec<String> =
                    pre.stack.iter().map(|item| format!(r#""{item:#x}""#)).collect();
                format!(
                    r#"{{"e":"step","d":{},"pc":{},"op":{},"gas":"{:#x}","static":{},"sl":{},"stack":[{}],"mem":"{:#x}","facts":{},"gas_after":"{gas_after:#x}","spawn":{spawn},"zk":"{zk:#x}","ok":{ok},"charged":true}}"#,
                    pre.depth,
                    pre.pc,
                    pre.opcode,
                    pre.gas,
                    pre.is_static,
                    pre.stack_len,
                    stack.join(","),
                    pre.memory_size,
                    facts_json(&pre.facts),
                )
            }
            Self::Precompile { address, supplied, left, success, zk, ok } => format!(
                r#"{{"e":"precompile","addr":"{address:#x}","supplied":"{supplied:#x}","left":"{left:#x}","success":{success},"zk":"{zk:#x}","ok":{ok}}}"#
            ),
            Self::TxEnd { index, status, tx_zk } => format!(
                r#"{{"e":"tx_end","index":{index},"status":"{}","tx_zk":{}}}"#,
                status.name(),
                tx_zk.map_or_else(|| "null".to_owned(), |zk| format!(r#""{zk:#x}""#)),
            ),
            Self::FrameEnd { status, frame_zk } => format!(
                r#"{{"e":"frame_end","status":"{status}","frame_zk":{}}}"#,
                frame_zk.map_or_else(|| "null".to_owned(), |zk| format!(r#""{zk:#x}""#)),
            ),
        }
    }
}

/// The `facts` object of a step, keys in schema order.
fn facts_json(facts: &StepFacts) -> String {
    match facts {
        StepFacts::None => "{}".to_owned(),
        StepFacts::Jump { jumpdest_valid } => format!(r#"{{"jumpdest_valid":{jumpdest_valid}}}"#),
        StepFacts::ReturnDataCopy { returndata_size } => {
            format!(r#"{{"returndata_size":"{returndata_size:#x}"}}"#)
        }
        StepFacts::Sload { slot_warm } => format!(r#"{{"slot_warm":{slot_warm}}}"#),
        StepFacts::Sstore { slot_warm, original, current } => format!(
            r#"{{"slot_warm":{slot_warm},"original":"{original:#x}","current":"{current:#x}"}}"#
        ),
        StepFacts::Account { account_warm } => format!(r#"{{"account_warm":{account_warm}}}"#),
        StepFacts::Call { account_warm, account_empty, delegate_warm, delegate_is_target } => {
            format!(
                r#"{{"account_warm":{account_warm},"account_empty":{account_empty},"delegate_warm":{}{}}}"#,
                delegate_warm.map_or_else(|| "null".to_owned(), |warm| warm.to_string()),
                if *delegate_is_target { r#","delegate_is_target":true"# } else { "" },
            )
        }
        StepFacts::SelfDestruct { account_warm, account_empty, self_balance } => format!(
            r#"{{"account_warm":{account_warm},"account_empty":{account_empty},"self_balance":"{self_balance:#x}"}}"#
        ),
    }
}

/// The trace file text of `lines`: one line each, every line ending with a newline.
pub fn render(lines: &[TraceLine]) -> String {
    let mut text = String::new();
    for line in lines {
        let _ = writeln!(text, "{}", line.to_json());
    }
    text
}

/// The zk gas events of `records`.
fn events(records: &[TraceRecord]) -> Vec<&ZkGasTraceEvent> {
    records
        .iter()
        .filter_map(|record| match record {
            TraceRecord::Event(event) => Some(event),
            TraceRecord::Pre(_) => None,
        })
        .collect()
}

/// Fails with "reth plain/inspector trace divergence" unless the inspector run's records
/// `replay` hold exactly the zk gas events of the plain run's records `plain`.
fn check_parity(plain: &[TraceRecord], replay: &[TraceRecord]) -> Result<(), String> {
    let plain = events(plain);
    let replay = events(replay);
    if plain == replay {
        return Ok(());
    }
    let at = plain.iter().zip(&replay).take_while(|(a, b)| a == b).count();
    Err(format!(
        "reth plain/inspector trace divergence at event {at}: plain {:?}, inspector {:?} \
         ({} plain events, {} inspector events)",
        plain.get(at),
        replay.get(at),
        plain.len(),
        replay.len()
    ))
}

/// The lines of the step and precompile events `charges` of the plain run, each step joined with
/// the next state the inspector run recorded in `replay`.
fn charge_lines(
    charges: &[&ZkGasTraceEvent],
    replay: &[TraceRecord],
) -> Result<Vec<TraceLine>, String> {
    let mut pres = replay.iter().filter_map(|record| match record {
        TraceRecord::Pre(pre) => Some(pre),
        TraceRecord::Event(_) => None,
    });
    let mut lines = Vec::with_capacity(charges.len());
    for (index, charge) in charges.iter().enumerate() {
        match **charge {
            ZkGasTraceEvent::Step {
                depth,
                pc,
                opcode,
                gas_before,
                gas_after,
                spawned,
                zk_charge,
                ok,
            } => {
                let pre = pres.next().ok_or_else(|| {
                    format!("trace facts: no recorded state for step event {index}")
                })?;
                if (pre.depth, pre.pc, pre.opcode, pre.gas) != (depth, pc, opcode, gas_before) {
                    return Err(format!(
                        "trace facts: the state recorded for step event {index} is of depth {} pc \
                         {} opcode {:#04x} gas {}, the step is of depth {depth} pc {pc} opcode \
                         {opcode:#04x} gas {gas_before}",
                        pre.depth, pre.pc, pre.opcode, pre.gas
                    ));
                }
                lines.push(TraceLine::Step {
                    pre: pre.clone(),
                    gas_after,
                    spawn: spawned,
                    zk: zk_charge,
                    ok,
                });
            }
            ZkGasTraceEvent::Precompile { address, supplied, left, success, zk_charge, ok } => {
                lines.push(TraceLine::Precompile {
                    address,
                    supplied,
                    left,
                    success,
                    zk: zk_charge,
                    ok,
                });
            }
            ref other => return Err(format!("unexpected zk gas event {other:?} inside a tx")),
        }
    }
    if let Some(pre) = pres.next() {
        return Err(format!("trace facts: a state was recorded for a step never charged: {pre:?}"));
    }
    Ok(lines)
}

/// The trace lines of a harness run: its step and precompile charges and the `frame_end` with the
/// run's `status` and `frame_zk`.
///
/// `plain` and `replay` are the records of the plain and the inspector run. Each run starts with
/// the wrapper's reset of the in-flight zk gas, which is not part of the trace.
pub fn harness_lines(
    plain: &[TraceRecord],
    replay: &[TraceRecord],
    status: &'static str,
    frame_zk: Option<U256>,
) -> Result<Vec<TraceLine>, String> {
    check_parity(plain, replay)?;
    let events = events(plain);
    let charges = match events.split_first() {
        Some((ZkGasTraceEvent::TxReset, rest)) => rest,
        _ => return Err(format!("a harness run starts with a zk gas reset, got {events:?}")),
    };
    let mut lines = charge_lines(charges, replay)?;
    lines.push(TraceLine::FrameEnd { status, frame_zk });
    Ok(lines)
}

/// The trace lines of block tx `index`, which ended with `status`: its intrinsic charge, its step
/// and precompile charges and its `tx_end`.
///
/// `plain` and `replay` are the records of the tx on the build and on the inspector replay. A tx
/// the executor rejects before its intrinsic charge has no events and only a `tx_end`.
pub fn block_tx_lines(
    index: usize,
    status: TxEndStatus,
    plain: &[TraceRecord],
    replay: &[TraceRecord],
) -> Result<Vec<TraceLine>, String> {
    check_parity(plain, replay)?;
    let tx_end = |tx_zk| TraceLine::TxEnd { index, status, tx_zk };
    let events = events(plain);
    if events.is_empty() {
        if status != TxEndStatus::Skipped {
            return Err(format!("a {} tx with no zk gas events", status.name()));
        }
        return Ok(vec![tx_end(None)]);
    }
    // The executor discards the previous tx's in-flight zk gas first.
    let events = match events.split_first() {
        Some((ZkGasTraceEvent::TxReset, rest)) => rest,
        _ => &events[..],
    };
    let Some((ZkGasTraceEvent::TxIntrinsic { zk_charge, ok }, rest)) = events.split_first() else {
        return Err(format!("a tx starts with its intrinsic charge, got {events:?}"));
    };
    let Some((last, charges)) = rest.split_last() else {
        return Err("a tx ends with a commit or a reset of its zk gas, got nothing".to_owned());
    };
    let mut lines = vec![TraceLine::Intrinsic { zk: *zk_charge, ok: *ok }];
    lines.extend(charge_lines(charges, replay)?);
    let rejected = !ok ||
        charges.iter().any(|event| {
            matches!(
                event,
                ZkGasTraceEvent::Step { ok: false, .. } |
                    ZkGasTraceEvent::Precompile { ok: false, .. }
            )
        });
    match (last, status) {
        (ZkGasTraceEvent::TxCommit { tx_zk }, TxEndStatus::Committed) if !rejected => {
            lines.push(tx_end(Some(*tx_zk)));
        }
        (ZkGasTraceEvent::TxReset, TxEndStatus::Aborted) if rejected => lines.push(tx_end(None)),
        (ZkGasTraceEvent::TxReset, TxEndStatus::Skipped) if !rejected => lines.push(tx_end(None)),
        (last, status) => {
            return Err(format!(
                "a {} tx whose last zk gas event is {last:?}{}",
                status.name(),
                if rejected { " after a rejected charge" } else { "" }
            ));
        }
    }
    Ok(lines)
}

/// Writes (`write`) or checks the trace file `traces/<id>.jsonl` of corpus `dir` against the
/// generated `trace`. Checking fails when the file is missing or differs, naming the first line
/// that differs.
pub fn sync_trace_file(dir: &Path, id: &str, trace: &str, write: bool) -> Result<(), String> {
    let traces = dir.join("traces");
    let path = traces.join(format!("{id}.jsonl"));
    if write {
        fs::create_dir_all(&traces).map_err(|err| format!("{}: {err}", traces.display()))?;
        return fs::write(&path, trace).map_err(|err| format!("{}: {err}", path.display()));
    }
    let committed = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            return Err(format!(
                "missing trace traces/{id}.jsonl ({err}); generate it with ZK_GAS_VECTORS_WRITE=1"
            ));
        }
    };
    match first_difference(&committed, trace) {
        None => Ok(()),
        Some(difference) => Err(format!(
            "traces/{id}.jsonl differs from the generated trace: {difference}; regenerate it with \
             ZK_GAS_VECTORS_WRITE=1 only for an intended change"
        )),
    }
}

/// Describes the first line where `committed` and `generated` differ, if any (lines counted from
/// 0, like the trace checker's event indices).
fn first_difference(committed: &str, generated: &str) -> Option<String> {
    if committed == generated {
        return None;
    }
    let mut want = committed.split('\n');
    let mut got = generated.split('\n');
    let mut line = 0;
    loop {
        match (want.next(), got.next()) {
            (Some(a), Some(b)) if a == b => line += 1,
            (a, b) => {
                let show = |text: Option<&str>| match text {
                    None => "end of file".to_owned(),
                    Some(text) => format!("`{text}`"),
                };
                return Some(format!("line {line}: committed {}, generated {}", show(a), show(b)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    #[test]
    fn lines_use_schema_field_order_and_canonical_hex() {
        let step = TraceLine::Step {
            pre: StepPre {
                depth: 1,
                pc: 4,
                opcode: opcode::SSTORE,
                gas: 0x186a0,
                is_static: false,
                stack_len: 3,
                stack: vec![U256::ZERO, U256::MAX],
                memory_size: 0x40,
                facts: StepFacts::Sstore {
                    slot_warm: false,
                    original: U256::ZERO,
                    current: U256::from(0x2a),
                },
            },
            gas_after: 0,
            spawn: false,
            zk: 0,
            ok: false,
        };
        assert_eq!(
            step.to_json(),
            concat!(
                r#"{"e":"step","d":1,"pc":4,"op":85,"gas":"0x186a0","static":false,"sl":3,"#,
                r#""stack":["0x0","0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"],"#,
                r#""mem":"0x40","facts":{"slot_warm":false,"original":"0x0","current":"0x2a"},"#,
                r#""gas_after":"0x0","spawn":false,"zk":"0x0","ok":false,"charged":true}"#
            )
        );
        // Addresses are lowercase, not EIP-55 checksummed.
        let precompile = TraceLine::Precompile {
            address: address!("0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"),
            supplied: 100,
            left: 0,
            success: false,
            zk: 85_900,
            ok: true,
        };
        assert_eq!(
            precompile.to_json(),
            r#"{"e":"precompile","addr":"0x7e5f4552091a69125d5dfcb7b8c2659029395bdf","supplied":"0x64","left":"0x0","success":false,"zk":"0x14f8c","ok":true}"#
        );
        let call = StepFacts::Call {
            account_warm: true,
            account_empty: false,
            delegate_warm: None,
            delegate_is_target: false,
        };
        assert_eq!(
            facts_json(&call),
            r#"{"account_warm":true,"account_empty":false,"delegate_warm":null}"#
        );
        assert_eq!(
            TraceLine::TxEnd { index: 2, status: TxEndStatus::Skipped, tx_zk: None }.to_json(),
            r#"{"e":"tx_end","index":2,"status":"skipped","tx_zk":null}"#
        );
        assert_eq!(
            TraceLine::FrameEnd { status: "zk_limit", frame_zk: None }.to_json(),
            r#"{"e":"frame_end","status":"zk_limit","frame_zk":null}"#
        );
        assert_eq!(render(&[]), "");
    }

    #[test]
    fn block_tx_lines_follow_the_executor_bracket() {
        let event = |event| TraceRecord::Event(event);
        let intrinsic = |ok| event(ZkGasTraceEvent::TxIntrinsic { zk_charge: 243_000, ok });
        let committed = [
            event(ZkGasTraceEvent::TxReset),
            intrinsic(true),
            event(ZkGasTraceEvent::TxCommit { tx_zk: 243_000 }),
        ];
        assert_eq!(
            render(&block_tx_lines(1, TxEndStatus::Committed, &committed, &committed).unwrap()),
            concat!(
                r#"{"e":"intrinsic","zk":"0x3b538","ok":true}"#,
                "\n",
                r#"{"e":"tx_end","index":1,"status":"committed","tx_zk":"0x3b538"}"#,
                "\n"
            )
        );
        // Rejected before the intrinsic charge: a lone skipped tx_end.
        assert_eq!(
            block_tx_lines(2, TxEndStatus::Skipped, &[], &[]).unwrap(),
            [TraceLine::TxEnd { index: 2, status: TxEndStatus::Skipped, tx_zk: None }]
        );
        assert!(block_tx_lines(2, TxEndStatus::Committed, &[], &[]).is_err());
        // An aborted tx needs a rejected charge, and a skipped one must not have one.
        let refused =
            [event(ZkGasTraceEvent::TxReset), intrinsic(false), event(ZkGasTraceEvent::TxReset)];
        assert!(block_tx_lines(3, TxEndStatus::Aborted, &refused, &refused).is_ok());
        assert!(block_tx_lines(3, TxEndStatus::Skipped, &refused, &refused).is_err());
        // The replay must report the same events.
        let err = block_tx_lines(1, TxEndStatus::Committed, &committed, &refused).unwrap_err();
        assert!(err.starts_with("reth plain/inspector trace divergence at event 1"), "{err}");
    }

    #[test]
    fn first_difference_names_the_line() {
        assert_eq!(first_difference("a\nb\n", "a\nb\n"), None);
        assert_eq!(
            first_difference("a\nb\n", "a\nc\n").as_deref(),
            Some("line 1: committed `b`, generated `c`")
        );
        assert_eq!(
            first_difference("a\n", "a\nb\n").as_deref(),
            Some("line 1: committed ``, generated `b`")
        );
    }
}
