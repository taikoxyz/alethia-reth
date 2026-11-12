//! Runner for the shared Unzen zk gas harness vectors.
//!
//! The vectors live in taiko-mono's `testdata/zk_gas_vectors`, found through
//! `ZK_GAS_VECTORS_DIR`. alethia-reth is the reference client: with `ZK_GAS_VECTORS_WRITE=1`
//! this runner writes each harness vector's `expected` values, otherwise it asserts them. Every
//! vector runs on both the plain (production) and the inspector metered path, which must agree.

use std::{
    any::Any,
    collections::BTreeMap,
    ffi::OsString,
    fmt, fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_evm::{Evm as AlloyEvm, EvmEnv, EvmFactory};
use alloy_primitives::{Address, Bytes, U64, U256, address};
use reth_evm::precompiles::PrecompilesMap;
use reth_revm::{
    Context, MainBuilder, MainContext,
    context::{
        BlockEnv, CfgEnv, TxEnv,
        result::{EVMError, ExecutionResult},
    },
    db::InMemoryDB,
    inspector::NoOpInspector,
    precompile::{PrecompileSpecId, Precompiles},
    primitives::TxKind,
    state::{AccountInfo, Bytecode, bytecode::opcode},
};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, DeserializeOwned, MapAccess, SeqAccess, Visitor},
};

use crate::{alloy::TaikoEvmWrapper, evm::TaikoEvm, factory::TaikoEvmFactory, spec::TaikoSpecId};

use super::{
    adapter::{ZK_GAS_LIMIT_ERR, ZkGasInspector},
    schedule::ZkGasSchedule,
    unzen::{BLOCK_ZK_GAS_LIMIT, UNZEN_ZK_GAS_SCHEDULE},
};

/// Chain id of the harness environment.
const HARNESS_CHAIN_ID: u64 = 167_001;

/// Intrinsic gas of a call with empty calldata: the harness tx gas limit is `frame_gas` plus this,
/// so the top frame starts with exactly `frame_gas`.
const EMPTY_CALL_INTRINSIC_GAS: u64 = 21_000;

/// One harness-layer vector file. Metadata fields (`cells`, `description`, ...) are checked by
/// `check.py` and ignored here.
#[derive(Debug, Deserialize)]
struct HarnessVector {
    /// Vector id, equal to the file name without `.json`.
    id: String,
    /// The single call frame to run.
    input: HarnessInput,
    /// Expected outputs; absent until reth's write mode generates them.
    expected: Option<HarnessExpected>,
}

/// One tx from `caller` to `to` with empty calldata, value 0, gas price 0 and base fee 0.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessInput {
    /// Pre-state accounts.
    alloc: BTreeMap<Address, AllocAccount>,
    /// Tx sender; its nonce comes from `alloc` (0 when absent).
    caller: Address,
    /// Called account.
    to: Address,
    /// Gas available to the top frame.
    frame_gas: U64,
    /// Block zk gas limit of the schedule; the intrinsic zk gas is not charged against it.
    zk_block_limit: U64,
    /// Block number.
    number: U256,
    /// Block timestamp.
    timestamp: U256,
    /// Block beneficiary (warm from the start of the tx).
    coinbase: Address,
}

/// Pre-state of one account. Only `balance` is required.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AllocAccount {
    /// Balance in wei.
    balance: U256,
    /// Account nonce.
    #[serde(default)]
    nonce: U64,
    /// Runtime bytecode; empty for an account without code.
    #[serde(default)]
    code: Bytes,
    /// Storage slot to value.
    #[serde(default)]
    storage: BTreeMap<U256, U256>,
}

/// Expected outputs of a harness vector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessExpected {
    /// Zk gas charged by the frame, excluding the per-tx intrinsic; `None` for `zk_limit`.
    frame_zk_gas: Option<U256>,
    /// How the frame ended.
    status: HarnessStatus,
}

/// How a harness frame ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HarnessStatus {
    /// The tx succeeded.
    Success,
    /// The tx reverted.
    Revert,
    /// The tx halted exceptionally.
    Halt,
    /// Metering exceeded the block zk gas limit and the tx aborted.
    ZkLimit,
}

/// What the corpus test does, from `ZK_GAS_VECTORS_WRITE`, `CI`, `ZK_GAS_VECTORS_DIR` and
/// `ZK_GAS_VECTORS_REQUIRED`.
#[derive(Debug, PartialEq, Eq)]
enum CorpusMode {
    /// No corpus configured.
    Skip,
    /// Compare every vector with its expected values.
    Assert(PathBuf),
    /// Write every vector's expected values.
    Write(PathBuf),
}

/// Decides the corpus test mode. Write mode (`ZK_GAS_VECTORS_WRITE=1`) is refused when `CI` is
/// set, and requires `ZK_GAS_VECTORS_DIR`; without write mode an unset directory skips, unless
/// `ZK_GAS_VECTORS_REQUIRED=1` makes it an error.
fn corpus_mode(
    write: Option<OsString>,
    ci: Option<OsString>,
    dir: Option<OsString>,
    required: Option<OsString>,
) -> Result<CorpusMode, String> {
    let write = write.is_some_and(|value| value == "1");
    if write && ci.is_some() {
        return Err("ZK_GAS_VECTORS_WRITE=1 is refused because CI is set: expected values are \
                    generated locally, never in CI"
            .to_owned());
    }
    let required = required.is_some_and(|value| value == "1");
    match (dir, write) {
        (None, false) if required => {
            Err("ZK_GAS_VECTORS_REQUIRED=1 needs ZK_GAS_VECTORS_DIR, the absolute path of the \
                 vector corpus"
                .to_owned())
        }
        (None, false) => Ok(CorpusMode::Skip),
        (None, true) => Err("ZK_GAS_VECTORS_WRITE=1 needs ZK_GAS_VECTORS_DIR, the absolute path \
                             of the vector corpus"
            .to_owned()),
        (Some(dir), false) => Ok(CorpusMode::Assert(dir.into())),
        (Some(dir), true) => Ok(CorpusMode::Write(dir.into())),
    }
}

/// Loads every vector of `layer` from `<dir>/cases/*.json`, sorted by path.
///
/// Fails when a file cannot be read or parsed, or when no vector of `layer` exists.
fn load_vectors<T: DeserializeOwned>(dir: &Path, layer: &str) -> Result<Vec<(PathBuf, T)>, String> {
    let cases = dir.join("cases");
    let list_error = |err: std::io::Error| format!("{}: {err}", cases.display());
    let mut paths = Vec::new();
    for entry in fs::read_dir(&cases).map_err(list_error)? {
        let path = entry.map_err(list_error)?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            paths.push(path);
        }
    }
    paths.sort();

    let mut vectors = Vec::new();
    for path in paths {
        let text = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        let Some(file_layer) = value.get("layer").and_then(serde_json::Value::as_str) else {
            return Err(format!("{}: missing string field `layer`", path.display()));
        };
        if file_layer != layer {
            continue;
        }
        let vector =
            serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        vectors.push((path, vector));
    }
    if vectors.is_empty() {
        return Err(format!("no {layer} vectors in {}", cases.display()));
    }
    Ok(vectors)
}

/// Runs `input` on the plain and the inspector metered path and returns their common result.
///
/// Panics with "reth plain/inspector divergence" when the two paths disagree, and when the tx
/// fails validation (a malformed vector).
fn run_harness(input: &HarnessInput) -> HarnessExpected {
    let schedule: &'static ZkGasSchedule = Box::leak(Box::new(ZkGasSchedule {
        block_limit: input.zk_block_limit.to(),
        ..UNZEN_ZK_GAS_SCHEDULE
    }));
    let plain = run_metered_path(input, schedule, false);
    let inspected = run_metered_path(input, schedule, true);
    if plain != inspected {
        panic!(
            "reth plain/inspector divergence: plain {}, inspector {}",
            to_json(&plain),
            to_json(&inspected)
        );
    }
    plain
}

/// Runs `input` on one metered path: the production plain loop, or the inspector adapter.
fn run_metered_path(
    input: &HarnessInput,
    schedule: &'static ZkGasSchedule,
    inspect: bool,
) -> HarnessExpected {
    let path = if inspect { "inspector" } else { "plain" };
    let mut evm = harness_evm(input, schedule, inspect);
    let outcome = evm.transact(harness_tx(input));
    let status = match outcome {
        Ok(result) => match result.result {
            ExecutionResult::Success { .. } => HarnessStatus::Success,
            ExecutionResult::Revert { .. } => HarnessStatus::Revert,
            ExecutionResult::Halt { .. } => HarnessStatus::Halt,
        },
        Err(EVMError::Custom(message)) if message == ZK_GAS_LIMIT_ERR => {
            return HarnessExpected { frame_zk_gas: None, status: HarnessStatus::ZkLimit };
        }
        Err(err) => panic!("harness tx failed on the {path} path: {err}"),
    };
    let meter = evm.meter().unwrap_or_else(|| panic!("{path} path installed no zk gas meter"));
    HarnessExpected { frame_zk_gas: Some(U256::from(meter.tx_zk_gas_used())), status }
}

/// Builds the EVM for one path, wired like [`TaikoEvmFactory`] but with `schedule` (whose block
/// limit comes from the vector) in place of the fork's static schedule.
fn harness_evm(
    input: &HarnessInput,
    schedule: &'static ZkGasSchedule,
    inspect: bool,
) -> TaikoEvmWrapper<InMemoryDB, NoOpInspector, PrecompilesMap> {
    let env = harness_env(input);
    let spec_id = env.cfg_env.spec;
    let evm = Context::mainnet()
        .with_cfg(env.cfg_env)
        .with_block(env.block_env)
        .with_db(harness_db(&input.alloc))
        .build_mainnet_with_inspector(ZkGasInspector::new(
            NoOpInspector {},
            inspect.then_some(schedule),
        ))
        .with_precompiles(PrecompilesMap::from_static(Precompiles::new(
            PrecompileSpecId::from_spec_id(spec_id.into()),
        )));
    let evm = TaikoEvm::new(evm);
    let evm = if inspect { evm } else { evm.with_zk_gas_schedule(Some(schedule)) };
    TaikoEvmWrapper::new(evm, inspect)
}

/// Unzen environment with the tx gas cap and the block gas limit raised to `u64::MAX` and a zero
/// base fee.
fn harness_env(input: &HarnessInput) -> EvmEnv<TaikoSpecId> {
    let mut cfg_env = CfgEnv::new()
        .with_chain_id(HARNESS_CHAIN_ID)
        .with_spec_and_mainnet_gas_params(TaikoSpecId::UNZEN);
    cfg_env.tx_gas_limit_cap = Some(u64::MAX);
    let block_env = BlockEnv {
        number: input.number,
        beneficiary: input.coinbase,
        timestamp: input.timestamp,
        gas_limit: u64::MAX,
        basefee: 0,
        ..Default::default()
    };
    EvmEnv::new(cfg_env, block_env)
}

/// In-memory pre-state holding every `alloc` account.
fn harness_db(alloc: &BTreeMap<Address, AllocAccount>) -> InMemoryDB {
    let mut db = InMemoryDB::default();
    for (address, account) in alloc {
        let mut info = AccountInfo {
            balance: account.balance,
            nonce: account.nonce.to(),
            ..Default::default()
        };
        if !account.code.is_empty() {
            let bytecode = Bytecode::new_raw_checked(account.code.clone())
                .unwrap_or_else(|err| panic!("invalid code at {address}: {err:?}"));
            info.code_hash = bytecode.hash_slow();
            info.code = Some(bytecode);
        }
        db.insert_account_info(*address, info);
        for (slot, value) in &account.storage {
            db.insert_account_storage(*address, *slot, *value)
                .expect("in-memory storage insert is infallible");
        }
    }
    db
}

/// The harness tx: `caller` calls `to` with empty calldata, value 0 and gas price 0.
fn harness_tx(input: &HarnessInput) -> TxEnv {
    let frame_gas: u64 = input.frame_gas.to();
    let gas_limit = frame_gas
        .checked_add(EMPTY_CALL_INTRINSIC_GAS)
        .unwrap_or_else(|| panic!("frame_gas {frame_gas} leaves no room for the intrinsic gas"));
    let nonce = input.alloc.get(&input.caller).map_or(0, |account| account.nonce.to());
    TxEnv::builder()
        .caller(input.caller)
        .kind(TxKind::Call(input.to))
        .chain_id(Some(HARNESS_CHAIN_ID))
        .nonce(nonce)
        .gas_limit(gas_limit)
        .gas_price(0)
        .value(U256::ZERO)
        .data(Bytes::new())
        .build()
        .expect("harness tx env is well formed")
}

/// Compares harness outputs. For `zk_limit` only the status is compared, because clients record
/// the partial zk gas of an aborted tx differently by design.
fn compare_harness(expected: &HarnessExpected, actual: &HarnessExpected) -> Result<(), String> {
    let matches = expected.status == actual.status &&
        (expected.status == HarnessStatus::ZkLimit ||
            expected.frame_zk_gas == actual.frame_zk_gas);
    if matches {
        Ok(())
    } else {
        Err(format!("expected {}, got {}", to_json(expected), to_json(actual)))
    }
}

/// Renders harness outputs as they appear in a vector file.
fn to_json(expected: &HarnessExpected) -> String {
    serde_json::to_string(expected).expect("harness expectations serialize")
}

/// Replaces the top-level `expected` key of the vector file at `path` (appending it when absent),
/// keeping every other key, its value and its position. Writes 2-space-indented JSON with a
/// trailing newline.
fn write_expected(path: &Path, expected: &serde_json::Value) -> Result<(), String> {
    let error = |err: &dyn fmt::Display| format!("{}: {err}", path.display());
    let text = fs::read_to_string(path).map_err(|err| error(&err))?;
    let OrderedJson::Object(mut entries) =
        serde_json::from_str::<OrderedJson>(&text).map_err(|err| error(&err))?
    else {
        return Err(error(&"top level is not a JSON object"));
    };
    let expected =
        serde_json::from_value::<OrderedJson>(expected.clone()).map_err(|err| error(&err))?;
    match entries.iter_mut().find(|(key, _)| key == "expected") {
        Some((_, value)) => *value = expected,
        None => entries.push(("expected".to_owned(), expected)),
    }
    let mut out =
        serde_json::to_string_pretty(&OrderedJson::Object(entries)).map_err(|err| error(&err))?;
    out.push('\n');
    fs::write(path, out).map_err(|err| error(&err))
}

/// A JSON value that keeps object keys in file order.
///
/// `serde_json::Map` sorts its keys unless serde_json's `preserve_order` feature is enabled, so
/// [`write_expected`] round-trips vector files through this type instead.
enum OrderedJson {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A number.
    Number(serde_json::Number),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<OrderedJson>),
    /// An object, in file order.
    Object(Vec<(String, OrderedJson)>),
}

impl Serialize for OrderedJson {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => value.serialize(serializer),
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(items) => serializer.collect_seq(items),
            Self::Object(entries) => serializer.collect_map(entries.iter().map(|(k, v)| (k, v))),
        }
    }
}

impl<'de> Deserialize<'de> for OrderedJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(OrderedJsonVisitor)
    }
}

/// Builds an [`OrderedJson`] from any self-describing input.
struct OrderedJsonVisitor;

impl<'de> Visitor<'de> for OrderedJsonVisitor {
    type Value = OrderedJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<OrderedJson, E> {
        Ok(OrderedJson::Null)
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<OrderedJson, E> {
        Ok(OrderedJson::Bool(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<OrderedJson, E> {
        Ok(OrderedJson::Number(value.into()))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<OrderedJson, E> {
        Ok(OrderedJson::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<OrderedJson, E> {
        serde_json::Number::from_f64(value)
            .map(OrderedJson::Number)
            .ok_or_else(|| E::custom("non-finite number"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<OrderedJson, E> {
        Ok(OrderedJson::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<OrderedJson, E> {
        Ok(OrderedJson::String(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<OrderedJson, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(OrderedJson::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<OrderedJson, A::Error> {
        let mut entries = Vec::new();
        while let Some(entry) = map.next_entry()? {
            entries.push(entry);
        }
        Ok(OrderedJson::Object(entries))
    }
}

/// Extracts the message of a caught panic.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|message| (*message).to_owned()))
        .unwrap_or_else(|| "panic with a non-string payload".to_owned())
}

/// Inline copy of the `smoke.harness.add` vector input: `PUSH1 1, PUSH1 2, ADD, STOP`.
fn inline_add_input() -> HarnessInput {
    let target = address!("0x000000000000000000000000000000000000c0de");
    let code = [opcode::PUSH1, 0x01, opcode::PUSH1, 0x02, opcode::ADD, opcode::STOP];
    HarnessInput {
        alloc: BTreeMap::from([(
            target,
            AllocAccount {
                balance: U256::ZERO,
                nonce: U64::ZERO,
                code: Bytes::copy_from_slice(&code),
                storage: BTreeMap::new(),
            },
        )]),
        caller: address!("0x000000000000000000000000000000000000ca11"),
        to: target,
        frame_gas: U64::from(100_000),
        zk_block_limit: U64::from(BLOCK_ZK_GAS_LIMIT),
        number: U256::from(1),
        timestamp: U256::from(12),
        coinbase: address!("0x0000000000000000000000000000000000c0ffee"),
    }
}

/// Creates an empty directory under the system temp dir, unique to this test run.
fn scratch_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock after epoch").as_nanos();
    let dir = std::env::temp_dir()
        .join(format!("alethia-zk-gas-vectors-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn harness_compare_rejects_mutated_expectations() {
    let actual = run_harness(&inline_add_input());
    assert_eq!(actual.status, HarnessStatus::Success);

    let mut mutated = actual.clone();
    mutated.frame_zk_gas = mutated.frame_zk_gas.map(|gas| gas + U256::from(1));
    assert!(compare_harness(&mutated, &actual).is_err(), "a changed frame_zk_gas must fail");

    let mut mutated = actual.clone();
    mutated.status = HarnessStatus::Revert;
    assert!(compare_harness(&mutated, &actual).is_err(), "a changed status must fail");

    assert!(compare_harness(&actual, &actual).is_ok());

    // For `zk_limit` only the status is compared.
    let limit = HarnessExpected { frame_zk_gas: None, status: HarnessStatus::ZkLimit };
    let limit_with_gas = HarnessExpected { frame_zk_gas: Some(U256::from(1)), ..limit.clone() };
    assert!(compare_harness(&limit, &limit_with_gas).is_ok());
    assert!(compare_harness(&limit, &actual).is_err(), "zk_limit must not match success");
}

#[test]
fn harness_add_charges_static_gas_times_multiplier() {
    // PUSH1, PUSH1, ADD, STOP: 3 x m(PUSH1) + 3 x m(PUSH1) + 3 x m(ADD) + 0 x m(STOP).
    let schedule = &UNZEN_ZK_GAS_SCHEDULE;
    let multiplier = |op: u8| u64::from(schedule.opcode_multipliers[usize::from(op)]);
    let want = 3 * multiplier(opcode::PUSH1) * 2 + 3 * multiplier(opcode::ADD);

    assert_eq!(run_harness(&inline_add_input()).frame_zk_gas, Some(U256::from(want)));
}

#[test]
fn harness_evm_meters_like_the_factory_at_the_unzen_block_limit() {
    // The runner builds its EVMs by hand so it can set the block limit. At the Unzen limit
    // they must meter exactly like the factory-built production and inspector EVMs.
    let input = inline_add_input();
    let want = run_harness(&input).frame_zk_gas;

    let mut plain = TaikoEvmFactory.create_evm(harness_db(&input.alloc), harness_env(&input));
    plain.transact(harness_tx(&input)).expect("factory plain path executes");
    let plain_zk_gas = plain.meter().expect("Unzen installs a meter").tx_zk_gas_used();

    let mut inspected = TaikoEvmFactory.create_evm_with_inspector(
        harness_db(&input.alloc),
        harness_env(&input),
        NoOpInspector {},
    );
    inspected.transact(harness_tx(&input)).expect("factory inspector path executes");
    let inspected_zk_gas = inspected.meter().expect("Unzen installs a meter").tx_zk_gas_used();

    assert_eq!(want, Some(U256::from(plain_zk_gas)));
    assert_eq!(want, Some(U256::from(inspected_zk_gas)));
}

#[test]
fn harness_input_defaults_missing_account_fields() {
    let input: HarnessInput = serde_json::from_str(
        r#"{
            "alloc": {
                "0x000000000000000000000000000000000000c0de": { "balance": "0x1" },
                "0x000000000000000000000000000000000000beef": {
                    "balance": "0x0",
                    "nonce": "0x2",
                    "code": "0x00",
                    "storage": { "0x0": "0x2a", "0x10": "0x1" }
                }
            },
            "caller": "0x000000000000000000000000000000000000ca11",
            "to": "0x000000000000000000000000000000000000c0de",
            "frame_gas": "0x186a0",
            "zk_block_limit": "0x5f5e100",
            "number": "0x1",
            "timestamp": "0xc",
            "coinbase": "0x0000000000000000000000000000000000c0ffee"
        }"#,
    )
    .expect("harness input parses");

    let bare = &input.alloc[&address!("0x000000000000000000000000000000000000c0de")];
    assert_eq!(bare.balance, U256::from(1));
    assert_eq!(bare.nonce, U64::ZERO);
    assert!(bare.code.is_empty());
    assert!(bare.storage.is_empty());

    let full = &input.alloc[&address!("0x000000000000000000000000000000000000beef")];
    assert_eq!(full.nonce, U64::from(2));
    assert_eq!(full.code, Bytes::from_static(&[opcode::STOP]));
    assert_eq!(
        full.storage,
        BTreeMap::from([(U256::ZERO, U256::from(42)), (U256::from(16), U256::from(1))])
    );
}

#[test]
fn load_vectors_fails_when_no_vectors_of_layer() {
    let dir = scratch_dir("load");
    let cases = dir.join("cases");
    fs::create_dir(&cases).expect("create cases dir");

    assert!(load_vectors::<HarnessVector>(&dir, "harness").is_err(), "an empty corpus must fail");

    fs::write(cases.join("only.block.json"), r#"{ "layer": "block" }"#).expect("write vector");
    assert!(
        load_vectors::<HarnessVector>(&dir, "harness").is_err(),
        "a corpus without harness vectors must fail"
    );

    fs::write(
        cases.join("one.harness.json"),
        r#"{
            "layer": "harness",
            "id": "one.harness",
            "input": {
                "alloc": {},
                "caller": "0x000000000000000000000000000000000000ca11",
                "to": "0x000000000000000000000000000000000000c0de",
                "frame_gas": "0x0",
                "zk_block_limit": "0x0",
                "number": "0x1",
                "timestamp": "0xc",
                "coinbase": "0x0000000000000000000000000000000000c0ffee"
            }
        }"#,
    )
    .expect("write vector");
    let vectors = load_vectors::<HarnessVector>(&dir, "harness").expect("one harness vector");
    assert_eq!(vectors.len(), 1);
    assert_eq!(vectors[0].0, cases.join("one.harness.json"));
    assert_eq!(vectors[0].1.id, "one.harness");
    assert!(vectors[0].1.expected.is_none());

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn write_expected_replaces_only_the_expected_key() {
    let dir = scratch_dir("write");
    let path = dir.join("vector.json");
    let new_expected = serde_json::json!({ "frame_zk_gas": "0x6f", "status": "success" });

    // An existing `expected` is replaced in place; every other key keeps its value and order.
    fs::write(
        &path,
        r#"{"schema_version": 1, "id": "v", "input": {"zeta": [1, 2], "alpha": "0x1"}, "expected": {"status": "halt"}, "cells": ["c"]}"#,
    )
    .expect("write vector");
    write_expected(&path, &new_expected).expect("write expected");
    assert_eq!(
        fs::read_to_string(&path).expect("read vector"),
        r#"{
  "schema_version": 1,
  "id": "v",
  "input": {
    "zeta": [
      1,
      2
    ],
    "alpha": "0x1"
  },
  "expected": {
    "frame_zk_gas": "0x6f",
    "status": "success"
  },
  "cells": [
    "c"
  ]
}
"#
    );

    // A vector without `expected` gets it appended as the last key.
    fs::write(&path, r#"{"id": "v", "layer": "harness"}"#).expect("write vector");
    write_expected(&path, &new_expected).expect("write expected");
    assert_eq!(
        fs::read_to_string(&path).expect("read vector"),
        r#"{
  "id": "v",
  "layer": "harness",
  "expected": {
    "frame_zk_gas": "0x6f",
    "status": "success"
  }
}
"#
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn corpus_mode_refuses_write_in_ci_and_without_a_dir() {
    let os = |value: &str| Some(OsString::from(value));
    let dir = PathBuf::from("/vectors");

    assert_eq!(corpus_mode(None, None, None, None), Ok(CorpusMode::Skip));
    assert_eq!(corpus_mode(None, None, None, os("0")), Ok(CorpusMode::Skip));
    assert_eq!(
        corpus_mode(None, os("true"), os("/vectors"), None),
        Ok(CorpusMode::Assert(dir.clone()))
    );
    assert_eq!(
        corpus_mode(os("0"), None, os("/vectors"), os("1")),
        Ok(CorpusMode::Assert(dir.clone()))
    );
    assert_eq!(corpus_mode(os("1"), None, os("/vectors"), None), Ok(CorpusMode::Write(dir)));
    assert!(
        corpus_mode(os("1"), os("true"), os("/vectors"), None).is_err(),
        "CI refuses write mode"
    );
    assert!(corpus_mode(os("1"), None, None, None).is_err(), "write mode needs ZK_GAS_VECTORS_DIR");
    assert!(
        corpus_mode(None, os("true"), None, os("1")).is_err(),
        "ZK_GAS_VECTORS_REQUIRED=1 needs ZK_GAS_VECTORS_DIR"
    );
}

#[test]
fn harness_vectors_match_expected() {
    let mode = corpus_mode(
        std::env::var_os("ZK_GAS_VECTORS_WRITE"),
        std::env::var_os("CI"),
        std::env::var_os("ZK_GAS_VECTORS_DIR"),
        std::env::var_os("ZK_GAS_VECTORS_REQUIRED"),
    )
    .unwrap_or_else(|err| panic!("{err}"));
    let (dir, write) = match mode {
        CorpusMode::Skip => {
            eprintln!("skip: ZK_GAS_VECTORS_DIR unset");
            return;
        }
        CorpusMode::Assert(dir) => (dir, false),
        CorpusMode::Write(dir) => (dir, true),
    };

    let vectors =
        load_vectors::<HarnessVector>(&dir, "harness").unwrap_or_else(|err| panic!("{err}"));
    let mut failures = Vec::new();
    for (path, vector) in &vectors {
        let actual = match catch_unwind(AssertUnwindSafe(|| run_harness(&vector.input))) {
            Ok(actual) => actual,
            Err(payload) => {
                failures.push(format!("{}: {}", vector.id, panic_message(payload.as_ref())));
                continue;
            }
        };
        if write {
            let value = serde_json::to_value(&actual).expect("harness expectations serialize");
            match write_expected(path, &value) {
                Ok(()) => eprintln!("wrote {}: {value}", vector.id),
                Err(err) => failures.push(format!("{}: {err}", vector.id)),
            }
            continue;
        }
        match &vector.expected {
            Some(expected) => {
                if let Err(err) = compare_harness(expected, &actual) {
                    failures.push(format!("{}: {err}", vector.id));
                }
            }
            None => failures.push(format!(
                "{}: no expected values; generate them with ZK_GAS_VECTORS_WRITE=1",
                vector.id
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} harness vectors failed:\n{}",
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
}
