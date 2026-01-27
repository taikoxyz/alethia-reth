use alloy_consensus::{BlockHeader as _, Transaction as _};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::U256;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_db_api::{DatabaseError, transaction::DbTx};
use reth_primitives_traits::{Block as _, BlockBody as _};
use reth_provider::{BlockReaderIdExt, DBProvider, DatabaseProviderFactory};
use reth_rpc_eth_types::EthApiError;

use crate::eth::error::TaikoApiError;
use alethia_reth_consensus::validation::ANCHOR_V4_SELECTOR;
use alethia_reth_db::model::{
    BatchToLastBlock, STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1OriginTable,
};
use alethia_reth_primitives::{decode_shasta_proposal_id, payload::attributes::RpcL1Origin};

/// trait interface for a custom rpc namespace: `taiko`
///
/// This defines the Taiko namespace where all methods are configured as trait functions.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "taiko"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "taiko"))]
pub trait TaikoExtApi {
    #[method(name = "l1OriginByID")]
    fn l1_origin_by_id(&self, id: U256) -> RpcResult<Option<RpcL1Origin>>;
    #[method(name = "headL1Origin")]
    fn head_l1_origin(&self) -> RpcResult<Option<RpcL1Origin>>;
    #[method(name = "lastL1OriginByBatchID")]
    fn last_l1_origin_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<RpcL1Origin>>;
    #[method(name = "lastBlockIDByBatchID")]
    fn last_block_id_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<U256>>;
}

/// The Taiko RPC extension implementation.
pub struct TaikoExt<Provider>
where
    Provider: DatabaseProviderFactory + BlockReaderIdExt,
{
    provider: Provider,
}

/// The result of searching for the last block number by batch ID.
#[derive(Debug, PartialEq, Eq)]
enum LastBlockSearchResult {
    Found(u64),
    NotFound,
    UncertainAtHead,
}

impl<Provider> TaikoExt<Provider>
where
    Provider: DatabaseProviderFactory + BlockReaderIdExt,
{
    /// Checks if the given database error indicates a missing table or key.
    fn is_missing_table_error(error: &DatabaseError) -> bool {
        match error {
            DatabaseError::Open(info) | DatabaseError::Read(info) => {
                info.code == -30798 ||
                    info.message.as_ref().contains("no matching key/data pair found")
            }
            _ => false,
        }
    }

    /// Creates a new instance of `TaikoExt` with the given provider.
    pub fn new(provider: Provider) -> Self {
        Self { provider }
    }

    /// Finds the last Shasta block number that contains an Anchor transaction with the given batch
    /// ID. It scans blocks backwards from the latest block until it finds a matching
    /// transaction or reaches the genesis block.
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

        while let Some(block) = current_block {
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
        }
    }
}

impl<Provider> TaikoExtApiServer for TaikoExt<Provider>
where
    Provider: DatabaseProviderFactory + BlockReaderIdExt + 'static,
{
    /// Retrieves the L1 origin by its ID from the database.
    fn l1_origin_by_id(&self, id: U256) -> RpcResult<Option<RpcL1Origin>> {
        let provider =
            self.provider.database_provider_ro().map_err(|_| EthApiError::InternalEthError)?;

        Ok(Some(
            provider
                .into_tx()
                .get::<StoredL1OriginTable>(id.to())
                .map_err(|_| EthApiError::InternalEthError)?
                .ok_or(TaikoApiError::GethNotFound)?
                .into_rpc(),
        ))
    }

    /// Retrieves the head L1 origin from the database.
    fn head_l1_origin(&self) -> RpcResult<Option<RpcL1Origin>> {
        let provider =
            self.provider.database_provider_ro().map_err(|_| EthApiError::InternalEthError)?;

        self.l1_origin_by_id(U256::from(
            provider
                .into_tx()
                .get::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY)
                .map_err(|_| EthApiError::InternalEthError)?
                .ok_or(TaikoApiError::GethNotFound)?,
        ))
    }

    /// Retrieves the last L1 origin by its batch ID from the database.
    fn last_l1_origin_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<RpcL1Origin>> {
        self.l1_origin_by_id(self.resolve_last_block_number_by_batch_id(batch_id)?)
    }

    /// Retrieves the last block ID for the given batch ID.
    fn last_block_id_by_batch_id(&self, batch_id: U256) -> RpcResult<Option<U256>> {
        Ok(Some(self.resolve_last_block_number_by_batch_id(batch_id)?))
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

    #[test]
    fn parses_shasta_proposal_id_from_extra_data() {
        let extra = [0x2a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        assert_eq!(
            decode_shasta_proposal_id(&extra).map(U256::from),
            Some(U256::from(0x010203040506u64))
        );
    }

    #[test]
    fn returns_none_for_truncated_extra_data() {
        assert!(decode_shasta_proposal_id(&[0x2a]).is_none());
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
        let api = TaikoExt::new(provider);

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
        let api = TaikoExt::new(provider);

        let err = api.resolve_last_block_number_by_batch_id(proposal_id).unwrap_err();
        assert_eq!(err.code(), -32005);
        assert_eq!(
            err.message(),
            "proposal last block uncertain: BatchToLastBlockID missing and no newer proposal observed",
        );
    }
}
