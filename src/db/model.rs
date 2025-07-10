use core::fmt;

use alloy_primitives::BlockNumber;
use alloy_rlp::BufMut;
use reth::revm::primitives::{
    B256, U256,
    alloy_primitives::{self},
};
use reth_codecs::Compact;
use reth_db_api::{TableSet, TableType, TableViewer, table::TableInfo, tables};
use serde::{Deserialize, Serialize};

pub const STORED_L1_HEAD_ORIGIN_KEY: u64 = 0;

/// Represents the L1 origin for a L2 block in Taiko network.
#[derive(Debug, Default, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct StoredL1Origin {
    pub block_id: U256,
    pub l2_block_hash: B256,
    pub l1_block_height: Option<U256>,
    pub l1_block_hash: Option<B256>,
    pub build_payload_args_id: [u8; 8],
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

// TODO: improve this implementation.
impl Compact for StoredL1Origin {
    /// Takes a buffer which can be written to. *Ideally*, it returns the length written to.
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let start_len = buf.remaining_mut();

        buf.put_slice(&self.block_id.to_be_bytes::<32>());
        buf.put_slice(self.l2_block_hash.as_slice());

        // Option<U256>
        match &self.l1_block_height {
            Some(val) => {
                buf.put_u8(1);
                buf.put_slice(&val.to_be_bytes::<32>());
            }
            None => {
                buf.put_u8(0);
            }
        }

        // Option<B256>
        match &self.l1_block_hash {
            Some(val) => {
                buf.put_u8(1);
                buf.put_slice(val.as_slice());
            }
            None => {
                buf.put_u8(0);
            }
        }

        buf.put_slice(&self.build_payload_args_id);

        start_len - buf.remaining_mut()
    }

    /// Takes a buffer which can be read from. Returns the object and `buf` with its internal cursor
    /// advanced (eg.`.advance(len)`).
    ///
    /// `len` can either be the `buf` remaining length, or the length of the compacted type.
    ///
    /// It will panic, if `len` is smaller than `buf.len()`.
    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let mut offset = 0;

        let block_id = U256::from_be_bytes::<32>(buf[offset..offset + 32].try_into().unwrap());
        offset += 32;

        let l2_block_hash = B256::from_slice(&buf[offset..offset + 32]);
        offset += 32;

        let l1_block_height = match buf[offset] {
            0 => {
                offset += 1;
                None
            }
            1 => {
                offset += 1;
                let v = U256::from_be_bytes::<32>(buf[offset..offset + 32].try_into().unwrap());
                offset += 32;
                Some(v)
            }
            _ => panic!("invalid prefix for l1_block_height"),
        };

        let l1_block_hash = match buf[offset] {
            0 => {
                offset += 1;
                None
            }
            1 => {
                offset += 1;
                let v = B256::from_slice(&buf[offset..offset + 32]);
                offset += 32;
                Some(v)
            }
            _ => panic!("invalid prefix for l1_block_hash"),
        };

        let build_payload_args_id: [u8; 8] = buf[offset..offset + 8].try_into().unwrap();
        offset += 8;

        let remaining = &buf[offset..];

        (
            StoredL1Origin {
                block_id,
                l2_block_hash,
                l1_block_height,
                l1_block_hash,
                build_payload_args_id,
            },
            remaining,
        )
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
