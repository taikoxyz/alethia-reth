use alethia_reth_db::model::{
    BatchToLastBlock, STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1OriginTable,
};
use alloy_rlp::Encodable;
use reth::network::eth_requests::SOFT_RESPONSE_LIMIT;
use reth_db_api::{cursor::DbCursorRO, transaction::DbTx};
use reth_ethereum::EthPrimitives;
use reth_node_api::NodeTypesWithDB;
use reth_provider::{
    DatabaseProviderFactory,
    providers::{BlockchainProvider, ProviderNodeTypes},
};
use reth_storage_errors::provider::ProviderResult;

use crate::taiko_aux::message::{AuxBatchLastBlockEntry, AuxL1OriginEntry, AuxRangeRequest};

/// Maximum number of auxiliary rows served in a single response.
pub const MAX_AUX_ROWS_SERVE: usize = 2048;

/// Provider trait for serving `taiko_aux` protocol data.
pub trait TaikoAuxProtocolProvider: Send + Sync {
    /// Returns rows from `StoredL1OriginTable`.
    fn l1_origins(&self, request: AuxRangeRequest) -> ProviderResult<Vec<AuxL1OriginEntry>>;

    /// Returns the current `StoredL1HeadOriginTable` pointer.
    fn head_l1_origin(&self) -> ProviderResult<Option<u64>>;

    /// Returns rows from `BatchToLastBlock`.
    fn batch_last_blocks(
        &self,
        request: AuxRangeRequest,
    ) -> ProviderResult<Vec<AuxBatchLastBlockEntry>>;
}

/// Reth-backed implementation of [`TaikoAuxProtocolProvider`].
#[derive(Clone, Debug)]
pub struct RethTaikoAuxProvider<P: NodeTypesWithDB> {
    /// Blockchain provider used to read aux table rows from local database.
    provider: BlockchainProvider<P>,
}

impl<P: NodeTypesWithDB> RethTaikoAuxProvider<P> {
    /// Creates a new provider from the node's blockchain provider.
    pub const fn new(provider: BlockchainProvider<P>) -> Self {
        Self { provider }
    }
}

#[inline]
/// Normalizes request size against protocol and implementation bounds.
fn normalize_limit(limit: u64) -> usize {
    if limit == 0 {
        return 0;
    }
    limit.min(MAX_AUX_ROWS_SERVE as u64) as usize
}

impl<P> TaikoAuxProtocolProvider for RethTaikoAuxProvider<P>
where
    P: ProviderNodeTypes<Primitives = EthPrimitives>,
{
    /// Serves a bounded range of rows from `StoredL1OriginTable`.
    fn l1_origins(&self, request: AuxRangeRequest) -> ProviderResult<Vec<AuxL1OriginEntry>> {
        let limit = normalize_limit(request.limit);
        if limit == 0 {
            return Ok(Vec::new());
        }

        let db_provider = self.provider.database_provider_ro()?;
        let mut cursor = db_provider.tx_ref().cursor_read::<StoredL1OriginTable>()?;
        let mut rows = Vec::with_capacity(limit);
        let mut total_bytes = 0usize;

        for row in cursor.walk_range(request.start..)? {
            let (block_number, value) = row?;
            let entry = AuxL1OriginEntry::from_table_row(block_number, value);
            total_bytes = total_bytes.saturating_add(entry.length());
            rows.push(entry);

            if rows.len() >= limit || total_bytes > SOFT_RESPONSE_LIMIT {
                break;
            }
        }

        Ok(rows)
    }

    /// Serves the latest head origin pointer from `StoredL1HeadOriginTable`.
    fn head_l1_origin(&self) -> ProviderResult<Option<u64>> {
        let db_provider = self.provider.database_provider_ro()?;
        Ok(db_provider.tx_ref().get::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY)?)
    }

    /// Serves a bounded range of rows from `BatchToLastBlock`.
    fn batch_last_blocks(
        &self,
        request: AuxRangeRequest,
    ) -> ProviderResult<Vec<AuxBatchLastBlockEntry>> {
        let limit = normalize_limit(request.limit);
        if limit == 0 {
            return Ok(Vec::new());
        }

        let db_provider = self.provider.database_provider_ro()?;
        let mut cursor = db_provider.tx_ref().cursor_read::<BatchToLastBlock>()?;
        let mut rows = Vec::with_capacity(limit);
        let mut total_bytes = 0usize;

        for row in cursor.walk_range(request.start..)? {
            let (batch_id, block_number) = row?;
            let entry = AuxBatchLastBlockEntry { batch_id, block_number };
            total_bytes = total_bytes.saturating_add(entry.length());
            rows.push(entry);

            if rows.len() >= limit || total_bytes > SOFT_RESPONSE_LIMIT {
                break;
            }
        }

        Ok(rows)
    }
}
