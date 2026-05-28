//! Proof-history backed overrides for selected `debug_` RPC methods.

use crate::proof_state::ProofHistoryStateProviderFactory;
use alethia_reth_block::executor::is_zk_gas_limit_exceeded;
use alethia_reth_db::model::{TerminalZkGasTx, TerminalZkGasTxTable};
use alloy_consensus::BlockHeader;
use alloy_eips::{BlockId, BlockNumberOrTag, Decodable2718};
use alloy_primitives::B256;
use alloy_rpc_types_debug::ExecutionWitness;
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_db::transaction::DbTx;
use reth_ethereum::{EthPrimitives, TransactionSigned};
use reth_ethereum_primitives::Block;
use reth_evm::{ConfigureEvm, execute::Executor};
use reth_optimism_trie::{OpProofsStorage, OpProofsStore};
use reth_primitives_traits::{Block as RethBlock, RecoveredBlock, SignedTransaction as _};
use reth_provider::{DBProvider, DatabaseProviderFactory, HeaderProvider};
use reth_revm::{State, database::StateProviderDatabase, witness::ExecutionWitnessRecord};
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;
use reth_trie_common::ExecutionWitnessMode;
use tokio::sync::Semaphore;

/// Maximum number of concurrent proof-history witness requests.
const MAX_CONCURRENT_WITNESS_REQUESTS: usize = 3;

/// RPC server trait for Taiko proof-history backed `debug_` witness methods.
#[cfg_attr(not(test), rpc(server, namespace = "debug"))]
#[cfg_attr(test, rpc(server, client, namespace = "debug"))]
pub trait TaikoDebugWitnessApi {
    /// Returns an execution witness for a canonical block number or tag.
    #[method(name = "executionWitness")]
    async fn execution_witness(
        &self,
        block: BlockNumberOrTag,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness>;

    /// Returns an execution witness for a block hash.
    #[method(name = "executionWitnessByBlockHash")]
    async fn execution_witness_by_block_hash(
        &self,
        hash: B256,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness>;
}

/// `debug_` namespace overrides that use proof-history state for witness generation.
#[derive(Debug)]
pub struct TaikoDebugWitnessExt<Eth, Storage, Provider> {
    /// Provider used to fetch ancestor block headers for the returned witness.
    provider: Provider,
    /// Ethereum RPC API used to load blocks, state, and the EVM configuration.
    eth_api: Eth,
    /// Factory for sidecar-backed state providers.
    state_provider_factory: ProofHistoryStateProviderFactory<Eth, Storage>,
    /// Semaphore limiting concurrent witness generation.
    semaphore: Semaphore,
}

impl<Eth, Storage, Provider> TaikoDebugWitnessExt<Eth, Storage, Provider>
where
    Eth: FullEthApi<Primitives = EthPrimitives> + Send + Sync + 'static,
    Eth::Provider: DatabaseProviderFactory,
    Storage: OpProofsStore + Clone + 'static,
    Provider: HeaderProvider + Clone + Send + Sync + 'static,
    Provider::Header: BlockHeader + alloy_rlp::Encodable,
{
    /// Creates a new proof-history backed debug witness override.
    pub fn new(provider: Provider, eth_api: Eth, storage: OpProofsStorage<Storage>) -> Self {
        Self {
            provider,
            state_provider_factory: ProofHistoryStateProviderFactory::new(eth_api.clone(), storage),
            eth_api,
            semaphore: Semaphore::new(MAX_CONCURRENT_WITNESS_REQUESTS),
        }
    }

    /// Re-executes the requested block and returns the generated witness.
    async fn execution_witness_for_id(
        &self,
        block_id: BlockId,
        mode: ExecutionWitnessMode,
    ) -> Result<ExecutionWitness, Eth::Error> {
        let block = self
            .eth_api
            .recovered_block(block_id)
            .await?
            .ok_or(EthApiError::HeaderNotFound(block_id))?;
        let block_number = block.header().number();
        let block_hash = block.hash();
        let parent_block = BlockId::Hash(block.parent_hash().into());
        // Proof-history state is still the witness source of truth. The terminal tx record is only
        // a durable recovery index for bytes that are not part of the canonical block body.
        let state_provider = self
            .state_provider_factory
            .state_provider(parent_block)
            .await
            .map_err(EthApiError::from)?;
        let terminal_zkgas_tx = self.terminal_zkgas_recovery_tx(block_number, block_hash)?;
        let execution_block = match terminal_zkgas_tx.as_ref() {
            Some(tx) => block_with_terminal_zkgas_tx(std::sync::Arc::unwrap_or_clone(block), tx)?,
            None => std::sync::Arc::unwrap_or_clone(block),
        };
        let db = StateProviderDatabase::new(&*state_provider);
        let block_executor = self.eth_api.evm_config().executor(db);
        let mut witness_record = ExecutionWitnessRecord::default();

        if terminal_zkgas_tx.is_some() {
            let result = block_executor.execute_with_state_closure_always(
                &execution_block,
                |statedb: &State<_>| {
                    witness_record.record_executed_state(statedb, mode);
                },
            );
            if let Err(err) = result &&
                !is_zk_gas_limit_exceeded(&err)
            {
                return Err(EthApiError::from(err).into());
            }
        } else {
            block_executor
                .execute_with_state_closure(&execution_block, |statedb: &State<_>| {
                    witness_record.record_executed_state(statedb, mode);
                })
                .map_err(EthApiError::from)?;
        }

        Ok(witness_record
            .into_execution_witness(&*state_provider, &self.provider, block_number, mode)
            .map_err(EthApiError::from)?)
    }

    /// Reads the durable terminal zk-gas recovery record for a canonical block.
    fn terminal_zkgas_recovery_tx(
        &self,
        block_number: u64,
        block_hash: B256,
    ) -> Result<Option<TerminalZkGasTx>, Eth::Error> {
        read_terminal_zkgas_recovery_tx(self.eth_api.provider(), block_number, block_hash)
            .map_err(Into::into)
    }
}

/// Reads a persisted terminal zk-gas recovery record when it belongs to the canonical block hash.
fn read_terminal_zkgas_recovery_tx<Provider>(
    provider: &Provider,
    block_number: u64,
    block_hash: B256,
) -> Result<Option<TerminalZkGasTx>, EthApiError>
where
    Provider: DatabaseProviderFactory,
{
    let db_provider = provider.database_provider_ro().map_err(|err| {
        EthApiError::EvmCustom(format!("failed to open terminal zk-gas tx table: {err}"))
    })?;
    let terminal_tx =
        db_provider.into_tx().get::<TerminalZkGasTxTable>(block_number).map_err(|err| {
            EthApiError::EvmCustom(format!("failed to read terminal zk-gas tx: {err}"))
        })?;

    Ok(terminal_tx.filter(|tx| tx.block_hash == block_hash))
}

/// Appends a filtered terminal zk-gas transaction to the execution-only block.
///
/// The recovered block is not canonicalized; it is only used for witness replay. A terminal
/// transaction must not already exist in the canonical body, and when present is appended as the
/// final transaction because payload building discards every later candidate.
fn block_with_terminal_zkgas_tx(
    block: RecoveredBlock<Block>,
    terminal_tx: &TerminalZkGasTx,
) -> Result<RecoveredBlock<Block>, EthApiError> {
    let block_hash = block.hash();
    let mut senders = block.senders().to_vec();
    let (header, mut body) = block.into_block().split();
    let tx = TransactionSigned::decode_2718_exact(terminal_tx.tx_rlp.as_ref()).map_err(|err| {
        EthApiError::EvmCustom(format!("failed to decode terminal zk-gas tx: {err}"))
    })?;

    if *tx.tx_hash() != terminal_tx.tx_hash {
        return Err(EthApiError::EvmCustom(format!(
            "terminal zk-gas tx hash mismatch: expected {:?}, got {:?}",
            terminal_tx.tx_hash,
            tx.tx_hash()
        )));
    }
    if body.transactions.iter().any(|included| *included.tx_hash() == terminal_tx.tx_hash) {
        return Err(EthApiError::EvmCustom(format!(
            "terminal zk-gas tx {:?} already exists in canonical block body",
            terminal_tx.tx_hash
        )));
    }

    let signer = tx.try_recover().map_err(|err| {
        EthApiError::EvmCustom(format!("failed to recover terminal zk-gas tx signer: {err}"))
    })?;
    body.transactions.push(tx);
    senders.push(signer);

    Ok(RecoveredBlock::new(Block::new(header, body), senders, block_hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{BlockBody, Header, TxLegacy};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, ChainId, Signature, TxKind, U256};
    use reth_db::{
        TableSet, Tables,
        mdbx::{DatabaseArguments, init_db_for},
        table::TableInfo,
        test_utils::{
            TempDatabase, create_test_rocksdb_dir, create_test_static_files_dir, tempdir_path,
        },
    };
    use reth_db_api::transaction::DbTxMut;
    use reth_ethereum::{TransactionSigned, chainspec::MAINNET};
    use reth_provider::{
        ProviderFactory,
        providers::{RocksDBBuilder, StaticFileProvider},
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
        let db = init_db_for::<PathBuf, TaikoTables>(path.clone(), DatabaseArguments::test())
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
            reth::tasks::Runtime::test(),
        )
        .expect("failed to create test provider factory")
    }

    fn signed_legacy_tx(nonce: u64) -> TransactionSigned {
        let tx = TxLegacy {
            chain_id: Some(ChainId::from(1u64)),
            nonce,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::with_last_byte(0x42)),
            value: U256::ZERO,
            input: nonce.to_be_bytes().to_vec().into(),
        };
        TransactionSigned::new_unhashed(
            tx.into(),
            Signature::new(U256::from(nonce + 1), U256::from(nonce + 2), false),
        )
    }

    #[test]
    fn terminal_zkgas_recovery_round_trip_rebuilds_execution_block() {
        let factory = create_taiko_test_provider_factory();
        let canonical_tx = signed_legacy_tx(0);
        let terminal_tx = signed_legacy_tx(1);
        let canonical_signer = canonical_tx.try_recover().expect("canonical tx should recover");
        let block_number = 42;
        let block = Header { number: block_number, gas_limit: 30_000_000, ..Default::default() }
            .into_block(BlockBody {
                transactions: vec![canonical_tx.clone()],
                ..Default::default()
            });
        let recovered_block = RecoveredBlock::new_unhashed(block, vec![canonical_signer]);
        let block_hash = recovered_block.hash();

        let tx = factory.database_provider_rw().expect("database provider").into_tx();
        tx.put::<TerminalZkGasTxTable>(
            block_number,
            TerminalZkGasTx {
                block_hash,
                tx_hash: *terminal_tx.tx_hash(),
                tx_rlp: terminal_tx.encoded_2718().into(),
            },
        )
        .expect("persist terminal tx");
        tx.commit().expect("commit terminal tx");

        let recovered_tx = read_terminal_zkgas_recovery_tx(&factory, block_number, block_hash)
            .expect("read recovery tx")
            .expect("terminal tx should be present");
        let execution_block =
            block_with_terminal_zkgas_tx(recovered_block, &recovered_tx).expect("rebuild block");
        let transactions = execution_block.body().transactions().collect::<Vec<_>>();

        assert_eq!(execution_block.hash(), block_hash);
        assert_eq!(transactions.len(), 2);
        assert_eq!(*transactions[0].tx_hash(), *canonical_tx.tx_hash());
        assert_eq!(*transactions[1].tx_hash(), *terminal_tx.tx_hash());
    }

    #[test]
    fn terminal_zkgas_recovery_ignores_other_block_hashes() {
        let factory = create_taiko_test_provider_factory();
        let terminal_tx = signed_legacy_tx(1);
        let block_number = 42;
        let block_hash = B256::repeat_byte(0x11);

        let tx = factory.database_provider_rw().expect("database provider").into_tx();
        tx.put::<TerminalZkGasTxTable>(
            block_number,
            TerminalZkGasTx {
                block_hash,
                tx_hash: *terminal_tx.tx_hash(),
                tx_rlp: terminal_tx.encoded_2718().into(),
            },
        )
        .expect("persist terminal tx");
        tx.commit().expect("commit terminal tx");

        let recovered =
            read_terminal_zkgas_recovery_tx(&factory, block_number, B256::repeat_byte(0x22))
                .expect("read recovery tx");

        assert!(recovered.is_none());
    }

    #[test]
    fn terminal_zkgas_recovery_rejects_duplicate_canonical_tx() {
        let terminal_tx = signed_legacy_tx(1);
        let signer = terminal_tx.try_recover().expect("terminal tx should recover");
        let block = Header { number: 42, gas_limit: 30_000_000, ..Default::default() }.into_block(
            BlockBody { transactions: vec![terminal_tx.clone()], ..Default::default() },
        );
        let recovered_block = RecoveredBlock::new_unhashed(block, vec![signer]);
        let terminal_tx = TerminalZkGasTx {
            block_hash: recovered_block.hash(),
            tx_hash: *terminal_tx.tx_hash(),
            tx_rlp: terminal_tx.encoded_2718().into(),
        };

        let err = block_with_terminal_zkgas_tx(recovered_block, &terminal_tx).unwrap_err();

        assert!(err.to_string().contains("already exists in canonical block body"));
    }
}

#[async_trait]
impl<Eth, Storage, Provider> TaikoDebugWitnessApiServer
    for TaikoDebugWitnessExt<Eth, Storage, Provider>
where
    Eth: FullEthApi<Primitives = EthPrimitives> + Send + Sync + 'static,
    Eth::Provider: DatabaseProviderFactory,
    Storage: OpProofsStore + Clone + 'static,
    Provider: HeaderProvider + Clone + Send + Sync + 'static,
    Provider::Header: BlockHeader + alloy_rlp::Encodable,
{
    /// Handles `debug_executionWitness` with proof-history backed state.
    async fn execution_witness(
        &self,
        block: BlockNumberOrTag,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness> {
        let _permit = self.semaphore.acquire().await;
        self.execution_witness_for_id(block.into(), mode.unwrap_or_default())
            .await
            .map_err(Into::into)
    }

    /// Handles `debug_executionWitnessByBlockHash` with proof-history backed state.
    async fn execution_witness_by_block_hash(
        &self,
        hash: B256,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness> {
        let _permit = self.semaphore.acquire().await;
        self.execution_witness_for_id(hash.into(), mode.unwrap_or_default())
            .await
            .map_err(Into::into)
    }
}
