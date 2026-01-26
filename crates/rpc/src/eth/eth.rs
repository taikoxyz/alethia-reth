use alloy_consensus::{BlockHeader as _, Transaction as _};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::U256;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_db_api::transaction::DbTx;
use reth_primitives_traits::{Block as _, BlockBody as _};
use reth_provider::{BlockReaderIdExt, DBProvider, DatabaseProviderFactory};
use reth_rpc_eth_types::EthApiError;

use crate::eth::error::TaikoApiError;
use alethia_reth_consensus::validation::ANCHOR_V4_SELECTOR;
use alethia_reth_db::model::{
    STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1OriginTable,
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

impl<Provider> TaikoExt<Provider>
where
    Provider: DatabaseProviderFactory + BlockReaderIdExt,
{
    /// Creates a new instance of `TaikoExt` with the given provider.
    pub fn new(provider: Provider) -> Self {
        Self { provider }
    }

    /// Retrieves the last block number for a batch by scanning the latest Shasta blocks.
    fn resolve_last_block_number_by_batch_id(&self, batch_id: U256) -> RpcResult<U256> {
        let mut current_block = self
            .provider
            .block_by_number_or_tag(BlockNumberOrTag::Latest)
            .map_err(|e| EthApiError::Internal(e.into()))?;

        let Some(latest_block) = current_block else {
            return Err(TaikoApiError::GethNotFound.into());
        };

        let Some(first_tx) = latest_block.body().transactions().first() else {
            return Err(TaikoApiError::GethNotFound.into());
        };

        if !first_tx.input().as_ref().starts_with(ANCHOR_V4_SELECTOR) {
            return Err(TaikoApiError::GethNotFound.into());
        }

        let (latest_proposal_id, _) =
            decode_shasta_proposal_id(latest_block.header().extra_data().as_ref())
                .map_err(|_| TaikoApiError::GethNotFound)?;

        if batch_id > U256::from(latest_proposal_id) {
            return Err(TaikoApiError::GethNotFound.into());
        }

        current_block = Some(latest_block);

        while let Some(block) = current_block {
            let Some(first_tx) = block.body().transactions().first() else {
                break;
            };

            if !first_tx.input().as_ref().starts_with(ANCHOR_V4_SELECTOR) {
                break;
            }

            let (proposal_id, end_of_proposal) =
                decode_shasta_proposal_id(block.header().extra_data().as_ref())
                    .map_err(|_| TaikoApiError::GethNotFound)?;
            let proposal_id = U256::from(proposal_id);

            if proposal_id == batch_id {
                if !end_of_proposal {
                    return Err(TaikoApiError::GethNotFound.into());
                }
                return Ok(U256::from(block.header().number()));
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

        Err(TaikoApiError::GethNotFound.into())
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

    #[test]
    fn parses_shasta_proposal_id_from_extra_data() {
        let extra = [0x2a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x01];
        let (proposal_id, end_of_proposal) = decode_shasta_proposal_id(&extra).unwrap();
        assert_eq!(U256::from(proposal_id), U256::from(0x010203040506u64));
        assert!(end_of_proposal);
    }

    #[test]
    fn returns_error_for_invalid_extra_data_len() {
        assert!(decode_shasta_proposal_id(&[0x2a]).is_err());
    }
}
