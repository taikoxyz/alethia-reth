#[cfg(feature = "net")]
pub mod engine;
pub mod extra_data;
#[cfg(feature = "net")]
pub mod payload;

pub use extra_data::{
    SHASTA_EXTRA_DATA_LEN, decode_shasta_basefee_sharing_pctg, decode_shasta_proposal_id,
};

#[cfg(not(feature = "net"))]
/// ```compile_fail
/// use alethia_reth_primitives::payload;
/// ```
///
/// The payload module must be unavailable when the `net` feature is disabled.
mod _net_feature_gates {}
