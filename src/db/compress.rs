use alloy_primitives::{B256, U256};
use alloy_rlp::{Buf, BufMut};
use reth_codecs::Compact;

use crate::db::model::StoredL1Origin;

impl Compact for StoredL1Origin {
    /// Takes a buffer which can be written to. *Ideally*, it returns the length written to.
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let len = buf.remaining_mut();

        buf.put_slice(self.block_id.to_be_bytes::<32>().as_slice());
        buf.put_slice(self.l2_block_hash.as_slice());
        buf.put_slice(self.l1_block_height.to_be_bytes::<32>().as_slice());
        buf.put_slice(self.l1_block_hash.as_slice());
        buf.put_slice(&self.build_payload_args_id);
        buf.put_u8(self.is_forced_inclusion as u8);
        buf.put_slice(&self.signature);

        len - buf.remaining_mut()
    }

    /// Takes a buffer which can be read from. Returns the object and `buf` with its internal cursor
    /// advanced (eg.`.advance(len)`).
    ///
    /// `len` can either be the `buf` remaining length, or the length of the compacted type.
    ///
    /// It will panic, if `len` is smaller than `buf.len()`.
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

        let stored = StoredL1Origin {
            block_id,
            l2_block_hash,
            l1_block_height,
            l1_block_hash,
            build_payload_args_id,
            is_forced_inclusion,
            signature,
        };

        let remaining = &buf[cursor.position() as usize..];
        (stored, remaining)
    }
}

impl reth_db_api::table::Compress for StoredL1Origin {
    type Compressed = Vec<u8>;

    /// Compresses data to a given buffer.
    fn compress_to_buf<B: alloy_primitives::bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = Compact::to_compact(self, buf);
    }
}

impl reth_db_api::table::Decompress for StoredL1Origin {
    /// Decompresses owned data coming from the database.
    fn decompress(value: &[u8]) -> Result<Self, reth_db_api::DatabaseError> {
        let (obj, _) = Compact::from_compact(value, value.len());
        Ok(obj)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloy_primitives::B256;
    use reth_db_api::table::{Compress, Decompress};

    #[test]
    fn test_stored_l1_origin_compact() {
        let stored = StoredL1Origin {
            block_id: U256::random(),
            l2_block_hash: B256::random(),
            l1_block_height: U256::random(),
            l1_block_hash: B256::random(),
            build_payload_args_id: [1u8; 8],
            is_forced_inclusion: true,
            signature: [1u8; 65],
        };

        let mut buf = Vec::new();
        let len = stored.to_compact(&mut buf);
        assert_eq!(len, buf.len());

        let (decompressed, remaining) = StoredL1Origin::from_compact(&buf, len);
        assert!(remaining.is_empty());
        assert_eq!(stored, decompressed);
    }

    #[test]
    fn test_stored_l1_origin_compress_decompress() {
        let stored = StoredL1Origin {
            block_id: U256::random(),
            l2_block_hash: B256::random(),
            l1_block_height: U256::random(),
            l1_block_hash: B256::random(),
            build_payload_args_id: [1u8; 8],
            is_forced_inclusion: true,
            signature: [1u8; 65],
        };

        let mut buf = Vec::new();
        stored.compress_to_buf(&mut buf);

        let decompressed = StoredL1Origin::decompress(&buf).unwrap();
        assert_eq!(stored, decompressed);
    }
}
