//! Tells a failed Etna build whose parent can never be built on apart from other build failures.
//!
//! Reth's payload job reports every failed build as `missing payload`, so the build's own error
//! never reaches the Engine API. After an Etna build fails, the handler reads the parent's state
//! once instead. Reth opens a historical state lazily, so a parent whose state was pruned is found
//! out on that first read.

use alethia_reth_primitives::etna::{
    ENGINE_CANNOT_BUILD_ON_PARENT_CODE, ENGINE_CANNOT_BUILD_ON_PARENT_MESSAGE,
};
use alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS;
use alloy_primitives::B256;
use jsonrpsee_types::ErrorObjectOwned;
use reth_provider::{AccountReader, ProviderError, ProviderResult, StateProviderFactory};
use reth_rpc_engine_api::EngineApiError;

/// Returns whether `error` reports the parent's state as pruned, which no retry can fix.
///
/// Two shapes count, matched structurally:
/// - `StateAtBlockPruned`.
/// - `InsufficientChangesets` whose requested block lies below the available range. Reth's state
///   overlay reports a parent below the history prune boundary this way, and only a prune
///   checkpoint gives that range a nonzero start.
///
/// Every other error is not definitive, including `InsufficientChangesets` for a missing Finish
/// checkpoint (block 0 in `0..=0`) or for a block past the tip.
pub(crate) fn is_pruned_parent_state(error: &ProviderError) -> bool {
    match error {
        ProviderError::StateAtBlockPruned(_) => true,
        ProviderError::InsufficientChangesets { requested, available } => {
            requested < available.start()
        }
        _ => false,
    }
}

/// Opens the state of the block `parent_hash` and reads one fixed account from it.
///
/// Opening alone proves nothing, because reth checks a historical state's availability only on its
/// first read. The account is the EIP-2935 history contract; any fixed address would do.
pub(crate) fn read_parent_state(
    provider: &impl StateProviderFactory,
    parent_hash: B256,
) -> ProviderResult<()> {
    provider.state_by_block_hash(parent_hash)?.basic_account(&HISTORY_STORAGE_ADDRESS)?;
    Ok(())
}

/// Returns the JSON-RPC error for an Etna FCU whose payload job did not deliver a block.
///
/// Only a failed build (`GetPayloadError`) calls `read_parent_state`. When that read fails with a
/// pruned parent state, the answer is `-38100 "cannot build on parent"` with the read error's text
/// as diagnostic data. Otherwise it is the job's own error, unchanged.
pub(crate) fn failed_build_error(
    error: EngineApiError,
    read_parent_state: impl FnOnce() -> ProviderResult<()>,
) -> ErrorObjectOwned {
    if matches!(error, EngineApiError::GetPayloadError(_)) &&
        let Err(read_error) = read_parent_state() &&
        is_pruned_parent_state(&read_error)
    {
        return ErrorObjectOwned::owned(
            ENGINE_CANNOT_BUILD_ON_PARENT_CODE,
            ENGINE_CANNOT_BUILD_ON_PARENT_MESSAGE,
            Some(read_error.to_string()),
        );
    }
    error.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_node_api::PayloadBuilderError;

    #[test]
    fn only_pruned_parent_state_is_unbuildable() {
        assert!(is_pruned_parent_state(&ProviderError::StateAtBlockPruned(7)));
        assert!(is_pruned_parent_state(&ProviderError::InsufficientChangesets {
            requested: 2,
            available: 3..=5,
        }));
        for transient in [
            // At the prune boundary the parent's state is still there.
            ProviderError::InsufficientChangesets { requested: 3, available: 3..=5 },
            // No Finish checkpoint.
            ProviderError::InsufficientChangesets { requested: 0, available: 0..=0 },
            // Past the tip.
            ProviderError::InsufficientChangesets { requested: 6, available: 0..=5 },
            ProviderError::StateForHashNotFound(B256::ZERO),
            ProviderError::BlockHashNotFound(B256::ZERO),
            ProviderError::BestBlockNotFound,
            ProviderError::other(std::io::Error::other("timeout")),
        ] {
            assert!(!is_pruned_parent_state(&transient), "{transient:?}");
        }
    }

    #[test]
    fn failed_builds_answer_cannot_build_on_parent_only_for_pruned_parent_reads() {
        let failed = || EngineApiError::GetPayloadError(PayloadBuilderError::MissingPayload);
        for pruned in [
            ProviderError::StateAtBlockPruned(7),
            ProviderError::InsufficientChangesets { requested: 2, available: 3..=5 },
        ] {
            let text = pruned.to_string();
            let error = failed_build_error(failed(), || Err(pruned));
            assert_eq!(error.code(), -38_100);
            assert_eq!(error.message(), "cannot build on parent");
            let data = error.data().expect("the read error is attached as data").get();
            assert_eq!(data, serde_json::to_string(&text).expect("a string serializes"));
        }

        // A readable parent, or a read that fails for another reason, keeps the job's own error.
        let expected = ErrorObjectOwned::from(failed());
        for read in [
            Ok(()),
            Err(ProviderError::InsufficientChangesets { requested: 0, available: 0..=0 }),
            Err(ProviderError::StateForHashNotFound(B256::ZERO)),
        ] {
            let error = failed_build_error(failed(), || read);
            assert_eq!(error, expected);
        }

        // Errors other than a failed build never read the parent's state.
        let error = failed_build_error(EngineApiError::UnknownPayload, || {
            panic!("only a failed build reads the parent's state")
        });
        assert_eq!(error.code(), -38_001);
    }
}
