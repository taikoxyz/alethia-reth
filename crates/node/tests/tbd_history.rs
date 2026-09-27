//! Synthetic pre-TBD differential fixtures; these do not replay public-chain history.

#[allow(dead_code)]
mod support;

use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_consensus::validation::{
    ANCHOR_V1_SELECTOR, ANCHOR_V2_SELECTOR, ANCHOR_V3_SELECTOR, ANCHOR_V4_SELECTOR,
};
use alethia_reth_primitives::addresses::{TAIKO_GOLDEN_TOUCH_ADDRESS, get_treasury_address};
use alloy_consensus::{SignableTransaction, Signed, TxEip1559};
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use reth_chainspec::{ChainSpec, EthChainSpec};
use reth_storage_api::{ReceiptProvider, StateProvider, StateProviderFactory};
use reth_tasks::Runtime;
use std::sync::Arc;
use support::*;

/// Builds each legacy fork profile with an unfunded anchor and a funded ordinary signer.
fn history_spec(stage: usize) -> Arc<TaikoChainSpec> {
    let profile = historical_chain_spec(stage);
    let genesis: Genesis =
        serde_json::from_str(include_str!("fixtures/historical-genesis.json")).unwrap();
    let mut inner = ChainSpec::builder()
        .chain(genesis.config.chain_id.into())
        .genesis(genesis)
        .with_forks(profile.inner.hardforks.clone())
        .build();
    inner.paris_block_and_final_difficulty = Some((0, U256::ZERO));
    Arc::new(TaikoChainSpec { inner })
}

/// Compares commitments and fee/refund behavior against a separately executed release baseline.
async fn historical_snapshot(stage: usize, name: &str) -> eyre::Result<()> {
    let signer: PrivateKeySigner =
        "0x92954368afd3caa1f3ce3ead0069c1af414054aefe1ef9aeacc1bf426222ce38".parse()?;
    let ordinary = PrivateKeySigner::from_bytes(&B256::with_last_byte(1))?;
    let golden = Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS);
    let treasury = get_treasury_address(167001);
    let refund_target = Address::with_last_byte(0x23);
    let selector = [
        ANCHOR_V1_SELECTOR,
        ANCHOR_V2_SELECTOR,
        ANCHOR_V3_SELECTOR,
        ANCHOR_V4_SELECTOR,
        ANCHOR_V4_SELECTOR,
    ][stage];
    let share = [0_u8, 17, 31, 43, 59][stage];
    let spec = history_spec(stage);
    let genesis = spec.genesis_hash();
    let node = launch_test_node(spec.clone(), Runtime::test()).await?;
    let client = node.auth_server_handle().http_client();
    fcu(&client, 2, genesis, genesis, None).await?;
    let anchor = TxEip1559 {
        chain_id: 167001,
        nonce: 7,
        gas_limit: if stage < 2 { 250_000 } else { 1_000_000 },
        max_fee_per_gas: 1_000_000_000,
        to: treasury.into(),
        input: Bytes::copy_from_slice(selector),
        ..Default::default()
    };
    let anchor =
        Signed::new_unhashed(anchor.clone(), signer.sign_hash_sync(&anchor.signature_hash())?)
            .into();
    let user = TxEip1559 {
        chain_id: 167001,
        gas_limit: 100_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 7,
        to: refund_target.into(),
        ..Default::default()
    };
    let user =
        Signed::new_unhashed(user.clone(), ordinary.sign_hash_sync(&user.signature_hash())?).into();
    let transactions = [anchor, user];
    let mut attrs = with_txs(fixture_attributes(99), &transactions);
    attrs.block_metadata.extra_data = if stage < 3 {
        let mut bytes = [0; 32];
        bytes[31] = share;
        bytes.to_vec().into()
    } else {
        vec![share, 0, 0, 0, 0, 0, 42].into()
    };
    let built = build(&client, spec.clone(), spec.genesis_header(), 0, attrs).await?;
    assert_eq!(built.block.body().transactions, transactions);
    canonicalize(&client, genesis, &built).await?;
    let receipts = node.inner.provider.receipts_by_block(built.block.hash().into())?.unwrap();
    assert_eq!(receipts.len(), 2);
    assert!(receipts.iter().all(|receipt| receipt.success));
    // SSTORE clearing a cold nonzero slot: 21,000 intrinsic + 5,000 + 6 PUSH gas - 4,800 refund.
    let ordinary_gas = receipts[1].cumulative_gas_used - receipts[0].cumulative_gas_used;
    assert_eq!(ordinary_gas, 21_206, "ordinary gas must include the storage-clear refund");
    let state = node.inner.provider.latest()?;
    let anchor_account = state.basic_account(&golden)?.unwrap();
    let user_account = state.basic_account(&ordinary.address())?.unwrap();
    assert_eq!(anchor_account.balance, U256::ZERO, "anchor must neither pay nor mint refunded gas");
    assert_eq!(anchor_account.nonce, 8);
    assert_eq!(user_account.nonce, 1);
    assert_eq!(state.storage(refund_target, B256::ZERO)?.unwrap_or_default(), U256::ZERO);
    let base_fee = U256::from(built.block.base_fee_per_gas.unwrap());
    let ordinary_fee = U256::from(ordinary_gas) * base_fee;
    let beneficiary_share = ordinary_fee * U256::from(share) / U256::from(100);
    let tip = U256::from(ordinary_gas * 7);
    let treasury_balance = state.account_balance(&treasury)?.unwrap_or_default();
    assert_eq!(treasury_balance, ordinary_fee - beneficiary_share);
    assert_eq!(
        state.account_balance(&built.block.beneficiary)?.unwrap_or_default(),
        beneficiary_share + tip
    );
    assert_eq!(
        user_account.balance,
        U256::from(1_000_000_000_000_000_000_000_u128) - ordinary_fee - tip
    );
    let mut captured = vector(&node, &built, spec.genesis_header())?;
    captured["profile"] = serde_json::json!({
        "name": name, "genesisFixture": "historical-genesis.json", "baseFeeSharePctg": share
    });
    captured["expected"]["senderNonce"] = anchor_account.nonce.into();
    captured["expected"]["ordinarySender"] = serde_json::to_value(ordinary.address())?;
    captured["expected"]["ordinarySenderNonce"] = user_account.nonce.into();
    captured["expected"]["ordinarySenderBalance"] = serde_json::to_value(user_account.balance)?;
    captured["expected"]["treasuryBalance"] = serde_json::to_value(treasury_balance)?;
    captured["expected"]["refundTargetSlotZero"] = serde_json::json!("0x0");
    verify_vector(&format!("historical-{name}"), captured)?;
    Ok(())
}

#[test]
fn historical_v2_genesis_matches_independent_pre_tbd_reference() -> eyre::Result<()> {
    run_live_test(historical_snapshot(0, "genesis"))
}

#[test]
fn historical_v2_ontake_matches_independent_pre_tbd_reference() -> eyre::Result<()> {
    run_live_test(historical_snapshot(1, "ontake"))
}

#[test]
fn historical_v2_pacaya_matches_independent_pre_tbd_reference() -> eyre::Result<()> {
    run_live_test(historical_snapshot(2, "pacaya"))
}

#[test]
fn historical_v2_shasta_matches_independent_pre_tbd_reference() -> eyre::Result<()> {
    run_live_test(historical_snapshot(3, "shasta"))
}

#[test]
fn historical_v2_unzen_matches_independent_pre_tbd_reference() -> eyre::Result<()> {
    run_live_test(historical_snapshot(4, "unzen"))
}
