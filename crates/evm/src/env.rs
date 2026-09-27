//! Block environments carrying Taiko fee context across EVM recreation and inspection.
use std::ops::{Deref, DerefMut};

use alloy_evm::{EvmEnv, env::BlockEnvironment};
use alloy_primitives::{Address, B256, U256};
use reth_revm::{
    context::{Block, BlockEnv},
    context_interface::block::BlobExcessGasAndPrice,
};

use crate::spec::TaikoSpecId;

/// EVM environment retaining authoritative Taiko block fee data during replay.
pub type TaikoEvmEnv = EvmEnv<TaikoSpecId, TaikoBlockEnv>;

/// Standard block fields plus the fee percentage supplied by an authoritative TBD block.
#[derive(Default, Clone, Debug)]
pub struct TaikoBlockEnv {
    /// Standard revm block environment used for opcode execution and validation.
    pub inner: BlockEnv,
    /// Percentage of base fees paid to the beneficiary; `None` means no header context.
    /// An explicit zero remains authoritative and routes all base fees to the treasury.
    pub base_fee_share_pctg: Option<u64>,
}

impl TaikoBlockEnv {
    /// Records an authoritative beneficiary share in percentage points, including zero.
    pub fn with_base_fee_share_pctg(mut self, percentage: u64) -> Self {
        self.base_fee_share_pctg = Some(percentage);
        self
    }
}

impl From<BlockEnv> for TaikoBlockEnv {
    /// Wraps raw block fields without claiming an authoritative fee percentage.
    fn from(inner: BlockEnv) -> Self {
        Self { inner, base_fee_share_pctg: None }
    }
}

impl Deref for TaikoBlockEnv {
    /// Standard block fields exposed for existing environment consumers.
    type Target = BlockEnv;

    /// Borrows the standard block fields.
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for TaikoBlockEnv {
    /// Mutates standard block fields while retaining the Taiko fee context.
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl BlockEnvironment for TaikoBlockEnv {
    /// Exposes standard fields for Alloy overrides without discarding fee context.
    fn inner_mut(&mut self) -> &mut BlockEnv {
        &mut self.inner
    }
}

impl Block for TaikoBlockEnv {
    /// Returns the block height.
    fn number(&self) -> U256 {
        self.inner.number()
    }

    /// Returns the recipient of transaction priority fees and the configured base-fee share.
    fn beneficiary(&self) -> Address {
        self.inner.beneficiary()
    }

    /// Returns the block timestamp in seconds since the Unix epoch.
    fn timestamp(&self) -> U256 {
        self.inner.timestamp()
    }

    /// Returns the block gas limit.
    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    /// Returns the base fee in wei per gas.
    fn basefee(&self) -> u64 {
        self.inner.basefee()
    }

    /// Returns the header difficulty supplied by the execution environment.
    fn difficulty(&self) -> U256 {
        self.inner.difficulty()
    }

    /// Returns the randomness exposed by the PREVRANDAO opcode.
    fn prevrandao(&self) -> Option<B256> {
        self.inner.prevrandao()
    }

    /// Returns the excess blob gas and its price when Cancun semantics are active.
    fn blob_excess_gas_and_price(&self) -> Option<BlobExcessGasAndPrice> {
        self.inner.blob_excess_gas_and_price()
    }

    /// Returns the slot number supplied by the standard environment.
    fn slot_num(&self) -> u64 {
        self.inner.slot_num()
    }
}
