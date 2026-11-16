//! Assert-only runner for Lean-generated production meter conformance vectors.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::Address;
use serde::{Deserialize, Deserializer, de};

use super::{
    meter::{ZkGasMeter, ZkGasOutcome, is_spawn_opcode},
    schedule::ZkGasSchedule,
    unzen::UNZEN_ZK_GAS_SCHEDULE,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MeterCorpus {
    schema_version: u64,
    schedule: String,
    vectors: Vec<MeterVector>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MeterVector {
    id: String,
    #[serde(deserialize_with = "hex_u64")]
    block_limit: u64,
    actions: Vec<MeterAction>,
    expected: Vec<MeterSnapshot>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum MeterAction {
    Opcode {
        op: u8,
        #[serde(deserialize_with = "hex_u64")]
        gas: u64,
    },
    Spawn {
        op: u8,
    },
    Precompile {
        #[serde(deserialize_with = "precompile_address")]
        address: Address,
        #[serde(deserialize_with = "hex_u64")]
        gas: u64,
    },
    Intrinsic {},
    Commit {},
    Reset {},
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct MeterSnapshot {
    ok: bool,
    #[serde(deserialize_with = "hex_u64")]
    tx_zk: u64,
    #[serde(deserialize_with = "hex_u64")]
    block_zk: u64,
}

fn hex_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = String::deserialize(deserializer)?;
    let digits =
        value.strip_prefix("0x").ok_or_else(|| de::Error::custom("expected canonical hex u64"))?;
    if digits.is_empty() ||
        digits.len() > 16 ||
        (digits.len() > 1 && digits.starts_with('0')) ||
        !digits.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(de::Error::custom("expected canonical hex u64"));
    }
    u64::from_str_radix(digits, 16).map_err(de::Error::custom)
}

fn precompile_address<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Address, D::Error> {
    let value = String::deserialize(deserializer)?;
    if value.len() != 42 ||
        !value.starts_with("0x") ||
        !value[2..].bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(de::Error::custom("expected lowercase 20-byte precompile address"));
    }
    value.parse().map_err(de::Error::custom)
}

const ADD_CORPUS: &str = r#"{
  "schema_version": 1,
  "schedule": "unzen",
  "vectors": [{
    "id": "add-three",
    "block_limit": "0x39",
    "actions": [{"kind": "opcode", "op": 1, "gas": "0x3"}],
    "expected": [{"ok": true, "tx_zk": "0x39", "block_zk": "0x0"}]
  }]
}"#;

fn parse_meter_json(text: &str) -> Result<MeterCorpus, String> {
    let corpus: MeterCorpus = serde_json::from_str(text).map_err(|err| err.to_string())?;
    if corpus.schema_version != 1 {
        return Err(format!("unsupported meter schema version {}", corpus.schema_version));
    }
    if corpus.schedule != "unzen" {
        return Err(format!("unsupported meter schedule {}", corpus.schedule));
    }
    if corpus.vectors.is_empty() {
        return Err("meter corpus must contain at least one vector".to_owned());
    }
    let mut ids = BTreeSet::new();
    for vector in &corpus.vectors {
        if vector.id.is_empty() || !ids.insert(&vector.id) {
            return Err(format!("empty or duplicate meter vector id {:?}", vector.id));
        }
        if vector.actions.is_empty() || vector.actions.len() != vector.expected.len() {
            return Err(format!(
                "{}: nonempty actions and equal-length expected arrays required",
                vector.id
            ));
        }
        for (index, action) in vector.actions.iter().enumerate() {
            if let MeterAction::Spawn { op } = action &&
                !is_spawn_opcode(*op)
            {
                return Err(format!("{}: action {index}: invalid spawn opcode {op:#x}", vector.id));
            }
        }
    }
    Ok(corpus)
}

fn check_meter_json(text: &str) -> Result<usize, String> {
    let corpus = parse_meter_json(text)?;
    for vector in &corpus.vectors {
        let schedule = ZkGasSchedule { block_limit: vector.block_limit, ..UNZEN_ZK_GAS_SCHEDULE };
        let mut meter = ZkGasMeter::new(&schedule);
        for (index, (action, expected)) in vector.actions.iter().zip(&vector.expected).enumerate() {
            let result = match action {
                MeterAction::Opcode { op, gas } => meter.charge_opcode(*op, *gas),
                MeterAction::Spawn { op } => meter.charge_spawn_opcode(*op),
                MeterAction::Precompile { address, gas } => meter.charge_precompile(address, *gas),
                MeterAction::Intrinsic {} => meter.charge_tx_intrinsic(),
                MeterAction::Commit {} => meter.commit_transaction(),
                MeterAction::Reset {} => {
                    meter.reset_transaction();
                    Ok(())
                }
            };
            let ok = match result {
                Ok(()) => true,
                Err(ZkGasOutcome::LimitExceeded) => false,
            };
            let actual = MeterSnapshot {
                ok,
                tx_zk: meter.tx_zk_gas_used(),
                block_zk: meter.block_zk_gas_used(),
            };
            if actual != *expected {
                return Err(format!(
                    "{}: action {index} {action:?}: expected {expected:?}, got {actual:?}",
                    vector.id
                ));
            }
        }
    }
    Ok(corpus.vectors.len())
}

fn check_meter_file(dir: &Path) -> Result<usize, String> {
    let path = dir.join("meter-vectors.json");
    let text = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    check_meter_json(&text).map_err(|err| format!("{}: {err}", path.display()))
}

fn meter_corpus_dir(
    dir: Option<OsString>,
    required: Option<OsString>,
) -> Result<Option<PathBuf>, String> {
    match dir {
        Some(dir) => Ok(Some(dir.into())),
        None if required.is_some_and(|value| value == "1") => {
            Err("ZK_GAS_VECTORS_REQUIRED=1 needs ZK_GAS_VECTORS_DIR".to_owned())
        }
        None => Ok(None),
    }
}

#[test]
fn meter_vectors_match_lean() {
    let dir = meter_corpus_dir(
        std::env::var_os("ZK_GAS_VECTORS_DIR"),
        std::env::var_os("ZK_GAS_VECTORS_REQUIRED"),
    )
    .unwrap_or_else(|err| panic!("{err}"));
    let Some(dir) = dir else {
        eprintln!("skip: ZK_GAS_VECTORS_DIR unset");
        return;
    };
    // Expectations are owned by Lean. Even ZK_GAS_VECTORS_WRITE=1 remains assert-only here.
    let count = check_meter_file(&dir).unwrap_or_else(|err| panic!("{err}"));
    eprintln!("checked {count} Lean meter vectors");
}

#[test]
fn meter_vectors_accept_literal_add() {
    assert_eq!(check_meter_json(ADD_CORPUS), Ok(1));
}

#[test]
fn meter_vectors_reject_changed_expectations() {
    for (before, after) in [
        (r#""ok": true"#, r#""ok": false"#),
        (r#""tx_zk": "0x39""#, r#""tx_zk": "0x38""#),
        (r#""block_zk": "0x0""#, r#""block_zk": "0x1""#),
    ] {
        let changed = ADD_CORPUS.replace(before, after);
        let err = check_meter_json(&changed).expect_err("altered Lean snapshot must fail");
        assert!(err.contains("add-three"), "missing vector context: {err}");
        assert!(err.contains("action 0"), "missing action context: {err}");
    }
}

#[test]
fn meter_vectors_reject_malformed_corpus() {
    for text in ["", "{", "null", "{}", r#"{"schema_version":1,"schedule":"unzen","vectors":[]}"#] {
        assert!(parse_meter_json(text).is_err(), "accepted malformed corpus: {text}");
    }
    let base: serde_json::Value = serde_json::from_str(ADD_CORPUS).expect("literal fixture");
    let changes = [
        ("/schema_version", serde_json::json!(2)),
        ("/schedule", serde_json::json!("other")),
        ("/vectors/0/id", serde_json::json!("")),
        ("/vectors/0/block_limit", serde_json::json!("0X39")),
        ("/vectors/0/block_limit", serde_json::json!("0x039")),
        ("/vectors/0/block_limit", serde_json::json!("0x10000000000000000")),
        ("/vectors/0/block_limit", serde_json::json!(57)),
        ("/vectors/0/actions", serde_json::json!([])),
        ("/vectors/0/expected", serde_json::json!([])),
        ("/vectors/0/actions/0", serde_json::json!({"kind":"unknown"})),
        ("/vectors/0/actions/0", serde_json::json!({"kind":"opcode","op":256,"gas":"0x3"})),
        ("/vectors/0/actions/0", serde_json::json!({"kind":"opcode","op":-1,"gas":"0x3"})),
        ("/vectors/0/actions/0", serde_json::json!({"kind":"opcode","op":1.5,"gas":"0x3"})),
        ("/vectors/0/actions/0", serde_json::json!({"kind":"spawn","op":1})),
        (
            "/vectors/0/actions/0",
            serde_json::json!({"kind":"precompile","address":"0x01","gas":"0x0"}),
        ),
        (
            "/vectors/0/actions/0",
            serde_json::json!({"kind":"precompile","address":"0x00000000000000000000000000000000000000AF","gas":"0x0"}),
        ),
        ("/vectors/0/actions/0/gas", serde_json::json!("0xG")),
        ("/vectors/0/actions/0/gas", serde_json::json!("0x")),
        ("/vectors/0/expected/0/tx_zk", serde_json::json!("0xA")),
        ("/vectors/0/expected/0/block_zk", serde_json::json!("0x00")),
    ];
    for (pointer, value) in changes {
        let mut changed = base.clone();
        *changed.pointer_mut(pointer).expect("fixture path") = value;
        assert!(
            parse_meter_json(&changed.to_string()).is_err(),
            "accepted malformed field {pointer}: {changed}"
        );
    }
    let mut duplicate = base;
    let vectors = duplicate["vectors"].as_array_mut().expect("vectors");
    vectors.push(vectors[0].clone());
    assert!(parse_meter_json(&duplicate.to_string()).is_err(), "accepted duplicate vector ids");
}

#[test]
fn meter_vectors_reject_unknown_fields() {
    let base: serde_json::Value = serde_json::from_str(ADD_CORPUS).expect("literal fixture");
    for pointer in ["", "/vectors/0", "/vectors/0/actions/0", "/vectors/0/expected/0"] {
        let mut changed = base.clone();
        changed
            .pointer_mut(pointer)
            .expect("fixture path")
            .as_object_mut()
            .expect("object")
            .insert("extra".to_owned(), serde_json::json!(0));
        assert!(
            parse_meter_json(&changed.to_string()).is_err(),
            "accepted unknown field at {pointer}"
        );
    }
    for kind in ["intrinsic", "commit", "reset"] {
        let mut changed = base.clone();
        changed["vectors"][0]["actions"][0] = serde_json::json!({"kind":kind, "extra":0});
        assert!(
            parse_meter_json(&changed.to_string()).is_err(),
            "accepted unknown field for {kind}"
        );
    }
}

#[test]
fn meter_vectors_reject_missing_empty_and_malformed_files() {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
    let dir =
        std::env::temp_dir().join(format!("reth-meter-vectors-{}-{stamp}", std::process::id()));
    fs::create_dir(&dir).expect("temporary corpus directory");
    let path = dir.join("meter-vectors.json");
    assert!(check_meter_file(&dir).is_err(), "missing corpus must fail");
    for contents in ["", "{", r#"{"schema_version":1,"schedule":"unzen","vectors":[]}"#] {
        fs::write(&path, contents).expect("write malformed fixture");
        assert!(check_meter_file(&dir).is_err(), "bad corpus file must fail");
    }
    fs::write(&path, ADD_CORPUS).expect("write valid fixture");
    assert_eq!(check_meter_file(&dir), Ok(1));
    fs::remove_dir_all(dir).expect("remove temporary corpus");
}

#[test]
fn meter_vectors_require_configured_corpus_when_required() {
    assert_eq!(meter_corpus_dir(None, None), Ok(None));
    assert!(meter_corpus_dir(None, Some("1".into())).is_err());
    assert_eq!(
        meter_corpus_dir(Some("/vectors".into()), Some("1".into())),
        Ok(Some(PathBuf::from("/vectors")))
    );
}
