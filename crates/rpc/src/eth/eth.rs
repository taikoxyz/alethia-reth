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
    BatchToLastBlock, STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1OriginTable,
};
use alethia_reth_primitives::payload::attributes::RpcL1Origin;

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

    /// Finds the last Shasta block number that contains an Anchor transaction with the given batch
    /// ID. It scans blocks backwards from the latest block until it finds a matching
    /// transaction or reaches the genesis block.
    fn find_last_block_number_by_batch_id(
        &self,
        batch_id: U256,
    ) -> Result<Option<u64>, EthApiError> {
        let mut current_block = self
            .provider
            .block_by_number_or_tag(BlockNumberOrTag::Latest)
            .map_err(|e| EthApiError::Internal(e.into()))?;

        while let Some(block) = current_block {
            let Some(first_tx) = block.body().transactions().first() else {
                break;
            };

            let input = first_tx.input();
            let input = input.as_ref();

            if !input.starts_with(ANCHOR_V4_SELECTOR) {
                break;
            }

            let Some(proposal_id) = extract_anchor_v4_proposal_id(input) else {
                break;
            };

            if proposal_id == batch_id {
                return Ok(Some(block.header().number()));
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

        Ok(None)
    }

    /// Retrieves the last block number for a batch, preferring the DB cache before falling back to
    /// a scan.
    fn resolve_last_block_number_by_batch_id(&self, batch_id: U256) -> RpcResult<U256> {
        let provider =
            self.provider.database_provider_ro().map_err(|_| EthApiError::InternalEthError)?;
        if let Some(block_number) = provider
            .into_tx()
            .get::<BatchToLastBlock>(batch_id.to())
            .map_err(|_| EthApiError::InternalEthError)?
        {
            return Ok(U256::from(block_number));
        }

        let block_number = self
            .find_last_block_number_by_batch_id(batch_id)?
            .ok_or(TaikoApiError::GethNotFound)?;

        Ok(U256::from(block_number))
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

/// Parses the proposal ID encoded in the first argument of an `anchorV4` call.
fn extract_anchor_v4_proposal_id(input: &[u8]) -> Option<U256> {
    const SELECTOR_LEN: usize = 4;
    const WORD_SIZE: usize = 32;
    const PROPOSAL_ID_WORD_OFFSET: usize = 1; // `submissionWindowEnd` precedes `proposalId`
    const PROPOSAL_ID_BYTE_OFFSET: usize = WORD_SIZE * PROPOSAL_ID_WORD_OFFSET;

    if input.len() < SELECTOR_LEN + WORD_SIZE {
        return None;
    }

    let calldata = &input[SELECTOR_LEN..];
    if calldata.len() < WORD_SIZE {
        return None;
    }

    let mut offset_bytes = [0u8; WORD_SIZE];
    offset_bytes.copy_from_slice(&calldata[..WORD_SIZE]);
    let offset = usize::try_from(U256::from_be_bytes(offset_bytes)).ok()?;

    let tuple_start = SELECTOR_LEN.checked_add(offset)?;
    let proposal_id_start = tuple_start.checked_add(PROPOSAL_ID_BYTE_OFFSET)?;
    let proposal_id_end = proposal_id_start.checked_add(WORD_SIZE)?;
    if proposal_id_end > input.len() {
        return None;
    }

    let mut proposal_id_bytes = [0u8; WORD_SIZE];
    proposal_id_bytes.copy_from_slice(&input[proposal_id_start..proposal_id_end]);
    Some(U256::from_be_bytes(proposal_id_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_anchor_v4_proposal_id_real_payload() {
        let calldata = hex_decode(concat!(
            "0x",
            "3c7aa91100000000000000000000000000000000000000000000000000000000",
            "000000a000000000000000000000000000000000000000000000000000000000",
            "00000005aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "aaaaaaaabbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "bbbbbbbbcccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "cccccccc00000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000a0000000000000000000000003c44cdddb6a900fa2b585dd299e03d12",
            "fa4293bc00000000000000000000000000000000000000000000000000000000",
            "000000c011111111111111111111111111111111111111111111111111111111",
            "1111111100000000000000000000000000000000000000000000000000000000",
            "0000010000000000000000000000000000000000000000000000000000000000",
            "00000004deadbeef000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000100000000000000000000000000000000000000000000000000000000",
            "0000000500000000000000000000000000000000000000000000000000000000",
            "0000000100000000000000000000000000000000000000000000000000000000",
            "0000000100000000000000000000000000000000000000000000000000000000",
            "00000002"
        ));
        assert_eq!(extract_anchor_v4_proposal_id(&calldata), Some(U256::from(10u64)));
    }

    #[test]
    fn returns_none_for_truncated_calldata() {
        assert!(extract_anchor_v4_proposal_id(&[0u8; 10]).is_none());
    }

    fn hex_decode(value: &str) -> Vec<u8> {
        let value = value.strip_prefix("0x").unwrap_or(value);
        let digits: String = value.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(digits.len() % 2 == 0, "hex value must have an even length (got {})", digits.len());
        digits
            .as_bytes()
            .chunks(2)
            .map(|chunk| {
                let hi = (chunk[0] as char).to_digit(16).expect("invalid hex") as u8;
                let lo = (chunk[1] as char).to_digit(16).expect("invalid hex") as u8;
                (hi << 4) | lo
            })
            .collect()
    }
}
