//! Runner for the shared Unzen zk gas block vectors.
//!
//! The vectors live in taiko-mono's `testdata/zk_gas_vectors`, found through
//! `ZK_GAS_VECTORS_DIR`. alethia-reth is the reference client: with `ZK_GAS_VECTORS_WRITE=1`
//! this runner writes each block vector's `expected` values and its zk gas trace
//! (`traces/<id>.jsonl`), otherwise it asserts them.
//!
//! A vector's txs are signed and built into block 1 on top of a genesis parent through the
//! production legacy-mode path ([`execute_provided_transactions`]). The runner then checks that
//! the built block imports, that the built block plus the truncated tx (if any) and the built
//! block with its difficulty raised by one both fail import, and that prover-mode execution of the
//! full tx list commits the same txs with the same zk gas.
//!
//! The trace takes its charges from the build, whose meter reports every event. The state each
//! step starts from comes from a replay of the same txs on the same parent state through an
//! inspector EVM with [`TraceFactsInspector`]; the replay must charge exactly like the build.

use std::{
    any::Any,
    collections::BTreeMap,
    ffi::OsString,
    fmt, fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::Arc,
};

use alloy_consensus::{
    BlockBody, Header, SignableTransaction, Transaction as _, TxEip1559, proofs,
    transaction::SignerRecoverable,
};
use alloy_eips::Encodable2718;
use alloy_primitives::{
    Address, B256, BlockNumber, Bytes, StorageKey, StorageValue, TxKind, U64, U128, U256, address,
    keccak256,
};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use reth::{
    consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator},
    revm::cancelled::CancelOnDrop,
};
use reth_ethereum::{Block, EthPrimitives, TransactionSigned as EthTransactionSigned};
use reth_evm::{
    ConfigureEvm, EvmFactory,
    block::{BlockExecutionError, BlockExecutor, BlockExecutorFactory, CommitChanges, GasOutput},
    eth::receipt_builder::ReceiptBuilder,
    execute::{BlockBuilder, BlockBuilderOutcome, ExecutorTx},
};
use reth_primitives_traits::{
    Account, Bytecode, Recovered, RecoveredBlock, SealedBlock, SealedHeader,
};
use reth_provider::ProviderResult;
use reth_revm::{
    State,
    db::{BundleState, InMemoryDB, states::bundle_state::BundleRetention},
    state::{AccountInfo, Bytecode as RevmBytecode, bytecode::opcode},
};
use reth_storage_api::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, StateProofProvider,
    StateProvider, StateRootProvider, StorageRootProvider,
};
use reth_trie_common::{
    AccountProof, DecodedMultiProofV2, ExecutionWitnessMode, HashedPostState, HashedStorage,
    KeccakKeyHasher, MultiProof, MultiProofTargets, MultiProofTargetsV2, StorageMultiProof,
    StorageProof, TrieInput,
    root::{state_root_unsorted, storage_root_unsorted},
    updates::TrieUpdates,
};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, DeserializeOwned, MapAccess, SeqAccess, Visitor},
};

use alethia_reth_block::{
    config::{TaikoEvmConfig, TaikoNextBlockEnvAttributes},
    derived_block::execute_derived_block,
    executor::{TaikoBlockExecutor, is_zk_gas_difficulty_mismatch, is_zk_gas_limit_exceeded},
    testutil::{ExecutorBackedBuilder, unzen_chain_spec},
};
use alethia_reth_chainspec::{TAIKO_DEVNET, TAIKO_DEVNET_CHAIN_ID};
use alethia_reth_consensus::validation::{
    ANCHOR_V3_V4_GAS_LIMIT, ANCHOR_V4_SELECTOR, TaikoBeaconConsensus, TaikoBlockReader,
};
use alethia_reth_evm::{
    handler::get_treasury_address,
    zk_gas::{
        trace::{
            TraceFactsInspector, TraceLog, TraceRecord, TxEndStatus, block_tx_lines, render,
            sync_trace_file,
        },
        unzen::TX_INTRINSIC_ZK_GAS,
    },
};
use alethia_reth_primitives::addresses::TAIKO_GOLDEN_TOUCH_ADDRESS;

use super::execution::{
    ExecutionOutcome, execute_provided_transactions, tests::GOLDEN_TOUCH_PRIVATE_KEY,
};

/// Chain id of every block vector (Taiko devnet, with Shasta and Unzen active from genesis).
const VECTOR_CHAIN_ID: u64 = TAIKO_DEVNET_CHAIN_ID;

/// Verdict of an import that succeeded.
const ACCEPT: &str = "accept";

/// Verdict of an import that failed.
const REJECT: &str = "reject";

/// Prefix of an [`import`] error raised because a tx exceeded the block zk gas limit.
const REJECTED_ZK_GAS_LIMIT: &str = "zk gas limit";

/// Prefix of an [`import`] error raised because the header difficulty is not the block zk gas.
const REJECTED_ZK_GAS_DIFFICULTY: &str = "zk gas difficulty";

/// One block-layer vector file. Metadata fields (`cells`, `description`, ...) are checked by
/// `check.py` and ignored here.
#[derive(Debug, Deserialize)]
struct BlockVector {
    /// Vector id, equal to the file name without `.json`.
    id: String,
    /// The block to build.
    input: BlockInput,
    /// Expected outputs; absent until reth's write mode generates them.
    expected: Option<BlockExpected>,
}

/// A block on top of genesis: its environment, the pre-state and the txs to build it from.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockInput {
    /// Block environment.
    env: VectorEnv,
    /// Pre-state accounts; the whole pre-state of the block.
    alloc: BTreeMap<Address, AllocAccount>,
    /// Txs handed to the block builder, in order; `txs[0]` is the anchor.
    txs: Vec<TxInput>,
}

/// Environment of block 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VectorEnv {
    /// Chain id; must be [`VECTOR_CHAIN_ID`].
    chain_id: U64,
    /// Block number; must be 1.
    number: U64,
    /// Block timestamp.
    timestamp: U64,
    /// Block base fee per gas.
    base_fee: U64,
    /// Block beneficiary.
    coinbase: Address,
    /// Block gas limit.
    gas_limit: U64,
    /// `prevrandao` (header `mix_hash`).
    prev_randao: B256,
    /// Header extra data: the base fee sharing percentage and a 6-byte proposal id.
    extra_data: Bytes,
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

/// One EIP-1559 tx without an access list.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TxInput {
    /// Who signs the tx.
    signer: Signer,
    /// Tx nonce.
    nonce: U64,
    /// Callee; `None` is a contract creation.
    to: Option<Address>,
    /// Tx gas limit.
    gas: U64,
    /// Max fee per gas.
    max_fee: U128,
    /// Max priority fee per gas.
    max_priority_fee: U128,
    /// Value in wei.
    value: U256,
    /// Calldata, or init code for a creation.
    data: Bytes,
}

/// Signer of a vector tx: `"golden_touch"` or a JSON integer `k` in `1..2^32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signer {
    /// The golden touch account, which signs the anchor.
    GoldenTouch,
    /// The account whose private key is `k` as a 32-byte big-endian number.
    Key(u32),
}

impl<'de> Deserialize<'de> for Signer {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The JSON shapes a signer may take before validation.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            /// A signer name.
            Name(String),
            /// A private key number.
            Key(u64),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Name(name) if name == "golden_touch" => Ok(Self::GoldenTouch),
            Repr::Name(name) => Err(de::Error::custom(format!("unknown signer {name:?}"))),
            Repr::Key(key) => {
                u32::try_from(key).ok().filter(|key| *key != 0).map(Self::Key).ok_or_else(|| {
                    de::Error::custom(format!("signer key {key} is outside 1..2^32"))
                })
            }
        }
    }
}

impl Signer {
    /// Returns the local signer holding this signer's private key.
    fn private_key_signer(self) -> PrivateKeySigner {
        match self {
            Self::GoldenTouch => {
                GOLDEN_TOUCH_PRIVATE_KEY.parse().expect("golden touch private key parses")
            }
            Self::Key(key) => PrivateKeySigner::from_bytes(&B256::from(U256::from(key)))
                .expect("a nonzero key below 2^32 is a valid secp256k1 secret key"),
        }
    }
}

/// Address of signer `key`.
fn signer_address(key: u32) -> Address {
    Signer::Key(key).private_key_signer().address()
}

/// Expected outputs of a block vector, in vector-file field order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockExpected {
    /// Signed EIP-2718 bytes of every input tx, in input order.
    raw_txs: Vec<Bytes>,
    /// Input indices of the txs in the built block.
    included: Vec<usize>,
    /// First input index dropped because it exceeded the block zk gas limit.
    truncated_at: Option<usize>,
    /// Block zk gas total after each included tx.
    cumulative_zk_gas: Vec<U64>,
    /// Block zk gas total, equal to the header difficulty.
    block_zk_gas: U64,
    /// Post-block state root.
    state_root: B256,
    /// Verdict of importing the built block; always [`ACCEPT`].
    import_built: String,
    /// Verdict of importing the built block plus `txs[truncated_at]`; [`REJECT`] when truncated.
    import_extended: Option<String>,
    /// Verdict of importing the built block with difficulty + 1; always [`REJECT`].
    import_wrong_difficulty: String,
}

/// The built block and what the builder did with each input tx.
#[derive(Debug)]
struct BuildOutcome {
    /// Input indices of the committed txs.
    included: Vec<usize>,
    /// Input index of the tx that exceeded the block zk gas limit, if any.
    truncated_at: Option<usize>,
    /// Finalized block zk gas after each committed tx.
    cumulative_zk_gas: Vec<u64>,
    /// The built block.
    block: SealedBlock<Block>,
    /// The built block's state root.
    state_root: B256,
    /// What the builder did with each tx it executed, in input order.
    outcomes: Vec<TxOutcome>,
    /// The zk gas events the build's meter reported during each executed tx.
    tx_records: Vec<Vec<TraceRecord>>,
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

/// Returns the expected values of the vector `input`; see [`run_block_traced`].
fn run_block(input: &BlockInput) -> BlockExpected {
    run_block_traced(input).0
}

/// Builds the block of `input`, imports it and its two invalid variants, runs prover-mode
/// execution and replays the txs for the trace, then returns the expected values of the vector and
/// its trace file text.
///
/// Panics, naming the failed check, when the built block does not import, when an invalid variant
/// imports or fails for a reason other than zk gas, when build and prover disagree, when build and
/// the inspector replay disagree ("reth plain/inspector trace divergence"), and when the vector is
/// malformed (wrong environment, the anchor not included).
fn run_block_traced(input: &BlockInput) -> (BlockExpected, String) {
    check_env(&input.env);
    let txs = sign_txs(input);
    let built = build(input, &txs);

    if let Err(err) = import(input, &built.block) {
        panic!("reth import rejected the built block: {err}");
    }

    let import_extended = built.truncated_at.map(|index| {
        match import(input, &with_extra_tx(&built.block, &txs[index])) {
            Ok(()) => panic!("reth import accepted the built block plus truncated tx {index}"),
            Err(err)
                if err.starts_with(REJECTED_ZK_GAS_LIMIT) ||
                    err.starts_with(REJECTED_ZK_GAS_DIFFICULTY) =>
            {
                REJECT.to_owned()
            }
            Err(err) => panic!(
                "reth import rejected the built block plus truncated tx {index} for a reason \
                 other than zk gas: {err}"
            ),
        }
    });

    match import(input, &with_difficulty_plus_one(&built.block)) {
        Ok(()) => panic!("reth import accepted the built block with difficulty + 1"),
        Err(err) if err.starts_with(REJECTED_ZK_GAS_DIFFICULTY) => {}
        Err(err) => panic!(
            "reth import rejected the built block with difficulty + 1 for a reason other than the \
             zk gas difficulty: {err}"
        ),
    }

    let block_zk_gas = built.cumulative_zk_gas.last().copied().unwrap_or_default();
    let (prover_included, prover_zk_gas) = prove(input, &txs);
    if prover_included != built.included || prover_zk_gas != block_zk_gas {
        panic!(
            "reth build/prover divergence: build included {:?} with {block_zk_gas} zk gas, prover \
             included {prover_included:?} with {prover_zk_gas}",
            built.included
        );
    }

    let (replay_outcomes, replay_records) = replay(input, &txs);
    if replay_outcomes != built.outcomes {
        panic!(
            "reth plain/inspector trace divergence: the build's tx outcomes {:?}, the inspector \
             replay's {replay_outcomes:?}",
            built.outcomes
        );
    }
    let mut lines = Vec::new();
    for (index, outcome) in built.outcomes.iter().enumerate() {
        let status = match outcome {
            TxOutcome::Committed { .. } => TxEndStatus::Committed,
            TxOutcome::Skipped(_) => TxEndStatus::Skipped,
            TxOutcome::ZkGasLimit => TxEndStatus::Aborted,
        };
        let tx_lines =
            block_tx_lines(index, status, &built.tx_records[index], &replay_records[index])
                .unwrap_or_else(|err| panic!("tx {index}: {err}"));
        lines.extend(tx_lines);
    }

    let expected = BlockExpected {
        raw_txs: txs.iter().map(|tx| tx.encoded_2718().into()).collect(),
        included: built.included,
        truncated_at: built.truncated_at,
        cumulative_zk_gas: built.cumulative_zk_gas.into_iter().map(U64::from).collect(),
        block_zk_gas: U64::from(block_zk_gas),
        state_root: built.state_root,
        import_built: ACCEPT.to_owned(),
        import_extended,
        import_wrong_difficulty: REJECT.to_owned(),
    };
    (expected, render(&lines))
}

/// Panics unless `env` is the fixed block vector environment: chain [`VECTOR_CHAIN_ID`], block 1.
fn check_env(env: &VectorEnv) {
    let chain_id: u64 = env.chain_id.to();
    assert_eq!(chain_id, VECTOR_CHAIN_ID, "block vectors run on chain {VECTOR_CHAIN_ID}");
    let number: u64 = env.number.to();
    assert_eq!(number, 1, "block vectors build block 1 on top of genesis");
}

/// Signs every input tx as an EIP-1559 tx on the vector chain.
fn sign_txs(input: &BlockInput) -> Vec<Recovered<EthTransactionSigned>> {
    let chain_id = input.env.chain_id.to();
    input
        .txs
        .iter()
        .enumerate()
        .map(|(index, tx)| {
            let signer = tx.signer.private_key_signer();
            let unsigned = TxEip1559 {
                chain_id,
                nonce: tx.nonce.to(),
                gas_limit: tx.gas.to(),
                max_fee_per_gas: tx.max_fee.to(),
                max_priority_fee_per_gas: tx.max_priority_fee.to(),
                to: tx.to.map_or(TxKind::Create, TxKind::Call),
                value: tx.value,
                access_list: Default::default(),
                input: tx.data.clone(),
            };
            let signature = signer
                .sign_hash_sync(&unsigned.signature_hash())
                .unwrap_or_else(|err| panic!("tx {index}: signing failed: {err}"));
            let signed: EthTransactionSigned = unsigned.into_signed(signature).into();
            let recovered = signed
                .try_into_recovered()
                .unwrap_or_else(|err| panic!("tx {index}: signer recovery failed: {err}"));
            assert_eq!(recovered.signer(), signer.address(), "tx {index}: recovered signer");
            recovered
        })
        .collect()
}

/// Builds block 1 from `txs` through the production legacy-mode flow: `builder_for_next_block`,
/// pre-execution changes, [`execute_provided_transactions`] and `finish`.
fn build(input: &BlockInput, txs: &[Recovered<EthTransactionSigned>]) -> BuildOutcome {
    let evm_config = vector_evm_config();
    let parent = parent_header(input);
    let provider = VectorStateProvider::new(&input.alloc);
    let mut db =
        State::builder().with_database(vector_db(input, &parent)).with_bundle_update().build();
    let builder = evm_config
        .builder_for_next_block(&mut db, &parent, next_block_attributes(&input.env))
        .unwrap_or_else(|err| panic!("next block builder: {err}"));
    let log = TraceLog::default();
    let mut builder = RecordingBuilder::new(builder, log.clone());
    builder
        .evm_mut()
        .meter_mut()
        .expect("Unzen installs a zk gas meter")
        .set_trace_sink(Some(Box::new(log)));

    builder
        .apply_pre_execution_changes()
        .unwrap_or_else(|err| panic!("build: pre-execution changes failed: {err}"));
    let base_fee = builder.evm_mut().block.basefee;
    match execute_provided_transactions(&mut builder, txs, base_fee, &CancelOnDrop::default()) {
        Ok(ExecutionOutcome::Completed(_)) => {}
        Ok(ExecutionOutcome::Cancelled) => panic!("build: cancelled"),
        Err(err) => panic!("build: tx execution failed: {err}"),
    }
    let outcomes = std::mem::take(&mut builder.outcomes);
    let tx_records = std::mem::take(&mut builder.tx_records);
    let BlockBuilderOutcome { block, .. } =
        builder.finish(&provider, None).unwrap_or_else(|err| panic!("build: finish failed: {err}"));
    let block = block.into_sealed_block();

    assert!(
        outcomes.len() <= txs.len(),
        "build: {} tx outcomes recorded for {} input txs",
        outcomes.len(),
        txs.len()
    );
    let mut included = Vec::new();
    let mut cumulative_zk_gas = Vec::new();
    let mut truncated_at = None;
    for (index, outcome) in outcomes.iter().enumerate() {
        if let Some(at) = truncated_at {
            panic!("build: tx {index} executed after tx {at} exceeded the block zk gas limit");
        }
        match outcome {
            TxOutcome::Committed { block_zk_gas } => {
                included.push(index);
                cumulative_zk_gas.push(*block_zk_gas);
            }
            TxOutcome::Skipped(reason) if index == 0 => {
                panic!("build: the anchor (tx 0) was skipped: {reason}")
            }
            TxOutcome::Skipped(_) => {}
            TxOutcome::ZkGasLimit => truncated_at = Some(index),
        }
    }
    assert_eq!(included.first(), Some(&0), "build: the anchor (tx 0) must be included");
    let block_zk_gas = cumulative_zk_gas.last().copied().unwrap_or_default();
    assert_eq!(
        block.header().difficulty,
        U256::from(block_zk_gas),
        "build: the header difficulty must be the finalized block zk gas"
    );

    let state_root = block.header().state_root;
    BuildOutcome {
        included,
        truncated_at,
        cumulative_zk_gas,
        block,
        state_root,
        outcomes,
        tx_records,
    }
}

/// Replays `txs` like [`build`] on the same parent state, through an inspector EVM whose inner
/// inspector is [`TraceFactsInspector`], and returns what the executor did with each tx and the
/// trace records (facts and zk gas events) of each.
fn replay(
    input: &BlockInput,
    txs: &[Recovered<EthTransactionSigned>],
) -> (Vec<TxOutcome>, Vec<Vec<TraceRecord>>) {
    let evm_config = vector_evm_config();
    let parent = parent_header(input);
    let attributes = next_block_attributes(&input.env);
    let mut db =
        State::builder().with_database(vector_db(input, &parent)).with_bundle_update().build();
    let env = evm_config
        .next_evm_env(parent.header(), &attributes)
        .unwrap_or_else(|err| panic!("replay: next block env: {err}"));
    let ctx = evm_config
        .context_for_next_block(&parent, attributes)
        .unwrap_or_else(|err| panic!("replay: next block context: {err}"));
    let log = TraceLog::default();
    let mut evm = evm_config.evm_factory().create_evm_with_inspector(
        &mut db,
        env,
        TraceFactsInspector::new(log.clone()),
    );
    evm.meter_mut()
        .expect("Unzen installs a zk gas meter")
        .set_trace_sink(Some(Box::new(log.clone())));
    let executor = evm_config.block_executor_factory().create_executor(evm, ctx);
    let mut builder = RecordingBuilder::new(ExecutorBackedBuilder { executor }, log);

    builder
        .apply_pre_execution_changes()
        .unwrap_or_else(|err| panic!("replay: pre-execution changes failed: {err}"));
    let base_fee = builder.evm_mut().block.basefee;
    match execute_provided_transactions(&mut builder, txs, base_fee, &CancelOnDrop::default()) {
        Ok(ExecutionOutcome::Completed(_)) => {}
        Ok(ExecutionOutcome::Cancelled) => panic!("replay: cancelled"),
        Err(err) => panic!("replay: tx execution failed: {err}"),
    }
    (builder.outcomes, builder.tx_records)
}

/// Imports `block` on top of the vector's genesis parent and pre-state, like a node does: header
/// and pre-execution checks, then the manual execution loop (pre-execution changes, every tx,
/// post-execution changes, which validate the header difficulty), the state root, and the
/// post-execution consensus checks.
///
/// Execution goes through `execute_transaction` one tx at a time instead of `execute_block`,
/// whose `prover`-feature override silently skips failing txs. A rejection caused by zk gas
/// starts with [`REJECTED_ZK_GAS_LIMIT`] or [`REJECTED_ZK_GAS_DIFFICULTY`].
fn import(input: &BlockInput, block: &SealedBlock<Block>) -> Result<(), String> {
    let chain_spec = Arc::new(unzen_chain_spec());
    let evm_config = TaikoEvmConfig::new(chain_spec.clone());
    let parent = parent_header(input);
    let consensus = TaikoBeaconConsensus::new(
        chain_spec,
        Arc::new(GenesisReader { hash: parent.hash(), timestamp: parent.timestamp }),
    );
    let stage = |stage: &'static str| move |err: ConsensusError| format!("{stage}: {err}");

    consensus.validate_header(block.sealed_header()).map_err(stage("header"))?;
    consensus
        .validate_header_against_parent(block.sealed_header(), &parent)
        .map_err(stage("header against parent"))?;
    consensus.validate_block_pre_execution(block).map_err(stage("pre-execution checks"))?;
    let recovered = block.clone().try_recover().map_err(|err| format!("sender recovery: {err}"))?;

    let mut db =
        State::builder().with_database(vector_db(input, &parent)).with_bundle_update().build();
    let result = {
        let mut executor = evm_config
            .executor_for_block(&mut db, block)
            .map_err(|err| format!("block executor: {err}"))?;
        executor
            .apply_pre_execution_changes()
            .map_err(|err| execution_rejection("pre-execution changes", &err))?;
        for (index, tx) in recovered.transactions_recovered().enumerate() {
            executor
                .execute_transaction(tx)
                .map_err(|err| execution_rejection(&format!("tx {index}"), &err))?;
        }
        executor
            .apply_post_execution_changes()
            .map_err(|err| execution_rejection("post-execution changes", &err))?
    };

    db.merge_transitions(BundleRetention::Reverts);
    let provider = VectorStateProvider::new(&input.alloc);
    let hashed_state = provider
        .hashed_post_state(&db.bundle_state)
        .map_err(|err| format!("hashed post-state: {err}"))?;
    let state_root =
        provider.state_root(hashed_state).map_err(|err| format!("state root: {err}"))?;
    if state_root != block.header().state_root {
        return Err(format!(
            "state root: header has {}, execution produced {state_root}",
            block.header().state_root
        ));
    }

    FullConsensus::<EthPrimitives>::validate_block_post_execution(
        &consensus, &recovered, &result, None, None,
    )
    .map_err(stage("post-execution checks"))
}

/// Formats an import execution error, prefixing zk gas rejections with their [`import`] prefix.
fn execution_rejection(stage: &str, err: &BlockExecutionError) -> String {
    if is_zk_gas_limit_exceeded(err) {
        format!("{REJECTED_ZK_GAS_LIMIT}: {stage}: {err}")
    } else if is_zk_gas_difficulty_mismatch(err) {
        format!("{REJECTED_ZK_GAS_DIFFICULTY}: {stage}: {err}")
    } else {
        format!("{stage}: {err}")
    }
}

/// Executes a derived block holding every input tx in prover mode and returns the input indices
/// of the txs it committed and the finalized block zk gas.
fn prove(input: &BlockInput, txs: &[Recovered<EthTransactionSigned>]) -> (Vec<usize>, u64) {
    let evm_config = vector_evm_config();
    let parent = parent_header(input);
    let env = &input.env;
    let header = Header {
        parent_hash: parent.hash(),
        number: env.number.to(),
        timestamp: env.timestamp.to(),
        beneficiary: env.coinbase,
        mix_hash: env.prev_randao,
        gas_limit: env.gas_limit.to(),
        base_fee_per_gas: Some(env.base_fee.to()),
        extra_data: env.extra_data.clone(),
        parent_beacon_block_root: Some(B256::ZERO),
        ..Default::default()
    };
    let body = BlockBody {
        transactions: txs.iter().map(|tx| tx.inner().clone()).collect(),
        ommers: Vec::new(),
        withdrawals: None,
    };
    let derived = RecoveredBlock::new_unhashed(
        Block { header, body },
        txs.iter().map(Recovered::signer).collect(),
    );

    let outcome = execute_derived_block(&evm_config, &parent, &derived, vector_db(input, &parent))
        .unwrap_or_else(|err| panic!("prover execution failed: {err}"));

    // Committed txs are a subsequence of the input list.
    let mut included = Vec::new();
    let mut next = 0;
    for committed in &outcome.committed_transactions {
        let index = (next..txs.len())
            .find(|&index| txs[index].tx_hash() == committed.tx_hash())
            .unwrap_or_else(|| {
                panic!("prover committed tx {} out of input order", committed.tx_hash())
            });
        included.push(index);
        next = index + 1;
    }
    (included, outcome.finalized_block_zk_gas)
}

/// Returns `block` with `tx` appended to its body and the tx root recomputed; every other header
/// field, the difficulty included, is unchanged.
fn with_extra_tx(
    block: &SealedBlock<Block>,
    tx: &Recovered<EthTransactionSigned>,
) -> SealedBlock<Block> {
    let mut block = block.clone().into_block();
    block.body.transactions.push(tx.inner().clone());
    block.header.transactions_root = proofs::calculate_transaction_root(&block.body.transactions);
    SealedBlock::seal_slow(block)
}

/// Returns `block` with its header difficulty raised by one.
fn with_difficulty_plus_one(block: &SealedBlock<Block>) -> SealedBlock<Block> {
    let mut block = block.clone().into_block();
    block.header.difficulty += U256::from(1);
    SealedBlock::seal_slow(block)
}

/// EVM configuration of the vector chain: Taiko devnet with Shasta and Unzen from genesis.
fn vector_evm_config() -> TaikoEvmConfig {
    TaikoEvmConfig::new(Arc::new(unzen_chain_spec()))
}

/// The genesis parent of the vector block: number 0, timestamp 0, holding the vector alloc.
fn parent_header(input: &BlockInput) -> SealedHeader {
    SealedHeader::seal_slow(Header {
        number: 0,
        timestamp: 0,
        gas_limit: input.env.gas_limit.to(),
        base_fee_per_gas: Some(input.env.base_fee.to()),
        state_root: VectorStateProvider::new(&input.alloc).root(&HashedPostState::default()),
        ..Default::default()
    })
}

/// Block-building attributes of block 1, as the driver hands them to the payload builder.
fn next_block_attributes(env: &VectorEnv) -> TaikoNextBlockEnvAttributes {
    TaikoNextBlockEnvAttributes {
        timestamp: env.timestamp.to(),
        suggested_fee_recipient: env.coinbase,
        prev_randao: env.prev_randao,
        gas_limit: env.gas_limit.to(),
        extra_data: env.extra_data.clone(),
        base_fee_per_gas: env.base_fee.to(),
        // Taiko has no beacon chain: its blocks carry the zero root.
        parent_beacon_block_root: Some(B256::ZERO),
    }
}

/// In-memory pre-state holding every `alloc` account, with `BLOCKHASH(0)` resolving to the
/// parent hash.
fn vector_db(input: &BlockInput, parent: &SealedHeader) -> InMemoryDB {
    let mut db = InMemoryDB::default();
    for (address, account) in &input.alloc {
        let mut info = AccountInfo {
            balance: account.balance,
            nonce: account.nonce.to(),
            ..Default::default()
        };
        if !account.code.is_empty() {
            let bytecode = RevmBytecode::new_raw_checked(account.code.clone())
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
    // Without this entry the empty backing database would return a placeholder hash.
    db.cache.block_hashes.insert(U256::ZERO, parent.hash());
    db
}

/// Block reader for consensus checks of block 1: it knows only the genesis parent.
#[derive(Debug)]
struct GenesisReader {
    /// Genesis hash.
    hash: B256,
    /// Genesis timestamp.
    timestamp: u64,
}

impl TaikoBlockReader for GenesisReader {
    fn block_timestamp_by_hash(&self, hash: B256) -> Option<u64> {
        (hash == self.hash).then_some(self.timestamp)
    }
}

/// What the builder did with one tx it was handed.
#[derive(Debug, PartialEq, Eq)]
enum TxOutcome {
    /// Committed; the finalized block zk gas right after the commit.
    Committed {
        /// Finalized block zk gas after this tx.
        block_zk_gas: u64,
    },
    /// Rejected as invalid and skipped, with the error.
    Skipped(String),
    /// Exceeded the block zk gas limit; building stops here.
    ZkGasLimit,
}

/// Read access to the finalized block zk gas of a block executor.
trait FinalizedZkGas {
    /// Returns the zk gas of the txs committed so far.
    fn finalized_block_zk_gas(&self) -> u64;
}

impl<E, Spec, R: ReceiptBuilder> FinalizedZkGas for TaikoBlockExecutor<'_, E, Spec, R> {
    fn finalized_block_zk_gas(&self) -> u64 {
        self.ctx.finalized_block_zk_gas()
    }
}

/// Block builder that records the outcome and the trace records of every tx it executes, in call
/// order.
struct RecordingBuilder<B> {
    /// The production block builder.
    inner: B,
    /// One entry per executed tx.
    outcomes: Vec<TxOutcome>,
    /// The log the EVM's meter (and facts inspector, if any) reports to.
    log: TraceLog,
    /// The records `log` received during each executed tx.
    tx_records: Vec<Vec<TraceRecord>>,
}

impl<B> RecordingBuilder<B> {
    /// Wraps `inner`, whose EVM reports to `log`.
    const fn new(inner: B, log: TraceLog) -> Self {
        Self { inner, outcomes: Vec::new(), log, tx_records: Vec::new() }
    }
}

impl<B> BlockBuilder for RecordingBuilder<B>
where
    B: BlockBuilder,
    B::Executor: FinalizedZkGas,
{
    type Primitives = B::Primitives;
    type Executor = B::Executor;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        let result = self.inner.apply_pre_execution_changes();
        // The system calls before the first tx are not part of any tx's trace.
        self.log.take();
        result
    }

    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutorTx<Self::Executor>,
        f: impl FnOnce(&<Self::Executor as BlockExecutor>::Result) -> CommitChanges,
    ) -> Result<Option<GasOutput>, BlockExecutionError> {
        let result = self.inner.execute_transaction_with_commit_condition(tx, f);
        self.outcomes.push(match &result {
            Ok(Some(_)) => TxOutcome::Committed {
                block_zk_gas: self.inner.executor().finalized_block_zk_gas(),
            },
            Ok(None) => TxOutcome::Skipped("not committed".to_owned()),
            Err(err) if is_zk_gas_limit_exceeded(err) => TxOutcome::ZkGasLimit,
            Err(err) => TxOutcome::Skipped(err.to_string()),
        });
        self.tx_records.push(self.log.take());
        result
    }

    fn finish(
        self,
        state_provider: impl StateProvider,
        state_root_precomputed: Option<(B256, TrieUpdates)>,
    ) -> Result<BlockBuilderOutcome<Self::Primitives>, BlockExecutionError> {
        self.inner.finish(state_provider, state_root_precomputed)
    }

    fn executor_mut(&mut self) -> &mut Self::Executor {
        self.inner.executor_mut()
    }

    fn executor(&self) -> &Self::Executor {
        self.inner.executor()
    }

    fn into_executor(self) -> Self::Executor {
        self.inner.into_executor()
    }
}

/// State provider over a vector's pre-state that computes real state roots.
///
/// `MockEthProvider` returns placeholder state roots, so `finish` and [`import`] use this
/// provider instead. It serves only what they read: the hashed post-state of an execution bundle
/// and the state root of a post-state on top of the vector alloc.
#[derive(Debug, Default)]
struct VectorStateProvider {
    /// Pre-state accounts by hashed address.
    accounts: BTreeMap<B256, Account>,
    /// Pre-state non-zero storage by hashed address and hashed slot.
    storages: BTreeMap<B256, BTreeMap<B256, U256>>,
}

impl VectorStateProvider {
    /// Hashes the `alloc` pre-state.
    fn new(alloc: &BTreeMap<Address, AllocAccount>) -> Self {
        let mut provider = Self::default();
        for (address, account) in alloc {
            let hashed_address = keccak256(address);
            provider.accounts.insert(
                hashed_address,
                Account {
                    nonce: account.nonce.to(),
                    balance: account.balance,
                    bytecode_hash: (!account.code.is_empty()).then(|| keccak256(&account.code)),
                },
            );
            let storage = account
                .storage
                .iter()
                .filter(|(_, value)| !value.is_zero())
                .map(|(slot, value)| (keccak256(B256::from(*slot)), *value))
                .collect();
            provider.storages.insert(hashed_address, storage);
        }
        provider
    }

    /// Returns the state root of `post` applied on top of the pre-state.
    fn root(&self, post: &HashedPostState) -> B256 {
        let mut accounts = self.accounts.clone();
        let mut storages = self.storages.clone();
        for (hashed_address, account) in &post.accounts {
            match account {
                Some(account) => {
                    accounts.insert(*hashed_address, *account);
                }
                None => {
                    accounts.remove(hashed_address);
                    storages.remove(hashed_address);
                }
            }
        }
        for (hashed_address, storage) in &post.storages {
            let slots = storages.entry(*hashed_address).or_default();
            for (slot, value) in &storage.storage {
                if value.is_zero() {
                    slots.remove(slot);
                } else {
                    slots.insert(*slot, *value);
                }
            }
        }
        state_root_unsorted(accounts.into_iter().map(|(hashed_address, account)| {
            let storage_root =
                storage_root_unsorted(storages.remove(&hashed_address).unwrap_or_default());
            (hashed_address, account.into_trie_account(storage_root))
        }))
    }
}

impl HashedPostStateProvider for VectorStateProvider {
    /// Hashes the bundle, and zeroes the pre-state storage of destroyed accounts like reth's
    /// database-backed providers do.
    fn hashed_post_state(&self, bundle_state: &BundleState) -> ProviderResult<HashedPostState> {
        let mut hashed =
            HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state());
        for (address, account) in bundle_state.state() {
            let hashed_address = keccak256(address);
            let Some(slots) = self.storages.get(&hashed_address) else { continue };
            if account.was_destroyed() {
                let storage = hashed.storages.entry(hashed_address).or_default();
                for slot in slots.keys() {
                    storage.storage.entry(*slot).or_insert(U256::ZERO);
                }
            }
        }
        Ok(hashed)
    }
}

impl StateRootProvider for VectorStateProvider {
    fn state_root(&self, hashed_state: HashedPostState) -> ProviderResult<B256> {
        Ok(self.root(&hashed_state))
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        unreachable!("the block vector runner only computes plain state roots")
    }

    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok((self.root(&hashed_state), TrieUpdates::default()))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        unreachable!("the block vector runner only computes plain state roots")
    }
}

impl StorageRootProvider for VectorStateProvider {
    fn storage_root(
        &self,
        _address: Address,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        unreachable!("the block vector runner reads no storage roots")
    }

    fn storage_proof(
        &self,
        _address: Address,
        _slot: B256,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        unreachable!("the block vector runner reads no proofs")
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        unreachable!("the block vector runner reads no proofs")
    }
}

impl StateProofProvider for VectorStateProvider {
    fn proof(
        &self,
        _input: TrieInput,
        _address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        unreachable!("the block vector runner reads no proofs")
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        unreachable!("the block vector runner reads no proofs")
    }

    fn multiproof_v2(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargetsV2,
    ) -> ProviderResult<DecodedMultiProofV2> {
        unreachable!("the block vector runner reads no proofs")
    }

    fn witness(
        &self,
        _input: TrieInput,
        _target: HashedPostState,
        _mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        unreachable!("the block vector runner reads no witnesses")
    }
}

impl BlockHashReader for VectorStateProvider {
    fn block_hash(&self, _number: BlockNumber) -> ProviderResult<Option<B256>> {
        unreachable!("block execution reads block hashes from the vector database")
    }

    fn canonical_hashes_range(
        &self,
        _start: BlockNumber,
        _end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        unreachable!("block execution reads block hashes from the vector database")
    }
}

impl AccountReader for VectorStateProvider {
    fn basic_account(&self, _address: &Address) -> ProviderResult<Option<Account>> {
        unreachable!("block execution reads accounts from the vector database")
    }
}

impl BytecodeReader for VectorStateProvider {
    fn bytecode_by_hash(&self, _code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        unreachable!("block execution reads code from the vector database")
    }
}

impl StateProvider for VectorStateProvider {
    fn storage(
        &self,
        _account: Address,
        _storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        unreachable!("block execution reads storage from the vector database")
    }
}

/// Compares block outputs field by field; the error names every field that differs.
fn compare_block(expected: &BlockExpected, actual: &BlockExpected) -> Result<(), String> {
    if expected == actual {
        return Ok(());
    }
    let (serde_json::Value::Object(want), serde_json::Value::Object(got)) = (
        serde_json::to_value(expected).expect("block expectations serialize"),
        serde_json::to_value(actual).expect("block expectations serialize"),
    ) else {
        unreachable!("block expectations serialize to JSON objects")
    };
    let differences: Vec<String> = want
        .iter()
        .filter(|(field, value)| got.get(*field) != Some(*value))
        .map(|(field, value)| format!("{field}: expected {value}, got {}", got[field]))
        .collect();
    Err(differences.join("; "))
}

/// Renders block outputs as they appear in a vector file.
fn to_json(expected: &BlockExpected) -> String {
    serde_json::to_string(expected).expect("block expectations serialize")
}

/// Replaces the top-level `expected` key of the vector file at `path` (appending it when absent),
/// keeping every other key, its value and its position. Writes 2-space-indented JSON with a
/// trailing newline.
///
/// `expected` is serialized straight into the order-keeping [`OrderedJson`], so a struct keeps
/// its field order; a `serde_json::Value` would sort the keys.
fn write_expected(path: &Path, expected: &impl Serialize) -> Result<(), String> {
    let error = |err: &dyn fmt::Display| format!("{}: {err}", path.display());
    let text = fs::read_to_string(path).map_err(|err| error(&err))?;
    let OrderedJson::Object(mut entries) =
        serde_json::from_str::<OrderedJson>(&text).map_err(|err| error(&err))?
    else {
        return Err(error(&"top level is not a JSON object"));
    };
    let expected = serde_json::to_string(expected)
        .and_then(|json| serde_json::from_str::<OrderedJson>(&json))
        .map_err(|err| error(&err))?;
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

/// Base fee of every block vector (25,000,000 wei, the Shasta initial base fee).
const VECTOR_BASE_FEE: u64 = 25_000_000;

/// Address of signer `1`.
const SIGNER_1: Address = address!("0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf");

/// The fixed block vector environment.
fn inline_env() -> VectorEnv {
    VectorEnv {
        chain_id: U64::from(VECTOR_CHAIN_ID),
        number: U64::from(1),
        timestamp: U64::from(12),
        base_fee: U64::from(VECTOR_BASE_FEE),
        coinbase: address!("0x0000000000000000000000000000000000c0ffee"),
        gas_limit: U64::from(50_000_000),
        prev_randao: B256::repeat_byte(0x2a),
        extra_data: Bytes::from_static(&[0x4b, 0, 0, 0, 0, 0, 1]),
    }
}

/// An account holding `balance` wei and `code`.
fn account(balance: U256, code: &[u8]) -> AllocAccount {
    AllocAccount {
        balance,
        nonce: U64::ZERO,
        code: Bytes::copy_from_slice(code),
        storage: BTreeMap::new(),
    }
}

/// The anchor tx every block vector starts with.
fn anchor_tx() -> TxInput {
    TxInput {
        signer: Signer::GoldenTouch,
        nonce: U64::ZERO,
        to: Some(get_treasury_address(VECTOR_CHAIN_ID)),
        gas: U64::from(ANCHOR_V3_V4_GAS_LIMIT),
        max_fee: U128::from(VECTOR_BASE_FEE),
        max_priority_fee: U128::ZERO,
        value: U256::ZERO,
        data: Bytes::copy_from_slice(ANCHOR_V4_SELECTOR),
    }
}

/// A zero-tip tx from signer `key` with nonce 0.
fn user_tx(key: u32, to: Address, gas: u64, value: u64, data: &[u8]) -> TxInput {
    TxInput {
        signer: Signer::Key(key),
        nonce: U64::ZERO,
        to: Some(to),
        gas: U64::from(gas),
        max_fee: U128::from(VECTOR_BASE_FEE),
        max_priority_fee: U128::ZERO,
        value: U256::from(value),
        data: Bytes::copy_from_slice(data),
    }
}

/// Inline copy of the `smoke.block.transfer` vector input: the anchor plus a 1-wei transfer from
/// signer 1 to an empty account.
fn inline_transfer_input() -> BlockInput {
    let ether = U256::from(1_000_000_000_000_000_000_u64);
    BlockInput {
        env: inline_env(),
        alloc: BTreeMap::from([
            (get_treasury_address(VECTOR_CHAIN_ID), account(U256::ZERO, &[opcode::STOP])),
            (SIGNER_1, account(ether, &[])),
            (address!("0x0000000000000000000000000000000000c0ffee"), account(U256::from(1), &[])),
        ]),
        txs: vec![
            anchor_tx(),
            user_tx(1, address!("0x000000000000000000000000000000000000beef"), 21_000, 1, &[]),
        ],
    }
}

/// The anchor; tx 1 runs a contract that makes two STATICCALLs to the point evaluation
/// precompile (0x0a), each with 100,000 gas and empty input; tx 2 is a valid transfer (the
/// sentinel).
///
/// A failing precompile call consumes all the gas it was given, and the Unzen point evaluation
/// multiplier is 859, so each call costs about 85.9M zk gas and the second one exceeds the 100M
/// block limit.
fn inline_truncation_input() -> BlockInput {
    let ether = U256::from(1_000_000_000_000_000_000_u64);
    let target = address!("0x000000000000000000000000000000000000c0de");
    // STATICCALL(gas = 100,000, to = 0x0a, args = empty, ret = empty), then POP the status.
    let point_evaluation_call = [
        opcode::PUSH1,
        0x00, // retLength
        opcode::PUSH1,
        0x00, // retOffset
        opcode::PUSH1,
        0x00, // argsLength
        opcode::PUSH1,
        0x00, // argsOffset
        opcode::PUSH1,
        0x0a, // point evaluation precompile
        opcode::PUSH3,
        0x01,
        0x86,
        0xa0, // 100,000 gas
        opcode::STATICCALL,
        opcode::POP,
    ];
    let code = [&point_evaluation_call[..], &point_evaluation_call[..], &[opcode::STOP]].concat();
    BlockInput {
        env: inline_env(),
        alloc: BTreeMap::from([
            (get_treasury_address(VECTOR_CHAIN_ID), account(U256::ZERO, &[opcode::STOP])),
            (SIGNER_1, account(ether, &[])),
            (signer_address(2), account(ether, &[])),
            (target, account(U256::ZERO, &code)),
        ]),
        txs: vec![
            anchor_tx(),
            user_tx(1, target, 300_000, 0, &[]),
            user_tx(2, address!("0x000000000000000000000000000000000000beef"), 21_000, 1, &[]),
        ],
    }
}

/// The anchor; tx 1 asks for more gas than the block has, so the executor rejects it before
/// charging its intrinsic zk gas; tx 2 is a transfer.
fn inline_oversized_gas_input() -> BlockInput {
    let mut input = inline_transfer_input();
    let ether = U256::from(1_000_000_000_000_000_000_u64);
    input.alloc.insert(signer_address(2), account(ether, &[]));
    let gas_limit: u64 = input.env.gas_limit.to();
    let beef = address!("0x000000000000000000000000000000000000beef");
    input.txs.insert(1, user_tx(2, beef, gas_limit + 1, 0, &[]));
    input
}

/// The event name of each trace line, with the status of `tx_end` lines.
fn trace_shape(trace: &str) -> Vec<String> {
    trace
        .lines()
        .map(|line| {
            let event: serde_json::Value = serde_json::from_str(line).expect("trace line is JSON");
            match event["e"].as_str().expect("trace line has an event name") {
                "tx_end" => format!("tx_end {}", event["status"].as_str().expect("tx_end status")),
                name => name.to_owned(),
            }
        })
        .collect()
}

/// A change to block expectations that [`compare_block`] must detect.
type Mutation = fn(&mut BlockExpected);

/// Adds one to the block zk gas.
fn bump_block_zk_gas(expected: &mut BlockExpected) {
    expected.block_zk_gas += U64::from(1);
}

/// Drops the last included index.
fn drop_last_included(expected: &mut BlockExpected) {
    expected.included.pop();
}

/// Adds one to the first cumulative zk gas entry.
fn bump_first_cumulative(expected: &mut BlockExpected) {
    expected.cumulative_zk_gas[0] += U64::from(1);
}

/// Flips one bit of the state root.
fn flip_state_root(expected: &mut BlockExpected) {
    expected.state_root.0[0] ^= 1;
}

/// Marks tx 1 as truncated.
fn set_truncated_at_1(expected: &mut BlockExpected) {
    expected.truncated_at = Some(1);
}

/// Creates an empty directory under the system temp dir, unique to this test run.
fn scratch_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let dir = std::env::temp_dir()
        .join(format!("alethia-zk-gas-block-vectors-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn block_compare_rejects_mutated_expectations() {
    let actual = run_block(&inline_transfer_input());
    assert_eq!(actual.included, vec![0, 1]);
    assert_eq!(actual.truncated_at, None);
    assert_eq!(*actual.cumulative_zk_gas.last().unwrap(), actual.block_zk_gas);
    assert!(compare_block(&actual, &actual).is_ok());

    let mutations: [(&str, Mutation); 5] = [
        ("bump_block_zk_gas", bump_block_zk_gas),
        ("drop_last_included", drop_last_included),
        ("bump_first_cumulative", bump_first_cumulative),
        ("flip_state_root", flip_state_root),
        ("set_truncated_at_1", set_truncated_at_1),
    ];
    for (name, mutate) in mutations {
        let mut mutated = actual.clone();
        mutate(&mut mutated);
        assert!(compare_block(&mutated, &actual).is_err(), "{name} must fail the comparison");
    }
}

#[test]
fn block_transfer_charges_only_the_intrinsic_zk_gas() {
    // The anchor calls a STOP stub and the transfer runs no code, so each tx costs exactly the
    // per-tx intrinsic zk gas.
    let actual = run_block(&inline_transfer_input());
    assert_eq!(
        actual.cumulative_zk_gas,
        vec![U64::from(TX_INTRINSIC_ZK_GAS), U64::from(2 * TX_INTRINSIC_ZK_GAS)]
    );
    assert_eq!(actual.import_built, "accept");
    assert_eq!(actual.import_extended, None);
    assert_eq!(actual.import_wrong_difficulty, "reject");
    assert_eq!(actual.raw_txs.len(), 2);
}

#[test]
fn block_transfer_post_state_holds_only_the_tx_effects() {
    // The EIP-2935 and EIP-4788 pre-execution system calls target contracts the alloc lacks and
    // must leave no state behind. The anchor bumps the golden touch nonce and pays no fee; the
    // transfer's base fee is split between the coinbase (`extra_data[0]` = 75%) and the treasury.
    let input = inline_transfer_input();
    let treasury = get_treasury_address(VECTOR_CHAIN_ID);
    let coinbase = input.env.coinbase;
    let fee = U256::from(21_000 * VECTOR_BASE_FEE);
    let coinbase_share = fee * U256::from(75) / U256::from(100);

    let mut post = input.alloc.clone();
    post.get_mut(&treasury).expect("treasury in alloc").balance += fee - coinbase_share;
    post.get_mut(&coinbase).expect("coinbase in alloc").balance += coinbase_share;
    let sender = post.get_mut(&SIGNER_1).expect("signer 1 in alloc");
    sender.balance -= fee + U256::from(1);
    sender.nonce = U64::from(1);
    post.insert(
        address!("0x000000000000000000000000000000000000beef"),
        account(U256::from(1), &[]),
    );
    let mut golden_touch = account(U256::ZERO, &[]);
    golden_touch.nonce = U64::from(1);
    post.insert(Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS), golden_touch);

    assert_eq!(
        run_block(&input).state_root,
        VectorStateProvider::new(&post).root(&HashedPostState::default())
    );
}

#[test]
fn block_truncates_at_first_zk_exhausting_tx() {
    let out = run_block(&inline_truncation_input());
    assert_eq!(out.included, vec![0]);
    assert_eq!(out.truncated_at, Some(1));
    assert_eq!(out.import_extended, Some("reject".into()));
    assert_eq!(out.cumulative_zk_gas, vec![U64::from(TX_INTRINSIC_ZK_GAS)]);
    assert_eq!(out.raw_txs.len(), 3);
}

#[test]
fn sign_txs_uses_the_vector_signers() {
    let input = inline_transfer_input();
    let txs = sign_txs(&input);
    assert_eq!(txs.len(), 2);
    assert_eq!(txs[0].signer(), Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS));
    assert_eq!(txs[1].signer(), SIGNER_1);
    assert_eq!(signer_address(1), SIGNER_1);
    for tx in &txs {
        assert!(tx.is_eip1559(), "every vector tx is EIP-1559");
        assert_eq!(tx.chain_id(), Some(VECTOR_CHAIN_ID));
    }
}

#[test]
fn vector_state_root_matches_the_devnet_genesis_root() {
    // The runner computes every state root itself; check it against a root reth computed
    // independently, over accounts with code, storage and plain balances.
    let alloc: BTreeMap<Address, AllocAccount> = TAIKO_DEVNET
        .inner
        .genesis
        .alloc
        .iter()
        .map(|(address, account)| {
            let storage =
                account.storage.iter().flatten().map(|(slot, value)| {
                    (U256::from_be_bytes(slot.0), U256::from_be_bytes(value.0))
                });
            let alloc_account = AllocAccount {
                balance: account.balance,
                nonce: U64::from(account.nonce.unwrap_or_default()),
                code: account.code.clone().unwrap_or_default(),
                storage: storage.collect(),
            };
            (*address, alloc_account)
        })
        .collect();
    assert!(alloc.values().any(|account| !account.storage.is_empty()), "covers storage");

    assert_eq!(
        VectorStateProvider::new(&alloc).root(&HashedPostState::default()),
        TAIKO_DEVNET.inner.genesis_header().state_root
    );
}

#[test]
fn block_input_parses_signers_and_defaults_missing_account_fields() {
    let input: BlockInput = serde_json::from_str(
        r#"{
            "env": {
                "chain_id": "0x28c59",
                "number": "0x1",
                "timestamp": "0xc",
                "base_fee": "0x17d7840",
                "coinbase": "0x0000000000000000000000000000000000c0ffee",
                "gas_limit": "0x2faf080",
                "prev_randao": "0x2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a",
                "extra_data": "0x4b000000000001"
            },
            "alloc": {
                "0x000000000000000000000000000000000000c0de": { "balance": "0x1" },
                "0x000000000000000000000000000000000000beef": {
                    "balance": "0x0",
                    "nonce": "0x2",
                    "code": "0x00",
                    "storage": { "0x0": "0x2a", "0x10": "0x1" }
                }
            },
            "txs": [
                {
                    "signer": "golden_touch",
                    "nonce": "0x0",
                    "to": "0x1670010000000000000000000000000000010001",
                    "gas": "0xf4240",
                    "max_fee": "0x17d7840",
                    "max_priority_fee": "0x0",
                    "value": "0x0",
                    "data": "0x523e6854"
                },
                {
                    "signer": 7,
                    "nonce": "0x0",
                    "to": null,
                    "gas": "0x5208",
                    "max_fee": "0x17d7840",
                    "max_priority_fee": "0x0",
                    "value": "0x0",
                    "data": "0x"
                }
            ]
        }"#,
    )
    .expect("block input parses");

    assert_eq!(input.env.chain_id, U64::from(VECTOR_CHAIN_ID));
    assert_eq!(input.txs[0].signer, Signer::GoldenTouch);
    assert_eq!(input.txs[0].to, Some(get_treasury_address(VECTOR_CHAIN_ID)));
    assert_eq!(input.txs[1].signer, Signer::Key(7));
    assert_eq!(input.txs[1].to, None, "a null `to` is a contract creation");

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

    for bad in [r#""alice""#, "0", "4294967296", "-1", "1.5"] {
        assert!(serde_json::from_str::<Signer>(bad).is_err(), "signer {bad} must be rejected");
    }
    assert_eq!(serde_json::from_str::<Signer>("4294967295").ok(), Some(Signer::Key(u32::MAX)));
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
fn load_vectors_fails_when_no_vectors_of_layer() {
    let dir = scratch_dir("load");
    let cases = dir.join("cases");
    fs::create_dir(&cases).expect("create cases dir");

    assert!(load_vectors::<BlockVector>(&dir, "block").is_err(), "an empty corpus must fail");

    fs::write(cases.join("only.harness.json"), r#"{ "layer": "harness" }"#).expect("write vector");
    assert!(
        load_vectors::<BlockVector>(&dir, "block").is_err(),
        "a corpus without block vectors must fail"
    );

    let mut vector = serde_json::json!({
        "schema_version": 1,
        "id": "one.block",
        "layer": "block",
        "cells": ["c"],
        "description": "d",
        "input": { "env": inline_env(), "alloc": {}, "txs": [] }
    });
    fs::write(cases.join("one.block.json"), vector.to_string()).expect("write vector");
    let vectors = load_vectors::<BlockVector>(&dir, "block").expect("one block vector");
    assert_eq!(vectors.len(), 1);
    assert_eq!(vectors[0].0, cases.join("one.block.json"));
    assert_eq!(vectors[0].1.id, "one.block");
    assert!(vectors[0].1.expected.is_none());

    // A malformed block vector fails the load instead of being skipped.
    vector["input"]["unknown"] = serde_json::json!(1);
    fs::write(cases.join("one.block.json"), vector.to_string()).expect("write vector");
    assert!(load_vectors::<BlockVector>(&dir, "block").is_err(), "unknown input fields must fail");

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
    fs::write(&path, r#"{"id": "v", "layer": "block"}"#).expect("write vector");
    write_expected(&path, &new_expected).expect("write expected");
    assert_eq!(
        fs::read_to_string(&path).expect("read vector"),
        r#"{
  "id": "v",
  "layer": "block",
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
fn write_expected_keeps_block_field_order_and_hex_zk_gas() {
    let dir = scratch_dir("order");
    let path = dir.join("vector.json");
    fs::write(&path, r#"{"id": "v"}"#).expect("write vector");
    let expected = BlockExpected {
        raw_txs: vec![Bytes::from_static(&[0x02, 0xc0])],
        included: vec![0],
        truncated_at: Some(1),
        cumulative_zk_gas: vec![U64::from(TX_INTRINSIC_ZK_GAS)],
        block_zk_gas: U64::from(TX_INTRINSIC_ZK_GAS),
        state_root: B256::repeat_byte(0x11),
        import_built: "accept".to_owned(),
        import_extended: Some("reject".to_owned()),
        import_wrong_difficulty: "reject".to_owned(),
    };

    write_expected(&path, &expected).expect("write expected");
    assert_eq!(
        fs::read_to_string(&path).expect("read vector"),
        r#"{
  "id": "v",
  "expected": {
    "raw_txs": [
      "0x02c0"
    ],
    "included": [
      0
    ],
    "truncated_at": 1,
    "cumulative_zk_gas": [
      "0x3b538"
    ],
    "block_zk_gas": "0x3b538",
    "state_root": "0x1111111111111111111111111111111111111111111111111111111111111111",
    "import_built": "accept",
    "import_extended": "reject",
    "import_wrong_difficulty": "reject"
  }
}
"#
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn block_vectors_match_expected() {
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

    let vectors = load_vectors::<BlockVector>(&dir, "block").unwrap_or_else(|err| panic!("{err}"));
    let mut failures = Vec::new();
    for (path, vector) in &vectors {
        let actual = match catch_unwind(AssertUnwindSafe(|| run_block(&vector.input))) {
            Ok(actual) => actual,
            Err(payload) => {
                failures.push(format!("{}: {}", vector.id, panic_message(payload.as_ref())));
                continue;
            }
        };
        if write {
            match write_expected(path, &actual) {
                Ok(()) => eprintln!("wrote {}: {}", vector.id, to_json(&actual)),
                Err(err) => failures.push(format!("{}: {err}", vector.id)),
            }
            continue;
        }
        match &vector.expected {
            Some(expected) => {
                if let Err(err) = compare_block(expected, &actual) {
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
        "{} of {} block vectors failed:\n{}",
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
}

#[test]
fn block_trace_of_transfer_charges_only_the_intrinsic() {
    // The anchor runs its STOP stub; the transfer runs no code.
    let (_, trace) = run_block_traced(&inline_transfer_input());
    assert_eq!(
        trace_shape(&trace),
        ["intrinsic", "step", "tx_end committed", "intrinsic", "tx_end committed"]
    );
    let lines: Vec<&str> = trace.lines().collect();
    assert_eq!(lines[0], r#"{"e":"intrinsic","zk":"0x3b538","ok":true}"#);
    assert!(lines[1].starts_with(r#"{"e":"step","d":1,"pc":0,"op":0,"#), "{}", lines[1]);
    assert_eq!(lines[2], r#"{"e":"tx_end","index":0,"status":"committed","tx_zk":"0x3b538"}"#);
    assert_eq!(lines[4], r#"{"e":"tx_end","index":1,"status":"committed","tx_zk":"0x3b538"}"#);
    assert!(trace.ends_with('\n'));
}

#[test]
fn block_trace_records_a_tx_skipped_before_its_intrinsic_charge_alone() {
    let (expected, trace) = run_block_traced(&inline_oversized_gas_input());
    assert_eq!(expected.included, vec![0, 2]);
    assert_eq!(
        trace_shape(&trace),
        [
            "intrinsic",
            "step",
            "tx_end committed",
            "tx_end skipped",
            "intrinsic",
            "tx_end committed"
        ]
    );
    assert!(trace.contains(r#"{"e":"tx_end","index":1,"status":"skipped","tx_zk":null}"#));
}

#[test]
fn block_trace_ends_at_the_aborted_tx() {
    let (expected, trace) = run_block_traced(&inline_truncation_input());
    assert_eq!(expected.truncated_at, Some(1));
    let shape = trace_shape(&trace);
    assert_eq!(&shape[..3], ["intrinsic", "step", "tx_end committed"]);
    // The second point evaluation call exceeds the limit; the sentinel tx never runs.
    assert_eq!(shape.last().map(String::as_str), Some("tx_end aborted"));
    assert_eq!(shape.iter().filter(|kind| *kind == "precompile").count(), 2);
    let lines: Vec<&str> = trace.lines().collect();
    let rejected = lines[lines.len() - 2];
    assert!(
        rejected.starts_with(
            r#"{"e":"precompile","addr":"0x000000000000000000000000000000000000000a","#
        )
    );
    assert!(rejected.ends_with(r#""ok":false}"#), "{rejected}");
    assert_eq!(
        lines[lines.len() - 1],
        r#"{"e":"tx_end","index":1,"status":"aborted","tx_zk":null}"#
    );
}

#[test]
fn block_trace_matches_committed() {
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

    let vectors = load_vectors::<BlockVector>(&dir, "block").unwrap_or_else(|err| panic!("{err}"));
    let mut failures = Vec::new();
    for (_, vector) in &vectors {
        let trace = match catch_unwind(AssertUnwindSafe(|| run_block_traced(&vector.input))) {
            Ok((_, trace)) => trace,
            Err(payload) => {
                failures.push(format!("{}: {}", vector.id, panic_message(payload.as_ref())));
                continue;
            }
        };
        if let Err(err) = sync_trace_file(&dir, &vector.id, &trace, write) {
            failures.push(format!("{}: {err}", vector.id));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} block traces failed:\n{}",
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
}
