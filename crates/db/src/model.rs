use core::fmt;

use alloy_primitives::BlockNumber;
use reth_db_api::{TableSet, TableType, TableViewer, table::TableInfo, tables};
use reth_revm::primitives::{B256, U256};
use serde::{Deserialize, Serialize};
use serde_with::{Bytes, serde_as};

use alethia_reth_primitives::payload::attributes::RpcL1Origin;

/// The key for the stored L1 head origin in the database.
pub const STORED_L1_HEAD_ORIGIN_KEY: u64 = 0;

/// Represents the L1 origin for a L2 block in Taiko network, which is saved in the database.
#[serde_as]
#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct StoredL1Origin {
    /// The number of the L2 block for which this L1 origin is created.
    pub block_id: U256,
    /// The hash of the L2 block.
    pub l2_block_hash: B256,
    /// The height of the L1 block that included the L2 block.
    pub l1_block_height: U256,
    /// The hash of the L1 block that included the L2 block.
    pub l1_block_hash: B256,
    /// The ID of the build payload arguments.
    pub build_payload_args_id: [u8; 8],
    /// Indicates if the L2 block was included as a forced inclusion.
    pub is_forced_inclusion: bool,
    /// The signature of the L2 block payload.
    #[serde_as(as = "Bytes")]
    pub signature: [u8; 65],
}

impl From<RpcL1Origin> for StoredL1Origin {
    // Converts an `RpcL1Origin` into a `StoredL1Origin`.
    fn from(rpc_l1_origin: RpcL1Origin) -> Self {
        StoredL1Origin {
            block_id: rpc_l1_origin.block_id,
            l2_block_hash: rpc_l1_origin.l2_block_hash,
            l1_block_height: rpc_l1_origin.l1_block_height.unwrap_or(U256::ZERO),
            l1_block_hash: rpc_l1_origin.l1_block_hash.unwrap_or(B256::ZERO),
            build_payload_args_id: rpc_l1_origin.build_payload_args_id,
            is_forced_inclusion: rpc_l1_origin.is_forced_inclusion,
            signature: rpc_l1_origin.signature,
        }
    }
}

impl From<&RpcL1Origin> for StoredL1Origin {
    // Converts a borrowed `RpcL1Origin` into a `StoredL1Origin` without cloning.
    fn from(rpc_l1_origin: &RpcL1Origin) -> Self {
        StoredL1Origin {
            block_id: rpc_l1_origin.block_id,
            l2_block_hash: rpc_l1_origin.l2_block_hash,
            l1_block_height: rpc_l1_origin.l1_block_height.unwrap_or(U256::ZERO),
            l1_block_hash: rpc_l1_origin.l1_block_hash.unwrap_or(B256::ZERO),
            build_payload_args_id: rpc_l1_origin.build_payload_args_id,
            is_forced_inclusion: rpc_l1_origin.is_forced_inclusion,
            signature: rpc_l1_origin.signature,
        }
    }
}

impl StoredL1Origin {
    /// Converts the stored representation back into its RPC form.
    pub fn into_rpc(self) -> RpcL1Origin {
        RpcL1Origin {
            block_id: self.block_id,
            l2_block_hash: self.l2_block_hash,
            l1_block_height: (self.l1_block_height != U256::ZERO).then_some(self.l1_block_height),
            l1_block_hash: (self.l1_block_hash != B256::ZERO).then_some(self.l1_block_hash),
            build_payload_args_id: self.build_payload_args_id,
            is_forced_inclusion: self.is_forced_inclusion,
            signature: self.signature,
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

  table BatchToLastBlock {
    type Key = BlockNumber;
    type Value = BlockNumber;
  }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_stored_l1_origin_from() {
        let l1_block_height = U256::random();
        let l1_block_hash = B256::from([1u8; 32]);

        let rpc_l1_origin = RpcL1Origin {
            block_id: U256::random(),
            l2_block_hash: B256::random(),
            l1_block_height: Some(l1_block_height),
            l1_block_hash: Some(l1_block_hash),
            build_payload_args_id: [2u8; 8],
            is_forced_inclusion: true,
            signature: [3u8; 65],
        };

        let stored_l1_origin: StoredL1Origin = rpc_l1_origin.clone().into();
        assert_eq!(stored_l1_origin.block_id, rpc_l1_origin.block_id);
        assert_eq!(stored_l1_origin.l2_block_hash, rpc_l1_origin.l2_block_hash);
        assert_eq!(stored_l1_origin.l1_block_height, l1_block_height);
        assert_eq!(stored_l1_origin.l1_block_hash, l1_block_hash);
        assert_eq!(stored_l1_origin.build_payload_args_id, rpc_l1_origin.build_payload_args_id);
        assert_eq!(stored_l1_origin.is_forced_inclusion, rpc_l1_origin.is_forced_inclusion);
        assert_eq!(stored_l1_origin.signature, rpc_l1_origin.signature);
    }
}
