# Move Batch Lookup RPCs to taikoAuth Namespace

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Move `lastL1OriginByBatchID` and `lastBlockIDByBatchID` RPC methods from `taiko` namespace to `taikoAuth` namespace to align with taiko-geth commit a6027c7ec.

**Architecture:** Move the two batch lookup methods and their supporting code (helper functions, constants, types) from `TaikoExtApi` in `eth.rs` to `TaikoAuthExtApi` in `auth.rs`. The `TaikoAuthExt` struct already has access to `provider` which is needed for these methods.

**Tech Stack:** Rust, jsonrpsee, reth provider APIs

---

### Task 1: Add Batch Lookup Methods to TaikoAuthExtApi Trait

**Files:**
- Modify: `crates/rpc/src/eth/auth.rs:61-97`

**Step 1: Add the two new method signatures to the trait**

Add these methods to `TaikoAuthExtApi` trait (after line 96, before the closing brace):

```rust
    #[method(name = "lastL1OriginByBatchID")]
    async fn last_l1_origin_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<RpcL1Origin>>;
    #[method(name = "lastBlockIDByBatchID")]
    async fn last_block_id_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<U256>>;
```

**Step 2: Verify compilation**

Run: `cargo check -p alethia-reth-rpc`
Expected: Compilation errors about missing trait implementations (this is expected)

---

### Task 2: Add Supporting Types and Constants to auth.rs

**Files:**
- Modify: `crates/rpc/src/eth/auth.rs`

**Step 1: Add imports needed for batch lookup**

Add these imports at the top of the file (merge with existing imports):

```rust
use alloy_eips::BlockNumberOrTag;
use reth_primitives_traits::{Block as _, BlockBody as _};
use reth_rpc_eth_types::EthApiError;
use alethia_reth_consensus::validation::ANCHOR_V4_SELECTOR;
use alethia_reth_primitives::decode_shasta_proposal_id;
```

**Step 2: Add the LastBlockSearchResult enum and constants after imports**

Add after the imports section (before `PreBuiltTxList` struct):

```rust
/// The result of searching for the last block number by batch ID.
#[derive(Debug, PartialEq, Eq)]
enum LastBlockSearchResult {
    Found(u64),
    NotFound,
    UncertainAtHead,
    LookbackExceeded,
}

/// Maximum number of blocks to scan backwards when resolving a batch ID.
/// Derived from protocol limits: 1024 derivation sources * DERIVATION_SOURCE_MAX_BLOCKS (192).
const MAX_BACKWARD_SCAN_BLOCKS: u64 = 192 * 1024;
#[cfg(test)]
// Keep unit tests fast by using a smaller lookback window.
const TEST_MAX_BACKWARD_SCAN_BLOCKS: u64 = 64;

/// Returns the maximum number of blocks to scan backwards.
fn max_backward_scan_blocks() -> u64 {
    #[cfg(test)]
    {
        TEST_MAX_BACKWARD_SCAN_BLOCKS
    }
    #[cfg(not(test))]
    {
        MAX_BACKWARD_SCAN_BLOCKS
    }
}
```

**Step 3: Verify compilation**

Run: `cargo check -p alethia-reth-rpc`
Expected: Still compilation errors about missing trait implementations

---

### Task 3: Add Helper Methods to TaikoAuthExt

**Files:**
- Modify: `crates/rpc/src/eth/auth.rs`

**Step 1: Add helper methods to TaikoAuthExt impl block**

Add a new impl block for `TaikoAuthExt` with helper methods (after the existing `impl TaikoAuthExt::new` block, before the `#[async_trait]` impl):

```rust
impl<Pool, Eth, Evm, Provider> TaikoAuthExt<Pool, Eth, Evm, Provider>
where
    Provider: DatabaseProviderFactory + BlockReaderIdExt,
{
    /// Checks if the given database error indicates a missing table or key.
    fn is_missing_table_error(error: &reth_db_api::DatabaseError) -> bool {
        match error {
            reth_db_api::DatabaseError::Open(info) | reth_db_api::DatabaseError::Read(info) => {
                info.code == -30798 ||
                    info.message.as_ref().contains("no matching key/data pair found")
            }
            _ => false,
        }
    }

    /// Finds the last Shasta block number that contains an Anchor transaction with the given batch
    /// ID. It scans blocks backwards from the latest block until it finds a matching
    /// transaction, reaches the genesis block, or hits the backward scan limit.
    fn find_last_block_number_by_batch_id(
        &self,
        batch_id: U256,
    ) -> Result<LastBlockSearchResult, EthApiError> {
        let head_number = match self.provider.database_provider_ro() {
            Ok(provider) => {
                let head_lookup =
                    provider.into_tx().get::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY);
                match head_lookup {
                    Ok(Some(number)) => number,
                    Ok(None) => return Ok(LastBlockSearchResult::NotFound),
                    Err(error) => {
                        if Self::is_missing_table_error(&error) {
                            return Ok(LastBlockSearchResult::NotFound);
                        }
                        return Err(EthApiError::InternalEthError);
                    }
                }
            }
            Err(_) => return Err(EthApiError::InternalEthError),
        };

        let mut current_block = self
            .provider
            .block_by_number_or_tag(BlockNumberOrTag::Number(head_number))
            .map_err(|e| EthApiError::Internal(e.into()))?;
        if current_block.is_none() {
            return Ok(LastBlockSearchResult::NotFound);
        }

        let mut scanned_blocks = 0u64;

        while let Some(block) = current_block {
            if scanned_blocks >= max_backward_scan_blocks() {
                return Ok(LastBlockSearchResult::LookbackExceeded);
            }
            scanned_blocks += 1;
            let Some(first_tx) = block.body().transactions().first() else {
                break;
            };

            let input = first_tx.input();
            let input = input.as_ref();

            if !input.starts_with(ANCHOR_V4_SELECTOR) {
                break;
            }

            let Some(proposal_id) =
                decode_shasta_proposal_id(block.header().extra_data().as_ref()).map(U256::from)
            else {
                break;
            };

            if proposal_id == batch_id {
                if head_number == block.header().number() {
                    return Ok(LastBlockSearchResult::UncertainAtHead);
                }
                return Ok(LastBlockSearchResult::Found(block.header().number()));
            }

            let block_number = block.header().number();
            if block_number == 0 {
                break;
            }

            current_block = self
                .provider
                .block_by_number_or_tag(BlockNumberOrTag::Number(block_number - 1))
                .map_err(|e| EthApiError::Internal(e.into()))?;
        }

        Ok(LastBlockSearchResult::NotFound)
    }

    /// Retrieves the last block number for a batch, preferring the DB cache before falling back to
    /// a scan.
    fn resolve_last_block_number_by_batch_id(&self, batch_id: U256) -> RpcResult<U256> {
        if let Ok(provider) = self.provider.database_provider_ro() {
            let batch_lookup = provider.into_tx().get::<BatchToLastBlock>(batch_id.to());
            if let Ok(Some(block_number)) = batch_lookup {
                return Ok(U256::from(block_number));
            }
            if let Err(error) = batch_lookup &&
                !Self::is_missing_table_error(&error)
            {
                return Err(EthApiError::InternalEthError.into());
            }
        }

        match self.find_last_block_number_by_batch_id(batch_id)? {
            LastBlockSearchResult::Found(block_number) => Ok(U256::from(block_number)),
            LastBlockSearchResult::UncertainAtHead => {
                Err(TaikoApiError::ProposalLastBlockUncertain.into())
            }
            LastBlockSearchResult::NotFound => Err(TaikoApiError::GethNotFound.into()),
            LastBlockSearchResult::LookbackExceeded => {
                Err(TaikoApiError::ProposalLastBlockLookbackExceeded.into())
            }
        }
    }
}
```

**Step 2: Add import for BlockHeader trait**

Add to imports:

```rust
use alloy_consensus::{BlockHeader, Transaction as _};
```

Update the existing `use alloy_consensus::BlockHeader;` line to include `Transaction`:

```rust
use alloy_consensus::{BlockHeader, Transaction as _};
```

**Step 3: Verify compilation**

Run: `cargo check -p alethia-reth-rpc`
Expected: Still errors about missing trait method implementations

---

### Task 4: Implement Batch Lookup Methods in TaikoAuthExtApiServer

**Files:**
- Modify: `crates/rpc/src/eth/auth.rs`

**Step 1: Add the two method implementations to the async_trait impl block**

Add these methods to `impl TaikoAuthExtApiServer` (after `tx_pool_content_with_min_tip`, before the closing brace):

```rust
    /// Retrieves the last L1 origin by its batch ID from the database.
    async fn last_l1_origin_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<RpcL1Origin>> {
        let block_id = self.resolve_last_block_number_by_batch_id(batch_id)?;
        let provider =
            self.provider.database_provider_ro().map_err(|_| EthApiError::InternalEthError)?;

        Ok(Some(
            provider
                .into_tx()
                .get::<StoredL1OriginTable>(block_id.to())
                .map_err(|_| EthApiError::InternalEthError)?
                .ok_or(TaikoApiError::GethNotFound)?
                .into_rpc(),
        ))
    }

    /// Retrieves the last block ID for the given batch ID.
    async fn last_block_id_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<U256>> {
        Ok(Some(self.resolve_last_block_number_by_batch_id(batch_id)?))
    }
```

**Step 2: Verify compilation**

Run: `cargo check -p alethia-reth-rpc`
Expected: PASS (no errors)

**Step 3: Commit**

```bash
git add crates/rpc/src/eth/auth.rs
git commit -m "feat(rpc): add batch lookup methods to taikoAuth namespace

Move lastL1OriginByBatchID and lastBlockIDByBatchID to taikoAuth
namespace to align with taiko-geth a6027c7ec."
```

---

### Task 5: Remove Batch Lookup Methods from TaikoExtApi

**Files:**
- Modify: `crates/rpc/src/eth/eth.rs`

**Step 1: Remove method signatures from TaikoExtApi trait**

Remove these lines from the `TaikoExtApi` trait (lines 27-30):

```rust
    #[method(name = "lastL1OriginByBatchID")]
    fn last_l1_origin_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<RpcL1Origin>>;
    #[method(name = "lastBlockIDByBatchID")]
    fn last_block_id_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<U256>>;
```

**Step 2: Remove LastBlockSearchResult enum and related constants**

Remove lines 41-67:

```rust
/// The result of searching for the last block number by batch ID.
#[derive(Debug, PartialEq, Eq)]
enum LastBlockSearchResult {
    Found(u64),
    NotFound,
    UncertainAtHead,
    LookbackExceeded,
}

/// Maximum number of blocks to scan backwards when resolving a batch ID.
/// Derived from protocol limits: 1024 derivation sources * DERIVATION_SOURCE_MAX_BLOCKS (192).
const MAX_BACKWARD_SCAN_BLOCKS: u64 = 192 * 1024;
#[cfg(test)]
// Keep unit tests fast by using a smaller lookback window.
const TEST_MAX_BACKWARD_SCAN_BLOCKS: u64 = 64;

/// Returns the maximum number of blocks to scan backwards.
fn max_backward_scan_blocks() -> u64 {
    #[cfg(test)]
    {
        TEST_MAX_BACKWARD_SCAN_BLOCKS
    }
    #[cfg(not(test))]
    {
        MAX_BACKWARD_SCAN_BLOCKS
    }
}
```

**Step 3: Remove helper methods from TaikoExt impl**

Remove `is_missing_table_error`, `find_last_block_number_by_batch_id`, and `resolve_last_block_number_by_batch_id` methods from the `impl<Provider> TaikoExt<Provider>` block (keeping only `new`).

**Step 4: Remove method implementations from TaikoExtApiServer**

Remove these methods from `impl TaikoExtApiServer`:

```rust
    /// Retrieves the last L1 origin by its batch ID from the database.
    fn last_l1_origin_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<RpcL1Origin>> {
        self.l1_origin_by_id(self.resolve_last_block_number_by_batch_id(batch_id)?)
    }

    /// Retrieves the last block ID for the given batch ID.
    fn last_block_id_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<U256>> {
        Ok(Some(self.resolve_last_block_number_by_batch_id(batch_id)?))
    }
```

**Step 5: Remove unused imports**

Remove these imports that are no longer needed:

```rust
use alethia_reth_consensus::validation::ANCHOR_V4_SELECTOR;
use alethia_reth_primitives::decode_shasta_proposal_id;
```

Also remove `BatchToLastBlock` from the `alethia_reth_db::model` import.

**Step 6: Verify compilation**

Run: `cargo check -p alethia-reth-rpc`
Expected: PASS

---

### Task 6: Move Tests from eth.rs to auth.rs

**Files:**
- Modify: `crates/rpc/src/eth/eth.rs` (remove tests)
- Modify: `crates/rpc/src/eth/auth.rs` (add tests)

**Step 1: Remove batch lookup tests from eth.rs**

Remove these test functions from `mod tests` in eth.rs:
- `returns_not_found_when_head_l1_origin_missing`
- `returns_uncertain_when_match_at_head_without_mapping`
- `uses_expected_lookback_limit_constant`
- `returns_lookback_exceeded_when_scan_exceeds_limit`

Keep only:
- `parses_shasta_proposal_id_from_extra_data`
- `returns_none_for_truncated_extra_data`

Also remove unused test imports:
- `BatchToLastBlock` from the test module imports
- Other imports only used by removed tests

**Step 2: Add test module to auth.rs**

Add at the end of `auth.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alethia_reth_db::model::{
        STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1Origin, StoredL1OriginTable,
    };
    use alloy_consensus::{BlockBody, Header, TxLegacy};
    use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
    use reth_db::{
        ClientVersion, TableSet, Tables,
        mdbx::{DatabaseArguments, init_db_for},
        table::TableInfo,
        test_utils::{
            TempDatabase, create_test_rocksdb_dir, create_test_static_files_dir, tempdir_path,
        },
    };
    use reth_db_api::transaction::{DbTx, DbTxMut};
    use reth_ethereum::{TransactionSigned, chainspec::MAINNET};
    use reth_primitives_traits::{RecoveredBlock, SealedHeader};
    use reth_provider::{
        BlockWriter, ProviderFactory,
        providers::{BlockchainProvider, RocksDBBuilder, StaticFileProvider},
        test_utils::MockNodeTypesWithDB,
    };
    use std::{path::PathBuf, sync::Arc};

    fn create_taiko_test_provider_factory() -> ProviderFactory<MockNodeTypesWithDB> {
        struct TaikoTables;

        impl TableSet for TaikoTables {
            fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>> {
                Box::new(
                    Tables::ALL.iter().map(|table| Box::new(*table) as Box<dyn TableInfo>).chain(
                        alethia_reth_db::model::Tables::ALL
                            .iter()
                            .map(|table| Box::new(*table) as Box<dyn TableInfo>),
                    ),
                )
            }
        }

        let (static_dir, _) = create_test_static_files_dir();
        let (rocksdb_dir, _) = create_test_rocksdb_dir();
        let path = tempdir_path();
        let db = init_db_for::<PathBuf, TaikoTables>(
            path.clone(),
            DatabaseArguments::new(ClientVersion::default()),
        )
        .expect("init db");
        let db = Arc::new(TempDatabase::new(db, path));
        ProviderFactory::new(
            db,
            MAINNET.clone(),
            StaticFileProvider::read_write(static_dir.keep()).expect("static file provider"),
            RocksDBBuilder::new(&rocksdb_dir)
                .with_default_tables()
                .build()
                .expect("failed to create test RocksDB provider"),
        )
        .expect("failed to create test provider factory")
    }

    /// Wrapper to test TaikoAuthExt helper methods without needing Pool/Eth/Evm
    struct TestAuthExt<Provider: DatabaseProviderFactory> {
        provider: Provider,
    }

    impl<Provider> TestAuthExt<Provider>
    where
        Provider: DatabaseProviderFactory + BlockReaderIdExt,
    {
        fn is_missing_table_error(error: &reth_db_api::DatabaseError) -> bool {
            TaikoAuthExt::<(), (), (), Provider>::is_missing_table_error(error)
        }

        fn find_last_block_number_by_batch_id(
            &self,
            batch_id: U256,
        ) -> Result<LastBlockSearchResult, EthApiError> {
            let head_number = match self.provider.database_provider_ro() {
                Ok(provider) => {
                    let head_lookup =
                        provider.into_tx().get::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY);
                    match head_lookup {
                        Ok(Some(number)) => number,
                        Ok(None) => return Ok(LastBlockSearchResult::NotFound),
                        Err(error) => {
                            if Self::is_missing_table_error(&error) {
                                return Ok(LastBlockSearchResult::NotFound);
                            }
                            return Err(EthApiError::InternalEthError);
                        }
                    }
                }
                Err(_) => return Err(EthApiError::InternalEthError),
            };

            let mut current_block = self
                .provider
                .block_by_number_or_tag(BlockNumberOrTag::Number(head_number))
                .map_err(|e| EthApiError::Internal(e.into()))?;
            if current_block.is_none() {
                return Ok(LastBlockSearchResult::NotFound);
            }

            let mut scanned_blocks = 0u64;

            while let Some(block) = current_block {
                if scanned_blocks >= max_backward_scan_blocks() {
                    return Ok(LastBlockSearchResult::LookbackExceeded);
                }
                scanned_blocks += 1;
                let Some(first_tx) = block.body().transactions().first() else {
                    break;
                };

                let input = first_tx.input();
                let input = input.as_ref();

                if !input.starts_with(ANCHOR_V4_SELECTOR) {
                    break;
                }

                let Some(proposal_id) =
                    decode_shasta_proposal_id(block.header().extra_data().as_ref()).map(U256::from)
                else {
                    break;
                };

                if proposal_id == batch_id {
                    if head_number == block.header().number() {
                        return Ok(LastBlockSearchResult::UncertainAtHead);
                    }
                    return Ok(LastBlockSearchResult::Found(block.header().number()));
                }

                let block_number = block.header().number();
                if block_number == 0 {
                    break;
                }

                current_block = self
                    .provider
                    .block_by_number_or_tag(BlockNumberOrTag::Number(block_number - 1))
                    .map_err(|e| EthApiError::Internal(e.into()))?;
            }

            Ok(LastBlockSearchResult::NotFound)
        }

        fn resolve_last_block_number_by_batch_id(&self, batch_id: U256) -> RpcResult<U256> {
            if let Ok(provider) = self.provider.database_provider_ro() {
                let batch_lookup = provider.into_tx().get::<BatchToLastBlock>(batch_id.to());
                if let Ok(Some(block_number)) = batch_lookup {
                    return Ok(U256::from(block_number));
                }
                if let Err(error) = batch_lookup &&
                    !Self::is_missing_table_error(&error)
                {
                    return Err(EthApiError::InternalEthError.into());
                }
            }

            match self.find_last_block_number_by_batch_id(batch_id)? {
                LastBlockSearchResult::Found(block_number) => Ok(U256::from(block_number)),
                LastBlockSearchResult::UncertainAtHead => {
                    Err(TaikoApiError::ProposalLastBlockUncertain.into())
                }
                LastBlockSearchResult::NotFound => Err(TaikoApiError::GethNotFound.into()),
                LastBlockSearchResult::LookbackExceeded => {
                    Err(TaikoApiError::ProposalLastBlockLookbackExceeded.into())
                }
            }
        }
    }

    #[test]
    fn returns_not_found_when_head_l1_origin_missing() {
        let proposal_id = U256::from(0x010203040506u64);
        let extra = vec![0x2a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06];

        let mut input = ANCHOR_V4_SELECTOR.to_vec();
        input.extend_from_slice(&[0u8; 4]);

        let tx = TxLegacy {
            chain_id: None,
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::from(input),
        };

        let signed = TransactionSigned::new_unhashed(
            tx.into(),
            Signature::new(U256::from(1), U256::from(2), false),
        );

        let header = Header {
            number: 1,
            gas_limit: 1_000_000,
            extra_data: extra.into(),
            ..Default::default()
        };
        let body = BlockBody { transactions: vec![signed], ..Default::default() };
        let block = header.clone().into_block(body);
        let recovered = RecoveredBlock::new_unhashed(block, vec![Address::ZERO]);

        let factory = create_taiko_test_provider_factory();
        let provider_rw = factory.provider_rw().expect("provider rw");
        let genesis_header = Header { number: 0, gas_limit: 1_000_000, ..Default::default() };
        let genesis_block = genesis_header.into_block(BlockBody::default());
        let genesis_recovered = RecoveredBlock::new_unhashed(genesis_block, vec![]);
        provider_rw.insert_block(&genesis_recovered).expect("insert genesis block");
        provider_rw.insert_block(&recovered).expect("insert block");
        provider_rw.commit().expect("commit");

        let latest = SealedHeader::seal_slow(header.clone());
        let provider = BlockchainProvider::with_latest(factory, latest).expect("provider");
        let api = TestAuthExt { provider };

        let err = api.resolve_last_block_number_by_batch_id(proposal_id).unwrap_err();
        assert_eq!(err.code(), -32004);
        assert_eq!(err.message(), "not found");
    }

    #[test]
    fn returns_uncertain_when_match_at_head_without_mapping() {
        let proposal_id = U256::from(0x010203040506u64);
        let extra = vec![0x2a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06];

        let mut input = ANCHOR_V4_SELECTOR.to_vec();
        input.extend_from_slice(&[0u8; 4]);

        let tx = TxLegacy {
            chain_id: None,
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::from(input),
        };

        let signed = TransactionSigned::new_unhashed(
            tx.into(),
            Signature::new(U256::from(1), U256::from(2), false),
        );

        let header = Header {
            number: 1,
            gas_limit: 1_000_000,
            extra_data: extra.into(),
            ..Default::default()
        };
        let body = BlockBody { transactions: vec![signed], ..Default::default() };
        let block = header.clone().into_block(body);
        let recovered = RecoveredBlock::new_unhashed(block, vec![Address::ZERO]);

        let factory = create_taiko_test_provider_factory();
        let mut provider_rw = factory.provider_rw().expect("provider rw");
        let genesis_header = Header { number: 0, gas_limit: 1_000_000, ..Default::default() };
        let genesis_block = genesis_header.into_block(BlockBody::default());
        let genesis_recovered = RecoveredBlock::new_unhashed(genesis_block, vec![]);
        provider_rw.insert_block(&genesis_recovered).expect("insert genesis block");
        provider_rw.insert_block(&recovered).expect("insert block");

        {
            let tx = provider_rw.tx_mut();
            let stored_origin = StoredL1Origin {
                block_id: U256::from(header.number),
                l2_block_hash: Default::default(),
                l1_block_height: U256::from(1u64),
                l1_block_hash: Default::default(),
                build_payload_args_id: [0u8; 8],
                is_forced_inclusion: false,
                signature: [0u8; 65],
            };
            tx.put::<StoredL1OriginTable>(header.number, stored_origin).expect("insert l1 origin");
            tx.put::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, header.number)
                .expect("insert head l1 origin");
        }
        provider_rw.commit().expect("commit");

        let provider_ro = factory.provider().expect("provider ro");
        let head_number = provider_ro
            .into_tx()
            .get::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY)
            .expect("read head l1 origin");
        assert_eq!(head_number, Some(header.number));

        let latest = SealedHeader::seal_slow(header.clone());
        let provider = BlockchainProvider::with_latest(factory, latest).expect("provider");
        let api = TestAuthExt { provider };

        let err = api.resolve_last_block_number_by_batch_id(proposal_id).unwrap_err();
        assert_eq!(err.code(), -32005);
        assert_eq!(
            err.message(),
            "proposal last block uncertain: BatchToLastBlockID missing and no newer proposal observed",
        );
    }

    #[test]
    fn uses_expected_lookback_limit_constant() {
        assert_eq!(MAX_BACKWARD_SCAN_BLOCKS, 192 * 1024);
    }

    #[test]
    fn returns_lookback_exceeded_when_scan_exceeds_limit() {
        let max_scan = max_backward_scan_blocks();
        let head_number = max_scan + 1;
        let target_batch_id = U256::from(1u64);

        let mut input = ANCHOR_V4_SELECTOR.to_vec();
        input.extend_from_slice(&[0u8; 4]);
        let input = Bytes::from(input);

        let build_anchor_tx = |input: &Bytes| {
            let tx = TxLegacy {
                chain_id: None,
                nonce: 0,
                gas_price: 1,
                gas_limit: 21_000,
                to: TxKind::Call(Address::ZERO),
                value: U256::ZERO,
                input: input.clone(),
            };

            TransactionSigned::new_unhashed(
                tx.into(),
                Signature::new(U256::from(1), U256::from(2), false),
            )
        };

        let factory = create_taiko_test_provider_factory();
        let mut provider_rw = factory.provider_rw().expect("provider rw");
        let genesis_header = Header { number: 0, gas_limit: 1_000_000, ..Default::default() };
        let genesis_block = genesis_header.into_block(BlockBody::default());
        let genesis_recovered = RecoveredBlock::new_unhashed(genesis_block, vec![]);
        provider_rw.insert_block(&genesis_recovered).expect("insert genesis block");

        let mut head_header = None;

        for number in 1..=head_number {
            let mut extra = [0u8; 7];
            extra[0] = 0x2a;
            extra[1..7].copy_from_slice(&number.to_be_bytes()[2..]);

            let header = Header {
                number,
                gas_limit: 1_000_000,
                extra_data: extra.to_vec().into(),
                ..Default::default()
            };
            let body =
                BlockBody { transactions: vec![build_anchor_tx(&input)], ..Default::default() };
            let block = header.clone().into_block(body);
            let recovered = RecoveredBlock::new_unhashed(block, vec![Address::ZERO]);
            provider_rw.insert_block(&recovered).expect("insert block");

            if number == head_number {
                head_header = Some(header);
            }
        }

        {
            let tx = provider_rw.tx_mut();
            let stored_origin = StoredL1Origin {
                block_id: U256::from(head_number),
                l2_block_hash: Default::default(),
                l1_block_height: U256::from(1u64),
                l1_block_hash: Default::default(),
                build_payload_args_id: [0u8; 8],
                is_forced_inclusion: false,
                signature: [0u8; 65],
            };
            tx.put::<StoredL1OriginTable>(head_number, stored_origin).expect("insert l1 origin");
            tx.put::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, head_number)
                .expect("insert head l1 origin");
        }
        provider_rw.commit().expect("commit");

        let latest = SealedHeader::seal_slow(head_header.expect("head header"));
        let provider = BlockchainProvider::with_latest(factory, latest).expect("provider");
        let api = TestAuthExt { provider };

        let err = api.resolve_last_block_number_by_batch_id(target_batch_id).unwrap_err();
        assert_eq!(err.code(), -32006);
        assert_eq!(
            err.message(),
            "proposal last block lookback exceeded: BatchToLastBlockID missing and lookback limit reached",
        );
    }
}
```

**Step 3: Remove unused test imports from eth.rs**

Update the test module imports in eth.rs to remove unused items.

**Step 4: Verify compilation and tests**

Run: `cargo check -p alethia-reth-rpc`
Expected: PASS

Run: `cargo nextest run -p alethia-reth-rpc`
Expected: All tests pass

**Step 5: Commit**

```bash
git add crates/rpc/src/eth/eth.rs crates/rpc/src/eth/auth.rs
git commit -m "refactor(rpc): remove batch lookup from taiko namespace

Complete migration of lastL1OriginByBatchID and lastBlockIDByBatchID
from taiko to taikoAuth namespace."
```

---

### Task 7: Run Full Test Suite and Clippy

**Step 1: Run clippy**

Run: `just clippy`
Expected: No warnings or errors

**Step 2: Run full test suite**

Run: `just test`
Expected: All tests pass

**Step 3: Final commit if any fixes needed**

If clippy or tests reveal issues, fix them and commit:

```bash
git add -A
git commit -m "fix(rpc): address clippy warnings and test issues"
```
