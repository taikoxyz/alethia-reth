//! Batch-to-last-block lookup helpers for `taikoAuth` RPC methods.

use alloy_consensus::{BlockHeader as _, Transaction as _};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::U256;
use jsonrpsee::core::RpcResult;
use reth_db_api::{DatabaseError, transaction::DbTx};
use reth_node_api::Block;
use reth_primitives_traits::BlockBody as _;
use reth_provider::{BlockReaderIdExt, DBProvider, DatabaseProviderFactory};
use reth_rpc_eth_types::EthApiError;

use crate::eth::{
    error::{TaikoApiError, internal_eth_error},
    eth::into_rpc,
};
use alethia_reth_consensus::validation::ANCHOR_V4_SELECTOR;
use alethia_reth_db::model::{BatchToLastBlock, StoredL1OriginTable};
use alethia_reth_primitives::{decode_shasta_proposal_id, payload::attributes::RpcL1Origin};

use super::TaikoAuthExt;

/// Outcome of searching for the last block number by batch ID.
#[derive(Debug, PartialEq, Eq)]
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
pub(super) const MAX_BACKWARD_SCAN_BLOCKS: u64 = 192 * 21_600;
#[cfg(test)]
/// Shorter backward scan limit for test execution.
const TEST_MAX_BACKWARD_SCAN_BLOCKS: u64 = 64;

/// Returns the scan limit, using the test override when enabled.
pub(super) fn max_backward_scan_blocks() -> u64 {
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
    /// Reads the cached last block number for a batch ID from the database mapping only.
    pub(super) fn read_cached_last_block_number_by_batch_id(
        &self,
        batch_id: U256,
    ) -> RpcResult<Option<U256>> {
        let provider = self.provider.database_provider_ro().map_err(internal_eth_error)?;
        let batch_lookup = provider.into_tx().get::<BatchToLastBlock>(batch_id.to());
        if let Ok(Some(block_number)) = batch_lookup {
            return Ok(Some(U256::from(block_number)));
        }
        if let Err(error) = batch_lookup &&
            !Self::is_missing_table_error(&error)
        {
            return Err(internal_eth_error(error).into());
        }

        Ok(None)
    }

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
            .map_err(internal_eth_error)?;
        // If there is no head, the lookup cannot proceed.
        let Some(latest_block) = latest_block else {
            return Ok(LastBlockSearchResult::NotFound);
        };
        // Capture the latest block number to detect head-only matches.
        let head_number = latest_block.header().number();
        // Read-only database provider for L1 origin lookups during scanning.
        let db_provider = self.provider.database_provider_ro().map_err(internal_eth_error)?;
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
            // Anchor transactions are expected to be first; bail if missing.
            let Some(first_tx) = block.body().transactions().first() else {
                break;
            };

            let input = first_tx.input();
            let input = input.as_ref();

            // Stop scanning once the anchor selector is no longer present.
            if !input.starts_with(ANCHOR_V4_SELECTOR) {
                break;
            }

            // Current block number used for DB lookups and traversal.
            let block_number = block.header().number();
            if block_number == 0 {
                break;
            }

            // Decode the Shasta proposal ID from the block extra data.
            let extra = block.header().extra_data();
            let extra = extra.as_ref();
            let Some(proposal_id) = decode_shasta_proposal_id(extra).map(U256::from) else {
                return Err(EthApiError::InvalidParams(format!(
                    "extraData too short for proposalId: {}",
                    extra.len()
                )));
            };

            if proposal_id < batch_id {
                return Ok(LastBlockSearchResult::NotFound);
            }

            if proposal_id > batch_id {
                // Walk backwards one block and continue scanning.
                current_block = self
                    .provider
                    .block_by_number_or_tag(BlockNumberOrTag::Number(block_number - 1))
                    .map_err(internal_eth_error)?;
                continue;
            }

            // L1 origin lookup to skip preconfirmation blocks.
            let mut has_l1_origin = false;
            let l1_origin_lookup = db_tx.get::<StoredL1OriginTable>(block_number);
            match l1_origin_lookup {
                Ok(Some(l1_origin)) => {
                    // Skip preconfirmation blocks where L1 height is zero.
                    if l1_origin.l1_block_height == U256::ZERO {
                        // Move to the previous block and continue scanning.
                        current_block = self
                            .provider
                            .block_by_number_or_tag(BlockNumberOrTag::Number(block_number - 1))
                            .map_err(internal_eth_error)?;
                        continue;
                    }
                    has_l1_origin = true;
                }
                Ok(None) => {}
                Err(error) => {
                    // Treat missing-table errors as empty data; fail on real DB errors.
                    if !Self::is_missing_table_error(&error) {
                        return Err(internal_eth_error(error));
                    }
                }
            }

            // A match at the head is still uncertain without a cache mapping.
            if head_number == block.header().number() {
                if !has_l1_origin {
                    return Ok(LastBlockSearchResult::UncertainAtHead);
                }
                return Ok(LastBlockSearchResult::Found(block.header().number()));
            }
            // Found a confirmed match below head.
            return Ok(LastBlockSearchResult::Found(block.header().number()));
        }

        Ok(LastBlockSearchResult::NotFound)
    }

    /// Resolves the last block number, preferring the DB cache before scanning.
    pub(super) fn resolve_last_block_number_by_batch_id(&self, batch_id: U256) -> RpcResult<U256> {
        if let Some(block_number) = self.read_cached_last_block_number_by_batch_id(batch_id)? {
            return Ok(block_number);
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

    /// Reads a stored L1 origin by resolved block ID.
    pub(super) fn read_l1_origin_by_block_id(
        &self,
        block_id: U256,
    ) -> RpcResult<Option<RpcL1Origin>> {
        let provider = self.provider.database_provider_ro().map_err(internal_eth_error)?;
        Ok(provider
            .into_tx()
            .get::<StoredL1OriginTable>(block_id.to())
            .map_err(internal_eth_error)?
            .map(into_rpc))
    }
}
