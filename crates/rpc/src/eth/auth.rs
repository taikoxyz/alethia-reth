#![allow(clippy::too_many_arguments)]
use std::{convert::Infallible, sync::Arc};

use alloy_consensus::{BlockHeader as _, Transaction as _};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_json_rpc::RpcObject;
use alloy_primitives::{Bytes, U256};
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth::{
    revm::primitives::Address,
    transaction_pool::{PoolConsensusTx, PoolTransaction, TransactionPool},
};
use reth_db_api::{DatabaseError, transaction::{DbTx, DbTxMut}};
use reth_ethereum::{EthPrimitives, TransactionSigned};
use reth_evm::ConfigureEngineEvm;
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_api::{Block, NodePrimitives};
use reth_primitives_traits::BlockBody as _;
use reth_provider::{BlockReaderIdExt, DBProvider, DatabaseProviderFactory, StateProviderFactory};
use reth_revm::{State, database::StateProviderDatabase};
use reth_rpc_eth_api::{RpcConvert, RpcTransaction};
use reth_rpc_eth_types::EthApiError;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::eth::error::TaikoApiError;
use alethia_reth_consensus::validation::ANCHOR_V4_SELECTOR;
use alethia_reth_block::{
    assembler::TaikoBlockAssembler,
    config::TaikoNextBlockEnvAttributes,
    factory::TaikoBlockExecutorFactory,
    tx_selection::{
        DEFAULT_DA_ZLIB_GUARD_BYTES, SelectionOutcome, TxSelectionConfig,
        select_and_execute_pool_transactions,
    },
};
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_db::model::{
    BatchToLastBlock, STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1OriginTable,
};
use alethia_reth_evm::factory::TaikoEvmFactory;
use alethia_reth_primitives::{
    decode_shasta_proposal_id, engine::types::TaikoExecutionData, payload::attributes::RpcL1Origin,
};

/// A pre-built transaction list that contains the mempool content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct PreBuiltTxList<T> {
    pub tx_list: Vec<T>,
    pub estimated_gas_used: u64,
    pub bytes_length: u64,
}

impl<T> Default for PreBuiltTxList<T> {
    fn default() -> Self {
        Self { tx_list: vec![], estimated_gas_used: 0, bytes_length: 0 }
    }
}

/// trait interface for a custom auth rpc namespace: `taikoAuth`
///
/// This defines the Taiko namespace where all methods are configured as trait functions.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "taikoAuth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "taikoAuth"))]
pub trait TaikoAuthExtApi<T: RpcObject> {
    #[method(name = "setHeadL1Origin")]
    async fn set_head_l1_origin(&self, id: U256) -> RpcResult<U256>;
    #[method(name = "updateL1Origin")]
    async fn update_l1_origin(&self, l1_origin: RpcL1Origin) -> RpcResult<Option<RpcL1Origin>>;
    #[method(name = "setL1OriginSignature")]
    async fn set_l1_origin_signature(&self, id: U256, signature: Bytes) -> RpcResult<RpcL1Origin>;
    #[method(name = "setBatchToLastBlock")]
    async fn set_batch_to_last_block(&self, batch_id: U256, block_number: U256) -> RpcResult<u64>;
    /// Returns the last L1 origin for a given batch ID.
    #[method(name = "lastL1OriginByBatchID")]
    async fn last_l1_origin_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<RpcL1Origin>>;
    /// Returns the last block ID for a given batch ID.
    #[method(name = "lastBlockIDByBatchID")]
    async fn last_block_id_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<U256>>;
    #[method(name = "txPoolContentWithMinTip")]
    async fn tx_pool_content_with_min_tip(
        &self,
        beneficiary: Address,
        base_fee: u64,
        block_max_gas_limit: u64,
        max_bytes_per_tx_list: u64,
        locals: Option<Vec<Address>>,
        max_transactions_lists: u64,
        min_tip: u64,
    ) -> RpcResult<Vec<PreBuiltTxList<T>>>;

    #[method(name = "txPoolContent")]
    async fn tx_pool_content(
        &self,
        beneficiary: Address,
        base_fee: u64,
        block_max_gas_limit: u64,
        max_bytes_per_tx_list: u64,
        locals: Option<Vec<Address>>,
        max_transactions_lists: u64,
    ) -> RpcResult<Vec<PreBuiltTxList<T>>>;
}

/// A concrete implementation of the `TaikoAuthExtApi` trait.
#[derive(Clone)]
pub struct TaikoAuthExt<Pool, Eth, Evm, Provider: DatabaseProviderFactory> {
    provider: Provider,
    pool: Pool,
    tx_resp_builder: Eth,
    evm_config: Evm,
}

impl<Pool, Eth, Evm, Provider: DatabaseProviderFactory> TaikoAuthExt<Pool, Eth, Evm, Provider> {
    /// Creates a new instance of `TaikoAuthExt` with the given provider.
    pub fn new(provider: Provider, pool: Pool, tx_resp_builder: Eth, evm_config: Evm) -> Self {
        Self { provider, pool, tx_resp_builder, evm_config }
    }
}

#[derive(Debug, PartialEq, Eq)]
/// Outcome of searching for the last block number by batch ID.
enum LastBlockSearchResult {
    /// Found a matching block number.
    Found(u64),
    /// No match observed within the scan window.
    NotFound,
    /// Match found at the head without a newer proposal to confirm.
    UncertainAtHead,
    /// Scan hit the configured backward limit.
    LookbackExceeded,
}

/// Maximum number of blocks to scan backwards when resolving a batch ID.
const MAX_BACKWARD_SCAN_BLOCKS: u64 = 192 * 1024;
#[cfg(test)]
/// Shorter backward scan limit for test execution.
const TEST_MAX_BACKWARD_SCAN_BLOCKS: u64 = 64;

/// Returns the scan limit, using the test override when enabled.
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

impl<Pool, Eth, Evm, Provider> TaikoAuthExt<Pool, Eth, Evm, Provider>
where
    Provider: DatabaseProviderFactory + BlockReaderIdExt,
{
    /// Checks if a database error indicates a missing table or key.
    fn is_missing_table_error(error: &DatabaseError) -> bool {
        match error {
            DatabaseError::Open(info) | DatabaseError::Read(info) => {
                info.code == -30798 ||
                    info.message.as_ref().contains("no matching key/data pair found")
            }
            _ => false,
        }
    }

    /// Scans backwards to find the last block number for the provided batch ID.
    fn find_last_block_number_by_batch_id(
        &self,
        batch_id: U256,
    ) -> Result<LastBlockSearchResult, EthApiError> {
        // Fetch the latest block as the head for scanning.
        let latest_block = self
            .provider
            .block_by_number_or_tag(BlockNumberOrTag::Latest)
            .map_err(|e| EthApiError::Internal(e.into()))?;
        let Some(latest_block) = latest_block else {
            return Ok(LastBlockSearchResult::NotFound);
        };
        // Capture the latest block number to detect head-only matches.
        let head_number = latest_block.header().number();
        // Read-only database provider for L1 origin lookups during scanning.
        let db_provider =
            self.provider.database_provider_ro().map_err(|_| EthApiError::InternalEthError)?;
        // Read-only transaction used for repeated L1 origin reads.
        let db_tx = db_provider.into_tx();

        // Start scanning from the current head block.
        let mut current_block = Some(latest_block);

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

            // Current block number used for DB lookups and traversal.
            let block_number = block.header().number();
            // L1 origin lookup to skip preconfirmation blocks.
            let l1_origin_lookup = db_tx.get::<StoredL1OriginTable>(block_number);
            match l1_origin_lookup {
                Ok(Some(l1_origin)) => {
                    if l1_origin.l1_block_height == U256::ZERO {
                        if block_number == 0 {
                            break;
                        }
                        current_block = self
                            .provider
                            .block_by_number_or_tag(BlockNumberOrTag::Number(block_number - 1))
                            .map_err(|e| EthApiError::Internal(e.into()))?;
                        continue;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if !Self::is_missing_table_error(&error) {
                        return Err(EthApiError::InternalEthError);
                    }
                }
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

    /// Resolves the last block number, preferring the DB cache before scanning.
    fn resolve_last_block_number_by_batch_id(&self, batch_id: U256) -> RpcResult<U256> {
        if let Ok(provider) = self.provider.database_provider_ro() {
            let batch_lookup = provider.into_tx().get::<BatchToLastBlock>(batch_id.to());
            if let Ok(Some(block_number)) = batch_lookup {
                return Ok(U256::from(block_number));
            }
            if let Err(error) = batch_lookup && !Self::is_missing_table_error(&error) {
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

#[async_trait]
impl<Pool, Eth, Evm, Provider> TaikoAuthExtApiServer<RpcTransaction<Eth::Network>>
    for TaikoAuthExt<Pool, Eth, Evm, Provider>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>> + 'static,
    Eth: RpcConvert<Primitives: NodePrimitives<SignedTx = PoolConsensusTx<Pool>>> + 'static,
    Provider: DatabaseProviderFactory
        + BlockReaderIdExt<Header = alloy_consensus::Header>
        + StateProviderFactory
        + 'static,
    Evm: ConfigureEngineEvm<
            TaikoExecutionData,
            Primitives = EthPrimitives,
            Error = Infallible,
            NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
            BlockExecutorFactory = TaikoBlockExecutorFactory<
                RethReceiptBuilder,
                Arc<TaikoChainSpec>,
                TaikoEvmFactory,
            >,
            BlockAssembler = TaikoBlockAssembler,
        > + 'static,
{
    /// Sets the L1 head origin in the database.
    async fn set_head_l1_origin(&self, id: U256) -> RpcResult<U256> {
        let tx = self
            .provider
            .database_provider_rw()
            .map_err(|_| EthApiError::InternalEthError)?
            .into_tx();

        tx.put::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, id.to::<u64>())
            .map_err(|_| EthApiError::InternalEthError)?;

        tx.commit().map_err(|_| EthApiError::InternalEthError)?;

        Ok(id)
    }

    /// Sets the L1 origin signature in the database.
    async fn set_l1_origin_signature(&self, id: U256, signature: Bytes) -> RpcResult<RpcL1Origin> {
        let tx = self
            .provider
            .database_provider_rw()
            .map_err(|_| EthApiError::InternalEthError)?
            .into_tx();

        let mut l1_origin = tx
            .get::<StoredL1OriginTable>(id.to())
            .map_err(|_| EthApiError::InternalEthError)?
            .ok_or(TaikoApiError::GethNotFound)?;

        l1_origin.signature = <[u8; 65]>::try_from(signature.as_ref()).map_err(|_| {
            EthApiError::InvalidParams("Signature must be a 65-byte array".to_string())
        })?;

        tx.put::<StoredL1OriginTable>(l1_origin.block_id.to(), l1_origin.clone())
            .map_err(|_| EthApiError::InternalEthError)?;

        tx.commit().map_err(|_| EthApiError::InternalEthError)?;

        Ok(l1_origin.into_rpc())
    }

    /// Sets the mapping from batch ID to its last block number in the database.
    async fn set_batch_to_last_block(&self, batch_id: U256, block_number: U256) -> RpcResult<u64> {
        let tx = self
            .provider
            .database_provider_rw()
            .map_err(|_| EthApiError::InternalEthError)?
            .into_tx();
        tx.put::<BatchToLastBlock>(batch_id.to(), block_number.to())
            .map_err(|_| EthApiError::InternalEthError)?;
        tx.commit().map_err(|_| EthApiError::InternalEthError)?;
        Ok(batch_id.to())
    }

    /// Updates the L1 origin in the database.
    async fn update_l1_origin(&self, l1_origin: RpcL1Origin) -> RpcResult<Option<RpcL1Origin>> {
        let tx = self
            .provider
            .database_provider_rw()
            .map_err(|_| EthApiError::InternalEthError)?
            .into_tx();

        tx.put::<StoredL1OriginTable>(l1_origin.block_id.to(), l1_origin.clone().into())
            .map_err(|_| EthApiError::InternalEthError)?;

        tx.commit().map_err(|_| EthApiError::InternalEthError)?;

        Ok(Some(l1_origin))
    }

    /// Retrieves the last L1 origin for the given batch ID.
    async fn last_l1_origin_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<RpcL1Origin>> {
        let provider =
            self.provider.database_provider_ro().map_err(|_| EthApiError::InternalEthError)?;

        let block_id = self.resolve_last_block_number_by_batch_id(batch_id)?;

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

    /// Retrieves the transaction pool content with the given limits.
    async fn tx_pool_content(
        &self,
        beneficiary: Address,
        base_fee: u64,
        block_max_gas_limit: u64,
        max_bytes_per_tx_list: u64,
        locals: Option<Vec<Address>>,
        max_transactions_lists: u64,
    ) -> RpcResult<Vec<PreBuiltTxList<RpcTransaction<Eth::Network>>>> {
        self.tx_pool_content_with_min_tip(
            beneficiary,
            base_fee,
            block_max_gas_limit,
            max_bytes_per_tx_list,
            locals,
            max_transactions_lists,
            0,
        )
        .await
    }

    /// Retrieves the transaction pool content with the given limits and minimum tip.
    async fn tx_pool_content_with_min_tip(
        &self,
        beneficiary: Address,
        base_fee: u64,
        block_max_gas_limit: u64,
        max_bytes_per_tx_list: u64,
        locals: Option<Vec<Address>>,
        max_transactions_lists: u64,
        min_tip: u64,
    ) -> RpcResult<Vec<PreBuiltTxList<RpcTransaction<Eth::Network>>>> {
        if max_transactions_lists == 0 {
            return Err(EthApiError::InvalidParams(
                "`maxTransactionsLists` must not be `0`".to_string(),
            )
            .into());
        }

        // Fetch the parent block and its state, for building the prebuilt transaction lists later.
        let parent_block = self
            .provider
            .block_by_number_or_tag(BlockNumberOrTag::Latest)
            .map_err(|e| EthApiError::Internal(e.into()))?
            .ok_or(EthApiError::HeaderNotFound(BlockId::Number(BlockNumberOrTag::Latest)))?;
        let sealed_parent = parent_block.seal();
        let parent = sealed_parent.sealed_header();

        let state_provider = self.provider.state_by_block_hash(parent.hash()).map_err(|_| {
            EthApiError::EvmCustom("Failed to initialize EVM state provider".to_string())
        })?;
        let mut db = State::builder()
            .with_database(StateProviderDatabase::new(&state_provider))
            .with_bundle_update()
            .build();

        info!(target: "taiko_rpc_payload_builder", ?parent, "Building prebuilt transaction based on the parent block");

        // Create the block builder based on the parent block and the provided attributes.
        let mut builder = self
            .evm_config
            .builder_for_next_block(
                &mut db,
                parent,
                TaikoNextBlockEnvAttributes {
                    timestamp: parent.timestamp(),
                    suggested_fee_recipient: beneficiary,
                    prev_randao: parent.mix_hash().unwrap_or_default(),
                    gas_limit: block_max_gas_limit * max_transactions_lists,
                    extra_data: parent.extra_data().clone(),
                    base_fee_per_gas: base_fee,
                },
            )
            .map_err(|_| {
                EthApiError::EvmCustom("failed to create block builder from EVM config".to_string())
            })?;

        info!(target: "taiko_rpc_payload_builder", ?base_fee, ?block_max_gas_limit, ?max_bytes_per_tx_list, ?locals, ?max_transactions_lists, "Building prebuilt transaction lists from the pool");

        // Use the shared transaction selection logic
        let config = TxSelectionConfig {
            base_fee,
            gas_limit_per_list: block_max_gas_limit,
            max_da_bytes_per_list: max_bytes_per_tx_list,
            da_size_zlib_guard_bytes: DEFAULT_DA_ZLIB_GUARD_BYTES,
            max_lists: max_transactions_lists as usize,
            min_tip,
            locals: locals.unwrap_or_default(),
        };

        match select_and_execute_pool_transactions(&mut builder, &self.pool, &config, || false) {
            Ok(SelectionOutcome::Completed(lists)) => {
                // Convert ExecutedTxList to PreBuiltTxList with RPC transactions
                lists
                    .into_iter()
                    .map(|list| {
                        let tx_list = list
                            .transactions
                            .into_iter()
                            .map(|etx| {
                                self.tx_resp_builder
                                    .fill_pending(etx.tx)
                                    .map_err(|e| EthApiError::Other(Box::new(e.into())))
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        Ok(PreBuiltTxList {
                            tx_list,
                            estimated_gas_used: list.total_gas_used,
                            bytes_length: list.total_da_bytes,
                        })
                    })
                    .collect::<Result<Vec<_>, EthApiError>>()
                    .map_err(Into::into)
            }
            Ok(SelectionOutcome::Cancelled) => {
                // Should never happen since we pass || false
                unreachable!("tx selection should never be cancelled")
            }
            Err(err) => Err(EthApiError::Internal(err.into()).into()),
        }
    }
}

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

    /// Builds a ProviderFactory wired with both reth and Taiko tables for lookup tests.
    fn create_taiko_test_provider_factory() -> ProviderFactory<MockNodeTypesWithDB> {
        /// Table set that merges reth and Taiko DB tables for the test database.
        struct TaikoTables;

        impl TableSet for TaikoTables {
            /// Returns all tables required for Taiko RPC lookup tests.
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

    #[test]
    /// Confirms Shasta proposal IDs decode correctly from extra data.
    fn parses_shasta_proposal_id_from_extra_data() {
        let extra = [0x2a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        assert_eq!(
            decode_shasta_proposal_id(&extra).map(U256::from),
            Some(U256::from(0x010203040506u64))
        );
    }

    #[test]
    /// Ensures truncated extra data yields no proposal ID.
    fn returns_none_for_truncated_extra_data() {
        assert!(decode_shasta_proposal_id(&[0x2a]).is_none());
    }

    #[test]
    /// Returns uncertainty when the head L1 origin is missing.
    fn returns_uncertain_when_head_l1_origin_missing() {
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
        let api = TaikoAuthExt::new(provider, (), (), ());

        let err = api.resolve_last_block_number_by_batch_id(proposal_id).unwrap_err();
        assert_eq!(err.code(), -32005);
        assert_eq!(
            err.message(),
            "proposal last block uncertain: BatchToLastBlockID missing and no newer proposal observed",
        );
    }

    #[test]
    /// Reports uncertainty when the matching proposal is at the head without a mapping.
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
        let api = TaikoAuthExt::new(provider, (), (), ());

        let err = api.resolve_last_block_number_by_batch_id(proposal_id).unwrap_err();
        assert_eq!(err.code(), -32005);
        assert_eq!(
            err.message(),
            "proposal last block uncertain: BatchToLastBlockID missing and no newer proposal observed",
        );
    }

    #[test]
    /// Verifies the production lookback limit constant.
    fn uses_expected_lookback_limit_constant() {
        assert_eq!(MAX_BACKWARD_SCAN_BLOCKS, 192 * 1024);
    }

    #[test]
    /// Returns lookback-exceeded when scanning beyond the allowed limit.
    fn returns_lookback_exceeded_when_scan_exceeds_limit() {
        let max_scan = max_backward_scan_blocks();
        let head_number = max_scan + 1;
        let target_batch_id = U256::from(1u64);

        let mut input = ANCHOR_V4_SELECTOR.to_vec();
        input.extend_from_slice(&[0u8; 4]);
        let input = Bytes::from(input);

        // Build an anchor transaction with the expected selector prefix.
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
        let api = TaikoAuthExt::new(provider, (), (), ());

        let err = api.resolve_last_block_number_by_batch_id(target_batch_id).unwrap_err();
        assert_eq!(err.code(), -32006);
        assert_eq!(
            err.message(),
            "proposal last block lookback exceeded: BatchToLastBlockID missing and lookback limit reached",
        );
    }

    #[test]
    /// Skips preconfirmation blocks while scanning for the last block by batch ID.
    fn skips_preconfirmation_blocks_when_scanning() {
        // Target batch ID encoded into the block extra data.
        let proposal_id = U256::from(0x010203040506u64);
        // Encoded extra data for both blocks with the matching proposal ID.
        let extra = vec![0x2a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06];

        // Anchor calldata prefix used by the batch lookup.
        let mut input = ANCHOR_V4_SELECTOR.to_vec();
        input.extend_from_slice(&[0u8; 4]);
        // Anchor calldata used for transactions in both blocks.
        let input = Bytes::from(input);

        // Builds an anchor transaction with the provided calldata.
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

        // Provider factory with Taiko tables enabled.
        let factory = create_taiko_test_provider_factory();
        // Writable provider used to insert test blocks.
        let mut provider_rw = factory.provider_rw().expect("provider rw");
        // Genesis header for the chain.
        let genesis_header = Header { number: 0, gas_limit: 1_000_000, ..Default::default() };
        // Genesis block for the chain.
        let genesis_block = genesis_header.into_block(BlockBody::default());
        // Recovered genesis block for insertion.
        let genesis_recovered = RecoveredBlock::new_unhashed(genesis_block, vec![]);
        provider_rw.insert_block(&genesis_recovered).expect("insert genesis block");

        // Captures the header of the chain head.
        let mut head_header = None;

        for number in 1..=2u64 {
            let header = Header {
                number,
                gas_limit: 1_000_000,
                extra_data: extra.clone().into(),
                ..Default::default()
            };
            // Anchor transaction for the test block.
            let anchor_tx = build_anchor_tx(&input);
            let body = BlockBody { transactions: vec![anchor_tx], ..Default::default() };
            let block = header.clone().into_block(body);
            let recovered = RecoveredBlock::new_unhashed(block, vec![Address::ZERO]);
            provider_rw.insert_block(&recovered).expect("insert block");

            if number == 2 {
                head_header = Some(header);
            }
        }

        {
            let tx = provider_rw.tx_mut();
            // Stored L1 origin for the matching, confirmed block.
            let confirmed_origin = StoredL1Origin {
                block_id: U256::from(1u64),
                l2_block_hash: Default::default(),
                l1_block_height: U256::from(1u64),
                l1_block_hash: Default::default(),
                build_payload_args_id: [0u8; 8],
                is_forced_inclusion: false,
                signature: [0u8; 65],
            };
            tx.put::<StoredL1OriginTable>(1, confirmed_origin)
                .expect("insert confirmed l1 origin");
            // Stored L1 origin for the preconfirmation head block.
            let preconf_origin = StoredL1Origin {
                block_id: U256::from(2u64),
                l2_block_hash: Default::default(),
                l1_block_height: U256::ZERO,
                l1_block_hash: Default::default(),
                build_payload_args_id: [0u8; 8],
                is_forced_inclusion: false,
                signature: [0u8; 65],
            };
            tx.put::<StoredL1OriginTable>(2, preconf_origin)
                .expect("insert preconf l1 origin");
        }
        provider_rw.commit().expect("commit");

        // Latest header set to the preconfirmation head block.
        let latest = SealedHeader::seal_slow(head_header.expect("head header"));
        // Provider scoped to the latest header.
        let provider = BlockchainProvider::with_latest(factory, latest).expect("provider");
        // Auth RPC wrapper for batch lookup helpers.
        let api = TaikoAuthExt::new(provider, (), (), ());

        // Resolved block ID should skip preconfirmation and land on block 1.
        let block_id = api.resolve_last_block_number_by_batch_id(proposal_id).unwrap();
        assert_eq!(block_id, U256::from(1u64));
    }
}
