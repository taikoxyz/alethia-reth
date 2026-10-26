//! Taiko payload-builder attribute normalization and payload-id derivation.
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rlp::{Decodable, Encodable};
use alloy_rpc_types_engine::PayloadId;
#[cfg(feature = "net")]
use alloy_rpc_types_eth::Withdrawal;
use alloy_rpc_types_eth::Withdrawals;
use reth_ethereum_primitives::TransactionSigned;
#[cfg(feature = "net")]
use reth_payload_primitives::PayloadAttributes;
use reth_primitives_traits::{Recovered, SignerRecoverable};
use sha2::{Digest, Sha256};
use std::fmt::Debug;
use tracing::debug;

use crate::{
    etna::{ETNA_MIN_MAX_BLOCK_BYTES, EtnaBuildFieldsBeforeEtna, ensure_no_etna_build_fields},
    extra_data::ETNA_EXTRA_DATA_LEN,
    payload::attributes::TaikoPayloadAttributes,
};

/// Version byte stamped into every Taiko payload identifier, before and after Etna.
pub const PAYLOAD_ID_VERSION_V2: u8 = 2;

/// Payload-ID preimage tag that precedes the big-endian `maxBlockBytes` term.
const PAYLOAD_ID_MAX_BLOCK_BYTES_TAG: u8 = 0x01;

/// Payload-ID preimage tag that precedes the keccak256 of the raw `forcedTransaction` bytes.
const PAYLOAD_ID_FORCED_TRANSACTION_TAG: u8 = 0x02;

/// Taiko Payload Builder Attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TaikoPayloadBuilderAttributes {
    /// Unique identifier for the payload job.
    pub id: PayloadId,
    /// Parent block hash for the payload job.
    pub parent: B256,
    /// Suggested fee recipient from the CL payload attributes.
    pub suggested_fee_recipient: Address,
    /// Previous RANDAO mix from the CL payload attributes.
    pub prev_randao: B256,
    /// Withdrawals committed to the payload job.
    pub withdrawals: Withdrawals,
    /// Optional parent beacon block root for post-Cancun payloads.
    pub parent_beacon_block_root: Option<B256>,
    /// Taiko related attributes.
    /// The hash of the RLP-encoded transactions in the L2 block.
    pub tx_list_hash: B256,
    /// The coinbase for the L2 block.
    pub beneficiary: Address,
    /// The gas limit for the L2 block.
    pub gas_limit: u64,
    /// The timestamp for the L2 block.
    pub timestamp: u64,
    /// The mix hash for the L2 block.
    pub mix_hash: B256,
    /// The basefee for the L2 block.
    pub base_fee_per_gas: u64,
    /// The transactions inside the L2 block.
    ///
    /// - `None`: Transactions are selected from the mempool.
    /// - `Some(vec)`: The decoded provided transaction list is executed in order.
    pub transactions: Option<Vec<Recovered<TransactionSigned>>>,
    /// The extra data for the L2 block.
    pub extra_data: Bytes,
    /// Optional prebuilt anchor transaction required for pre-Etna pool selection.
    pub anchor_transaction: Option<Recovered<TransactionSigned>>,
    /// The decoded, signer-recovered forced transaction of an Etna mixed build.
    ///
    /// `Some` only on Etna jobs without a transaction list (`transactions` is `None`): the forced
    /// transaction is executed first and the rest of the block is selected from the pool. Its
    /// transaction type is not checked here; the build leaves out an unsupported type exactly as
    /// it does in a given list.
    pub forced_transaction: Option<Recovered<TransactionSigned>>,
    /// Upper bound on the built block's executionBlock length, `rlp([header, [tx…]])` including
    /// its outer list header and with transactions in block-body form.
    ///
    /// `Some` (at least [`ETNA_MIN_MAX_BLOCK_BYTES`]) on every Etna job, whatever its build mode,
    /// and `None` before Etna.
    pub max_block_bytes: Option<u64>,
}

#[cfg(feature = "net")]
impl PayloadAttributes for TaikoPayloadBuilderAttributes {
    /// Returns the precomputed payload identifier bound to these payload-job attributes.
    fn payload_id(&self, _parent_hash: &B256) -> PayloadId {
        self.id
    }

    /// Returns the timestamp for the running payload job.
    fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Returns the withdrawals configured for the running payload job.
    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        Some(&self.withdrawals)
    }

    /// Returns the optional parent beacon block root for the running payload job.
    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.parent_beacon_block_root
    }

    /// Taiko payload-builder attributes do not track a beacon slot number.
    fn slot_number(&self) -> Option<u64> {
        None
    }
}

impl TaikoPayloadBuilderAttributes {
    /// Creates a new payload builder for the given parent block and the attributes.
    ///
    /// Normalizes the attributes under the pre-Etna rules and stamps the payload ID with
    /// [`PAYLOAD_ID_VERSION_V2`]; callers that want a different version byte should invoke
    /// [`payload_id_taiko`] directly.
    pub fn try_new(
        parent: B256,
        attributes: TaikoPayloadAttributes,
    ) -> Result<Self, alloy_rlp::Error> {
        Self::try_new_for_fork(parent, attributes, false)
    }

    /// Normalizes payload attributes under the rules active at the target timestamp.
    ///
    /// Every job requires `blockMetadata.timestamp` to equal the attributes' timestamp, because the
    /// fork is chosen from the latter and the block is built at the former. Etna jobs also require
    /// a non-zero beacon root, 13-byte extraData, empty withdrawals, no anchor transaction, and a
    /// `maxBlockBytes` of at least [`ETNA_MIN_MAX_BLOCK_BYTES`]; a `forcedTransaction` must come
    /// without a transaction list and decode as exactly one EIP-2718 transaction whose signer
    /// recovers. Pre-Etna jobs pass `false`, as [`Self::try_new`] does: they reject
    /// `forcedTransaction` and `maxBlockBytes` and, of the other Etna rules, keep only the root
    /// check: a nonzero root is rejected. Every job also rejects a base fee above `u64::MAX` and an
    /// anchor transaction that fails to decode or recover its signer.
    pub fn try_new_for_fork(
        parent: B256,
        attributes: TaikoPayloadAttributes,
        is_etna_active: bool,
    ) -> Result<Self, alloy_rlp::Error> {
        if !block_metadata_timestamp_matches(&attributes) {
            return Err(alloy_rlp::Error::Custom(
                "block metadata timestamp must match payload attributes timestamp",
            ));
        }
        let (forced_transaction, max_block_bytes) = if is_etna_active {
            if attributes
                .payload_attributes
                .parent_beacon_block_root
                .is_none_or(|root| root.is_zero())
            {
                return Err(alloy_rlp::Error::Custom(
                    "Etna payload requires a non-zero parent_beacon_block_root",
                ));
            }
            if attributes.anchor_transaction.is_some() {
                return Err(alloy_rlp::Error::Custom(
                    "Etna payload must not include anchor_transaction",
                ));
            }
            if attributes
                .payload_attributes
                .withdrawals
                .as_ref()
                .is_some_and(|withdrawals| !withdrawals.is_empty())
            {
                return Err(alloy_rlp::Error::Custom("Etna payload withdrawals must be empty"));
            }
            if attributes.block_metadata.extra_data.len() != ETNA_EXTRA_DATA_LEN {
                return Err(alloy_rlp::Error::Custom(
                    "Etna payload extra_data must contain exactly 13 bytes",
                ));
            }
            let max_block_bytes = attributes
                .max_block_bytes
                .ok_or(alloy_rlp::Error::Custom("Etna payload requires maxBlockBytes"))?;
            if max_block_bytes < ETNA_MIN_MAX_BLOCK_BYTES {
                return Err(alloy_rlp::Error::Custom(
                    "Etna payload maxBlockBytes must be at least 1024",
                ));
            }
            if attributes.forced_transaction.is_some() &&
                attributes.block_metadata.tx_list.is_some()
            {
                return Err(alloy_rlp::Error::Custom(
                    "Etna payload must not combine forcedTransaction with blockMetadata.txList",
                ));
            }
            let forced_transaction = attributes
                .forced_transaction
                .as_ref()
                .map(|bytes| decode_forced_transaction(bytes))
                .transpose()?;
            (forced_transaction, Some(max_block_bytes))
        } else {
            ensure_no_etna_build_fields(
                attributes.forced_transaction.is_some(),
                attributes.max_block_bytes.is_some(),
            )
            .map_err(|_| alloy_rlp::Error::Custom(EtnaBuildFieldsBeforeEtna::MESSAGE))?;
            if attributes
                .payload_attributes
                .parent_beacon_block_root
                .is_some_and(|root| !root.is_zero())
            {
                // Pre-Etna payload conversion reconstructs the Unzen zero-root convention. Although
                // `block_to_payload` preserves header roots in its Osaka sidecar, accepting a
                // non-zero root here would build a header that pre-Etna import rejects. Re-check at
                // job creation so callers outside the Engine RPC validation path retain the same
                // invariant.
                return Err(alloy_rlp::Error::Custom(
                    "non-zero parent_beacon_block_root is unsupported on Taiko",
                ));
            }
            (None, None)
        };

        let id = payload_id_taiko(&parent, &attributes, PAYLOAD_ID_VERSION_V2);

        // Determine transaction source based on whether tx_list is provided.
        let transactions = match &attributes.block_metadata.tx_list {
            None => {
                // New mode: transactions will be selected from mempool during payload building
                None
            }
            Some(tx_list_bytes) => {
                // Legacy mode: decode and recover the provided transactions, skipping any that
                // fail signer recovery. If the list itself cannot be decoded, mine an empty block.
                let txs = decode_recovered_transactions(tx_list_bytes).unwrap_or_else(|e| {
                    debug!(
                        target: "payload_builder",
                        "Failed to decode transactions: {e}, bytes: {:?}, mining empty block",
                        tx_list_bytes
                    );
                    Vec::new()
                });
                Some(txs)
            }
        };

        // Compute tx_list_hash based on whether tx_list is provided
        let tx_list_hash =
            attributes.block_metadata.tx_list.as_deref().map(keccak256).unwrap_or_default();

        let anchor_transaction = attributes
            .anchor_transaction
            .as_ref()
            .map(|bytes| {
                TransactionSigned::decode(&mut &bytes[..])
                    .map_err(|_| alloy_rlp::Error::Custom("invalid anchor_transaction"))?
                    .try_into_recovered()
                    .map_err(|_| alloy_rlp::Error::Custom("anchor tx not recoverable"))
            })
            .transpose()?;

        let res = Self {
            id,
            parent,
            suggested_fee_recipient: attributes.payload_attributes.suggested_fee_recipient,
            prev_randao: attributes.payload_attributes.prev_randao,
            withdrawals: attributes.payload_attributes.withdrawals.unwrap_or_default().into(),
            parent_beacon_block_root: attributes.payload_attributes.parent_beacon_block_root,
            tx_list_hash,
            beneficiary: attributes.block_metadata.beneficiary,
            gas_limit: attributes.block_metadata.gas_limit,
            timestamp: attributes.block_metadata.timestamp.to(),
            mix_hash: attributes.payload_attributes.prev_randao,
            base_fee_per_gas: attributes
                .base_fee_per_gas
                .try_into()
                .map_err(|_| alloy_rlp::Error::Custom("invalid attributes.base_fee_per_gas"))?,
            extra_data: attributes.block_metadata.extra_data,
            transactions,
            anchor_transaction,
            forced_transaction,
            max_block_bytes,
        };

        Ok(res)
    }

    /// Returns the id for the running payload job.
    pub const fn payload_id(&self) -> PayloadId {
        self.id
    }

    /// Returns the parent for the running payload job.
    pub const fn parent(&self) -> B256 {
        self.parent
    }

    /// Convenience accessor mirroring [`PayloadAttributes::timestamp`].
    pub const fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Convenience accessor mirroring [`PayloadAttributes::parent_beacon_block_root`].
    pub const fn parent_beacon_block_root(&self) -> Option<B256> {
        self.parent_beacon_block_root
    }

    /// Returns the suggested fee recipient for the running payload job.
    pub const fn suggested_fee_recipient(&self) -> Address {
        self.suggested_fee_recipient
    }

    /// Returns the random beacon value for the running payload job.
    pub const fn prev_randao(&self) -> B256 {
        self.prev_randao
    }

    /// Convenience accessor mirroring [`PayloadAttributes::withdrawals`].
    pub const fn withdrawals(&self) -> &Withdrawals {
        &self.withdrawals
    }
}

/// Generates the payload id for the configured payload from the [`TaikoPayloadAttributes`].
///
/// Returns an 8-byte identifier: the sha256 of the payload components with `payload_version`
/// stamped into the first byte. Pre-Etna and Etna jobs share one preimage: the parent hash,
/// timestamp, `prev_randao`, suggested fee recipient, RLP withdrawals when present, a nonzero
/// parent beacon block root, the transaction-list keccak (zero when absent), and the raw
/// `extraData`. Etna jobs are therefore told apart by their nonzero root and their 13-byte
/// `extraData` (`proposalId` and `anchorBlockNumber`). After `extraData` come the Etna build
/// fields, each only when present: `0x01 ‖ u64_be(maxBlockBytes)`, then
/// `0x02 ‖ keccak256(forcedTransaction)`. Jobs that differ only in these fields get distinct IDs,
/// and attributes without them (every pre-Etna job) keep their earlier IDs.
pub fn payload_id_taiko(
    parent: &B256,
    attributes: &TaikoPayloadAttributes,
    payload_version: u8,
) -> PayloadId {
    let mut hasher = Sha256::new();
    hasher.update(parent.as_slice());
    hasher.update(&attributes.payload_attributes.timestamp.to_be_bytes()[..]);
    hasher.update(attributes.payload_attributes.prev_randao.as_slice());
    hasher.update(attributes.payload_attributes.suggested_fee_recipient.as_slice());
    if let Some(withdrawals) = &attributes.payload_attributes.withdrawals {
        let mut buf = Vec::with_capacity(withdrawals.length());
        withdrawals.encode(&mut buf);
        hasher.update(buf);
    }

    // A zero root builds the same pre-Etna block as an absent one. Hashing neither keeps
    // FCUv3 IDs equal to V2-era IDs and to the drivers' stored fingerprints. Etna build roots
    // are nonzero, so they are always hashed.
    if let Some(root) =
        attributes.payload_attributes.parent_beacon_block_root.filter(|root| !root.is_zero())
    {
        hasher.update(root);
    }

    // Include tx_list hash if provided (legacy mode), otherwise use zero hash (new mode)
    let tx_hash = attributes.block_metadata.tx_list.as_deref().map(keccak256).unwrap_or_default();
    hasher.update(tx_hash);
    hasher.update(attributes.block_metadata.extra_data.as_ref());

    if let Some(max_block_bytes) = attributes.max_block_bytes {
        hasher.update([PAYLOAD_ID_MAX_BLOCK_BYTES_TAG]);
        hasher.update(max_block_bytes.to_be_bytes());
    }
    if let Some(forced_transaction) = &attributes.forced_transaction {
        hasher.update([PAYLOAD_ID_FORCED_TRANSACTION_TAG]);
        hasher.update(keccak256(forced_transaction));
    }

    let mut out = hasher.finalize();
    out[0] = payload_version;
    let mut id_bytes = [0u8; 8];
    id_bytes.copy_from_slice(&out[..8]);
    PayloadId::new(id_bytes)
}

/// Returns whether `blockMetadata.timestamp` equals the attributes' timestamp, as every job
/// requires.
pub fn block_metadata_timestamp_matches(attributes: &TaikoPayloadAttributes) -> bool {
    attributes.block_metadata.timestamp == U256::from(attributes.payload_attributes.timestamp)
}

/// Decodes the raw `forcedTransaction` bytes as exactly one EIP-2718 transaction and recovers its
/// signer.
///
/// The bytes are the canonical EIP-2718 encoding (a typed transaction is its type byte followed by
/// the payload, not wrapped in an RLP string as in the network form); trailing bytes are rejected.
fn decode_forced_transaction(
    bytes: &[u8],
) -> Result<Recovered<TransactionSigned>, alloy_rlp::Error> {
    TransactionSigned::decode_2718_exact(bytes)
        .map_err(|_| {
            alloy_rlp::Error::Custom(
                "Etna payload forcedTransaction must be exactly one EIP-2718 transaction",
            )
        })?
        .try_into_recovered()
        .map_err(|_| {
            alloy_rlp::Error::Custom("Etna payload forcedTransaction signer is not recoverable")
        })
}

/// Decode RLP-encoded bytes into signed transactions.
fn decode_transactions(bytes: &[u8]) -> Result<Vec<TransactionSigned>, alloy_rlp::Error> {
    Vec::<TransactionSigned>::decode(&mut &bytes[..])
}

/// Decode an RLP transaction list and recover each signer, dropping any transaction whose signer
/// cannot be recovered.
///
/// This mirrors Taiko's lenient tx-list ingestion: a malformed top-level RLP list is reported as an
/// error (so callers can decide whether to fall back, e.g. mine an empty block), while individual
/// transactions that fail signer recovery are skipped rather than failing the whole list.
pub fn decode_recovered_transactions(
    bytes: &[u8],
) -> Result<Vec<Recovered<TransactionSigned>>, alloy_rlp::Error> {
    Ok(decode_transactions(bytes)?
        .into_iter()
        .filter_map(|tx| match tx.try_into_recovered() {
            Ok(recovered) => Some(recovered),
            Err(e) => {
                debug!("Failed to recover transaction: {e}, skipping invalid transaction");
                None
            }
        })
        .collect())
}

#[cfg(all(test, feature = "net"))]
mod test {
    use super::*;
    use crate::payload::attributes::{RpcL1Origin, TaikoBlockMetadata, TaikoPayloadAttributes};
    use alloy_consensus::{Header, SignableTransaction, Signed, TxEip1559};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, Bytes, TxKind, U256, hex};
    use alloy_rpc_types_engine::PayloadAttributes as EthPayloadAttributes;
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use reth_chainspec::ChainSpec;
    use reth_engine_local::LocalPayloadAttributesBuilder;
    use reth_payload_primitives::PayloadAttributesBuilder;
    use reth_primitives_traits::SealedHeader;
    use std::sync::Arc;

    fn default_l1_origin() -> RpcL1Origin {
        RpcL1Origin {
            block_id: U256::ZERO,
            l2_block_hash: B256::ZERO,
            l1_block_hash: None,
            l1_block_height: None,
            build_payload_args_id: [0; 8],
            is_forced_inclusion: false,
            signature: [0; 65],
        }
    }

    fn default_eth_payload_attributes(timestamp: u64) -> EthPayloadAttributes {
        EthPayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
            target_gas_limit: None,
        }
    }

    fn create_block_metadata(timestamp: u64, tx_list: Option<Bytes>) -> TaikoBlockMetadata {
        TaikoBlockMetadata {
            beneficiary: Address::ZERO,
            gas_limit: 30_000_000,
            timestamp: U256::from(timestamp),
            mix_hash: B256::ZERO,
            extra_data: Bytes::default(),
            tx_list,
        }
    }

    fn create_payload_attrs(
        timestamp: u64,
        tx_list: Option<Bytes>,
        base_fee: u64,
    ) -> TaikoPayloadAttributes {
        TaikoPayloadAttributes {
            payload_attributes: default_eth_payload_attributes(timestamp),
            base_fee_per_gas: U256::from(base_fee),
            block_metadata: create_block_metadata(timestamp, tx_list),
            l1_origin: default_l1_origin(),
            anchor_transaction: None,
            forced_transaction: None,
            max_block_bytes: None,
        }
    }

    /// Etna attributes that pass every pre-existing Etna check, with the two build fields set.
    fn etna_attrs(
        max_block_bytes: Option<u64>,
        forced_transaction: Option<Bytes>,
    ) -> TaikoPayloadAttributes {
        let mut attributes = create_payload_attrs(1000, None, 1);
        attributes.payload_attributes.parent_beacon_block_root = Some(B256::repeat_byte(0x33));
        attributes.block_metadata.extra_data =
            Bytes::from_static(&[50, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 9]);
        attributes.max_block_bytes = max_block_bytes;
        attributes.forced_transaction = forced_transaction;
        attributes
    }

    /// Returns the raw EIP-2718 bytes of a signed EIP-1559 transaction and its signer.
    fn signed_eip1559(nonce: u64) -> (TransactionSigned, Address) {
        let signer =
            PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).expect("test key is valid");
        let tx = TxEip1559 {
            chain_id: 167_001,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(Address::with_last_byte(0x21)),
            value: U256::from(1),
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).expect("test tx signs");
        (Signed::new_unhashed(tx, signature).into(), signer.address())
    }

    fn raw_eip1559(nonce: u64) -> Bytes {
        signed_eip1559(nonce).0.encoded_2718().into()
    }

    #[test]
    fn try_new_for_fork_requires_etna_max_block_bytes() {
        for (max_block_bytes, ok) in
            [(None, false), (Some(1_023), false), (Some(1_024), true), (Some(124_000), true)]
        {
            let result = TaikoPayloadBuilderAttributes::try_new_for_fork(
                B256::ZERO,
                etna_attrs(max_block_bytes, None),
                true,
            );
            match result {
                Ok(attrs) => {
                    assert!(ok, "{max_block_bytes:?} must be rejected");
                    assert_eq!(attrs.max_block_bytes, max_block_bytes);
                    assert!(attrs.forced_transaction.is_none());
                    assert!(attrs.transactions.is_none(), "no list and no forced tx: pool build");
                }
                Err(err) => {
                    assert!(!ok, "{max_block_bytes:?} must be accepted: {err}");
                    assert!(err.to_string().contains("maxBlockBytes"), "{err}");
                }
            }
        }
        assert_eq!(ETNA_MIN_MAX_BLOCK_BYTES, 1_024);
        let err = TaikoPayloadBuilderAttributes::try_new_for_fork(
            B256::ZERO,
            etna_attrs(Some(ETNA_MIN_MAX_BLOCK_BYTES - 1), None),
            true,
        )
        .expect_err("a bound below the floor must be rejected");
        assert!(err.to_string().contains(&ETNA_MIN_MAX_BLOCK_BYTES.to_string()), "{err}");

        // Given-list builds need the bound too, and carry it unchanged.
        let mut given = etna_attrs(None, None);
        given.block_metadata.tx_list = Some(Bytes::from_static(&[0xc0]));
        TaikoPayloadBuilderAttributes::try_new_for_fork(B256::ZERO, given.clone(), true)
            .expect_err("a given-list Etna build without maxBlockBytes must be rejected");
        given.max_block_bytes = Some(124_000);
        let attrs = TaikoPayloadBuilderAttributes::try_new_for_fork(B256::ZERO, given, true)
            .expect("a given-list Etna build with maxBlockBytes must normalize");
        assert_eq!(attrs.max_block_bytes, Some(124_000));
        assert_eq!(attrs.transactions.as_deref(), Some([].as_slice()));
    }

    #[test]
    fn try_new_for_fork_decodes_the_forced_transaction() {
        let (tx, signer_address) = signed_eip1559(3);
        let raw: Bytes = tx.encoded_2718().into();
        let attrs = TaikoPayloadBuilderAttributes::try_new_for_fork(
            B256::ZERO,
            etna_attrs(Some(124_000), Some(raw.clone())),
            true,
        )
        .expect("a signed EIP-1559 forced transaction must normalize");

        // A mixed build selects the rest from the pool.
        assert!(attrs.transactions.is_none());
        assert_eq!(attrs.tx_list_hash, B256::ZERO);
        assert_eq!(attrs.max_block_bytes, Some(124_000));
        let forced = attrs.forced_transaction.expect("forced transaction is decoded");
        assert_eq!(*forced.tx_hash(), keccak256(&raw));
        assert_eq!(forced.signer(), signer_address);
        assert_eq!(forced.into_inner(), tx);
    }

    #[test]
    fn try_new_for_fork_rejects_malformed_forced_transactions() {
        let raw = raw_eip1559(0);
        // The network (pooled) form wraps the typed bytes in an RLP string header.
        let network_form = Bytes::from(alloy_rlp::encode(raw.as_ref()));
        assert_ne!(network_form, raw);
        let mut trailing = raw.to_vec();
        trailing.push(0x00);
        // A typed transaction whose signature cannot be recovered: r = s = 0.
        let unrecoverable: TransactionSigned = Signed::new_unhashed(
            TxEip1559 { chain_id: 167_001, gas_limit: 21_000, ..Default::default() },
            alloy_primitives::Signature::new(U256::ZERO, U256::ZERO, false),
        )
        .into();
        let unrecoverable: Bytes = unrecoverable.encoded_2718().into();
        let decoded = TransactionSigned::decode_2718_exact(&unrecoverable)
            .expect("the zero-signature transaction decodes, so only recovery rejects it");
        assert!(decoded.recover_signer().is_err());

        let cases: [(&str, Bytes, Option<Bytes>); 6] = [
            ("empty", Bytes::new(), None),
            ("type byte only", Bytes::from_static(&[0x02]), None),
            ("network form", network_form, None),
            ("trailing byte", Bytes::from(trailing), None),
            ("unrecoverable signer", unrecoverable, None),
            ("with txList", raw.clone(), Some(Bytes::from_static(&[0xc0]))),
        ];
        for (name, forced, tx_list) in cases {
            let mut attributes = etna_attrs(Some(124_000), Some(forced));
            attributes.block_metadata.tx_list = tx_list;
            let err = TaikoPayloadBuilderAttributes::try_new_for_fork(B256::ZERO, attributes, true)
                .expect_err(name);
            assert!(err.to_string().contains("forcedTransaction"), "{name}: {err}");
        }

        // An empty explicit list is still a list, so it excludes the forced transaction too.
        let mut attributes = etna_attrs(Some(124_000), Some(raw));
        attributes.block_metadata.tx_list = Some(Bytes::new());
        TaikoPayloadBuilderAttributes::try_new_for_fork(B256::ZERO, attributes, true)
            .expect_err("an empty txList and a forced transaction are exclusive");
    }

    #[test]
    fn pre_etna_attributes_reject_etna_build_fields() {
        let raw = raw_eip1559(0);
        for (max_block_bytes, forced) in
            [(Some(124_000), None), (None, Some(raw.clone())), (Some(124_000), Some(raw.clone()))]
        {
            let mut attributes = create_payload_attrs(1000, None, 1);
            attributes.max_block_bytes = max_block_bytes;
            attributes.forced_transaction = forced;
            type Normalize = fn(
                TaikoPayloadAttributes,
            )
                -> Result<TaikoPayloadBuilderAttributes, alloy_rlp::Error>;
            let normalizers: [Normalize; 2] = [
                |a| TaikoPayloadBuilderAttributes::try_new(B256::ZERO, a),
                |a| TaikoPayloadBuilderAttributes::try_new_for_fork(B256::ZERO, a, false),
            ];
            for normalize in normalizers {
                let err = normalize(attributes.clone())
                    .expect_err("Etna build fields must be rejected before Etna");
                assert_eq!(err.to_string(), "forcedTransaction and maxBlockBytes require Etna");
                assert_eq!(err.to_string(), EtnaBuildFieldsBeforeEtna.to_string());
            }
        }

        let attrs =
            TaikoPayloadBuilderAttributes::try_new(B256::ZERO, create_payload_attrs(1000, None, 1))
                .expect("pre-Etna attributes without the fields still normalize");
        assert_eq!(attrs.max_block_bytes, None);
        assert!(attrs.forced_transaction.is_none());
    }

    #[test]
    fn payload_id_binds_max_block_bytes_and_forced_transaction() {
        let parent = B256::repeat_byte(0xaa);
        let raw_a = raw_eip1559(0);
        let raw_b = raw_eip1559(1);
        let id = |max_block_bytes, forced| {
            payload_id_taiko(&parent, &etna_attrs(max_block_bytes, forced), PAYLOAD_ID_VERSION_V2)
        };

        let base = id(None, None);
        let max = id(Some(124_000), None);
        assert_ne!(base, max);
        assert_ne!(max, id(Some(123_999), None));
        assert_ne!(max, id(Some(124_000), Some(raw_a.clone())));
        assert_ne!(id(Some(124_000), Some(raw_a.clone())), id(Some(124_000), Some(raw_b)));
        // A forced transaction alone is bound too, independently of the bound.
        assert_ne!(base, id(None, Some(raw_a)));

        // Pinned against an independent sha256/keccak256 computation of the preimage: the
        // pre-existing terms, then `0x01 ‖ u64_be(maxBlockBytes)`, then
        // `0x02 ‖ keccak256(forcedTransaction)`. Without the fields the ID is unchanged.
        assert_eq!(base, PayloadId::new([0x02, 0x34, 0xfc, 0x21, 0x78, 0x65, 0x63, 0x9c]));
        assert_eq!(max, PayloadId::new([0x02, 0xa4, 0x74, 0xc7, 0x62, 0x62, 0x6a, 0x4a]));
        assert_eq!(
            id(Some(124_000), Some(Bytes::from_static(&[0x02, 0xc0]))),
            PayloadId::new([0x02, 0x55, 0xee, 0x4c, 0x2e, 0x0d, 0xd7, 0x06])
        );
    }

    #[test]
    fn test_decode_transactions() {
        let empty_decoded = decode_transactions(&Bytes::from_static(&hex!("0xc0")));
        assert_eq!(empty_decoded.unwrap().len(), 0);

        let with_anchor_decoded = decode_transactions(&Bytes::from_static(&hex!(
            "0xf90220b901b302f901af83028c59808083989680830f424094167001000000000000000000000000000001000180b9014448080a450000000000000000000000000000000000000000000000000000000000000028d2c559ea42da728e0d0154b95699eeac543c768755611756ab0d1ce2b0abe95600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000003200000000000000000000000000000000000000000000000000000000004c4b4000000000000000000000000000000000000000000000000000000000502989660000000000000000000000000000000000000000000000000000000023c3460000000000000000000000000000000000000000000000000000000000000001200000000000000000000000000000000000000000000000000000000000000000c080a079be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798a060ad1bd4369cd9156712860a4aaf49c474fa9290bbbc600069f666de1fd28cbdf868808502540be400830186a0943edb876b8928dd168f3785576a79afa7d07dc7978080830518d5a072ae800154047cf587c08937484082b436a4a0d236bfdf731603dfe5c7580a64a054161df1ea94ec7933b643fd6fbfefbb453350a47dfe4a4ec3cd840c0c5f915c"
        )));
        assert!(!with_anchor_decoded.unwrap().is_empty());
    }

    #[test]
    fn test_taiko_payload_builder_attributes_legacy_mode() {
        let tx_list_bytes = Bytes::from_static(&hex!("c0"));
        let payload_attrs = create_payload_attrs(1000, Some(tx_list_bytes.clone()), 100_000_000);

        let attrs = TaikoPayloadBuilderAttributes::try_new(B256::ZERO, payload_attrs)
            .expect("Should create builder attributes in legacy mode");

        assert!(attrs.transactions.is_some(), "Legacy mode should have transactions");
        assert_eq!(
            attrs.transactions.unwrap().len(),
            0,
            "Empty tx_list should decode to empty vec"
        );
        assert_eq!(attrs.tx_list_hash, keccak256(tx_list_bytes));
    }

    #[test]
    fn malformed_tx_list_yields_an_empty_transaction_list() {
        let payload_attrs = create_payload_attrs(1000, Some(Bytes::from_static(&[1])), 100_000_000);

        let attrs = TaikoPayloadBuilderAttributes::try_new(B256::ZERO, payload_attrs)
            .expect("an undecodable tx list still creates builder attributes");

        assert_eq!(attrs.transactions.as_deref(), Some([].as_slice()));
        assert_eq!(attrs.tx_list_hash, keccak256([1u8]));
    }

    #[test]
    fn test_taiko_payload_builder_attributes_new_mode() {
        let payload_attrs = create_payload_attrs(1000, None, 100_000_000);

        let attrs = TaikoPayloadBuilderAttributes::try_new(B256::ZERO, payload_attrs)
            .expect("Should create builder attributes in new mode");

        assert!(attrs.transactions.is_none(), "New mode should use mempool selection");
        assert_eq!(attrs.tx_list_hash, B256::ZERO, "tx_list_hash should be zero without tx_list");
    }

    #[test]
    fn v2_payload_id_bytes_remain_stable() {
        // V2-era drivers sent no root; FCUv3 sends a zero root. Both must keep the V2-era ID.
        let mut attributes = create_payload_attrs(1000, None, 100_000_000);
        attributes.payload_attributes.parent_beacon_block_root = None;
        let expected = PayloadId::new([0x02, 0x84, 0xe3, 0x12, 0x99, 0xa2, 0xad, 0x9e]);
        assert_eq!(payload_id_taiko(&B256::ZERO, &attributes, PAYLOAD_ID_VERSION_V2), expected);
        attributes.payload_attributes.parent_beacon_block_root = Some(B256::ZERO);
        assert_eq!(payload_id_taiko(&B256::ZERO, &attributes, PAYLOAD_ID_VERSION_V2), expected);
    }

    #[test]
    fn payload_id_pins_every_preimage_field() {
        // Every hashed field is nonzero, so dropping, reordering, or re-encoding any term changes
        // the pinned ID. taiko-client-rs uses this ID as its `buildPayloadArgsId` fingerprint.
        let parent = B256::repeat_byte(0xaa);
        let mut full = create_payload_attrs(1000, Some(Bytes::from_static(&[0xc1, 0x80])), 1);
        full.payload_attributes.prev_randao = B256::repeat_byte(0x11);
        full.payload_attributes.suggested_fee_recipient = Address::repeat_byte(0x22);
        full.payload_attributes.withdrawals = Some(vec![Withdrawal {
            index: 1,
            validator_index: 2,
            address: Address::repeat_byte(0x44),
            amount: 3,
        }]);
        full.payload_attributes.parent_beacon_block_root = Some(B256::repeat_byte(0x33));
        full.block_metadata.extra_data =
            Bytes::from_static(&[50, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 9]);
        let id = |attributes: &TaikoPayloadAttributes| {
            reth_payload_primitives::PayloadAttributes::payload_id(attributes, &parent)
        };
        let base = id(&full);
        assert_eq!(base, PayloadId::new([0x02, 0x34, 0xc1, 0x7f, 0x9e, 0xfa, 0x03, 0x8e]));
        assert_eq!(base, payload_id_taiko(&parent, &full, PAYLOAD_ID_VERSION_V2));

        let mutations: &[fn(&mut TaikoPayloadAttributes)] = &[
            // A zero root is the pre-Etna shape and must not alias an Etna job.
            |a| a.payload_attributes.parent_beacon_block_root = Some(B256::ZERO),
            // proposalId is bytes 1..=6 and anchorBlockNumber bytes 7..=12.
            |a| {
                a.block_metadata.extra_data =
                    Bytes::from_static(&[50, 0, 0, 0, 0, 0, 8, 0, 0, 0, 0, 0, 9])
            },
            |a| {
                a.block_metadata.extra_data =
                    Bytes::from_static(&[50, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 10])
            },
            // An absent list selects from the pool, an empty one is derived input.
            |a| a.block_metadata.tx_list = None,
            |a| a.block_metadata.tx_list = Some(Bytes::new()),
        ];
        let mut ids = std::collections::HashSet::from([base]);
        for mutate in mutations {
            let mut changed = full.clone();
            mutate(&mut changed);
            assert!(ids.insert(id(&changed)), "each change must give a distinct payload id");
        }
    }

    #[test]
    fn try_new_rejects_a_block_metadata_timestamp_mismatch() {
        // Before Etna too, the job must build at the timestamp its fork was chosen from.
        let mut payload_attrs = create_payload_attrs(1000, None, 100_000_000);
        payload_attrs.block_metadata.timestamp = U256::MAX;

        TaikoPayloadBuilderAttributes::try_new(B256::ZERO, payload_attrs)
            .expect_err("a mismatched or over-wide metadata timestamp must fail closed");
    }

    #[test]
    fn try_new_rejects_non_zero_parent_beacon_block_root() {
        let mut payload_attrs = create_payload_attrs(1000, None, 100_000_000);
        payload_attrs.payload_attributes.parent_beacon_block_root = Some(B256::repeat_byte(0x33));

        let err = TaikoPayloadBuilderAttributes::try_new(B256::ZERO, payload_attrs)
            .expect_err("a non-zero beacon root cannot round-trip and must fail closed");
        assert!(err.to_string().contains("parent_beacon_block_root"));

        // The zero root is the network invariant and keeps round-tripping.
        let mut payload_attrs = create_payload_attrs(1000, None, 100_000_000);
        payload_attrs.payload_attributes.parent_beacon_block_root = Some(B256::ZERO);
        let attrs = TaikoPayloadBuilderAttributes::try_new(B256::ZERO, payload_attrs)
            .expect("a zero beacon root must remain accepted");
        assert_eq!(attrs.parent_beacon_block_root(), Some(B256::ZERO));
    }

    #[test]
    fn payload_id_changes_with_extra_data() {
        let builder = LocalPayloadAttributesBuilder::new(Arc::new(ChainSpec::<Header>::default()));
        let parent_hash = B256::from([1u8; 32]);
        // Create a parent header to pass to the builder
        let parent_header = Header { timestamp: 1_700_000_000, ..Default::default() };
        let parent = SealedHeader::seal_slow(parent_header);
        let mut base_attributes: TaikoPayloadAttributes = builder.build(&parent);
        base_attributes.block_metadata.extra_data = Bytes::from_static(b"extra-a");

        let mut other_attributes = base_attributes.clone();
        other_attributes.block_metadata.extra_data = Bytes::from_static(b"extra-b");

        let first = payload_id_taiko(&parent_hash, &base_attributes, 1);
        let second = payload_id_taiko(&parent_hash, &other_attributes, 1);

        assert_ne!(first, second);
    }
}
