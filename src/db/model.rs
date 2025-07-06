use core::fmt;

use alloy_rlp::BufMut;
use parity_scale_codec::Encode;
use reth::revm::primitives::{
    B256, U256,
    alloy_primitives::{self},
};
use reth_codecs::Compact;
use reth_db_api::{TableSet, TableType, TableViewer, table::TableInfo, tables};
use serde::{Deserialize, Serialize};

pub const STORED_L1_HEAD_ORIGIN_KEY: u64 = 0;

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
    type Key = u64;
    type Value = StoredL1Origin;
  }

  table StoredL1HeadOriginTable {
    type Key = u64;
    type Value = u64;
  }
}

impl Compact for StoredL1Origin {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let mut len = 0;

        len += self.block_id.to_compact(buf);

        buf.put_slice(self.l2_block_hash.as_slice());
        len += 32;

        if let Some(height) = self.l1_block_height {
            buf.put_u8(1);
            len += 1;
            len += height.to_compact(buf);
        } else {
            buf.put_u8(0);
            len += 1;
        }

        if let Some(hash) = self.l1_block_hash {
            buf.put_u8(1);
            buf.put_slice(hash.as_slice());
            len += 33;
        } else {
            buf.put_u8(0);
            len += 1;
        }

        buf.put_slice(&self.build_payload_args_id);
        len += 8;

        len
    }

    fn from_compact(buf: &[u8], _length: usize) -> (Self, &[u8]) {
        let mut buf = buf;

        let (block_id, rest) = U256::from_compact(buf, buf.len());
        buf = rest;

        let l2_block_hash = B256::from_slice(&buf[..32]);
        buf = &buf[32..];

        let (l1_block_height, rest) = match buf[0] {
            1 => {
                let (height, rest) = U256::from_compact(&buf[1..], buf.len() - 1);
                (Some(height), rest)
            }
            _ => (None, &buf[1..]),
        };
        buf = rest;

        let (l1_block_hash, rest) = match buf[0] {
            1 => {
                let hash = B256::from_slice(&buf[1..33]);
                (Some(hash), &buf[33..])
            }
            _ => (None, &buf[1..]),
        };
        buf = rest;

        let build_payload_args_id = buf[..8].try_into().expect("fixed size array");
        buf = &buf[8..];

        (
            Self {
                block_id,
                l2_block_hash,
                l1_block_height,
                l1_block_hash,
                build_payload_args_id,
            },
            buf,
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
