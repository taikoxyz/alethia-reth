use core::fmt;

use alloy_primitives::BlockNumber;
use alloy_rlp::{Buf, BufMut};
use reth::revm::primitives::{
    B256, U256,
    alloy_primitives::{self},
};
use reth_codecs::Compact;
use reth_db_api::{TableSet, TableType, TableViewer, table::TableInfo, tables};
use serde::{Deserialize, Serialize};
use serde_with::{Bytes, serde_as};

use crate::payload::attributes::L1Origin;

pub const STORED_L1_HEAD_ORIGIN_KEY: u64 = 0;

/// Represents the L1 origin for a L2 block in Taiko network.
#[serde_as]
#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct StoredL1Origin {
    pub block_id: U256,
    pub l2_block_hash: B256,
    pub l1_block_height: U256,
    pub l1_block_hash: B256,
    pub build_payload_args_id: [u8; 8],
    pub is_forced_inclusion: bool,
    #[serde_as(as = "Bytes")]
    pub signature: [u8; 65],
}

impl From<L1Origin> for StoredL1Origin {
    fn from(l1_origin: L1Origin) -> Self {
        StoredL1Origin {
            block_id: l1_origin.block_id,
            l2_block_hash: l1_origin.l2_block_hash,
            l1_block_height: l1_origin.l1_block_height.unwrap_or(U256::ZERO),
            l1_block_hash: l1_origin.l1_block_hash.unwrap_or(B256::ZERO),
            build_payload_args_id: l1_origin.build_payload_args_id,
            is_forced_inclusion: l1_origin.is_forced_inclusion,
            signature: l1_origin.signature,
        }
    }
}

tables! {
  table StoredL1OriginTable {
    type Key = BlockNumber;
    type Value = StoredL1Origin;
  }

  table StoredL1HeadOriginTable {
    type Key = BlockNumber;
    type Value = BlockNumber;
  }
}

impl Compact for StoredL1Origin {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let start_len = buf.remaining_mut();

        buf.put_slice(self.block_id.to_be_bytes::<32>().as_slice());
        buf.put_slice(self.l2_block_hash.as_slice());
        buf.put_slice(self.l1_block_height.to_be_bytes::<32>().as_slice());
        buf.put_slice(self.l1_block_hash.as_slice());
        buf.put_slice(&self.build_payload_args_id);
        buf.put_u8(self.is_forced_inclusion as u8);
        buf.put_slice(&self.signature);

        start_len - buf.remaining_mut()
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let mut cursor = std::io::Cursor::new(&buf[..len]);

        let mut block_id_bytes = [0u8; 32];
        cursor.copy_to_slice(&mut block_id_bytes);
        let block_id = U256::from_be_bytes(block_id_bytes);

        let mut l2_block_hash_bytes = [0u8; 32];
        cursor.copy_to_slice(&mut l2_block_hash_bytes);
        let l2_block_hash = B256::from(l2_block_hash_bytes);

        let mut l1_block_height_bytes = [0u8; 32];
        cursor.copy_to_slice(&mut l1_block_height_bytes);
        let l1_block_height = U256::from_be_bytes(l1_block_height_bytes);

        let mut l1_block_hash_bytes = [0u8; 32];
        cursor.copy_to_slice(&mut l1_block_hash_bytes);
        let l1_block_hash = B256::from(l1_block_hash_bytes);

        let mut build_payload_args_id = [0u8; 8];
        cursor.copy_to_slice(&mut build_payload_args_id);

        let is_forced_inclusion = cursor.get_u8() != 0;

        let mut signature = [0u8; 65];
        cursor.copy_to_slice(&mut signature);

        let obj = StoredL1Origin {
            block_id,
            l2_block_hash,
            l1_block_height,
            l1_block_hash,
            build_payload_args_id,
            is_forced_inclusion,
            signature,
        };

        let remaining = &buf[cursor.position() as usize..];
        (obj, remaining)
    }
}

impl reth_db_api::table::Compress for StoredL1Origin {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: alloy_primitives::bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = Compact::to_compact(self, buf);
    }
}

impl reth_db_api::table::Decompress for StoredL1Origin {
    fn decompress(value: &[u8]) -> Result<Self, reth_db_api::DatabaseError> {
        let (obj, _) = Compact::from_compact(value, value.len());
        Ok(obj)
    }
}
