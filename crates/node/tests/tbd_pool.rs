//! Live payload-builder regression for the fork-dependent pool gas budget.

#[allow(dead_code)]
mod support;

use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_consensus::validation::ANCHOR_V4_SELECTOR;
use alethia_reth_primitives::addresses::get_treasury_address;
use alloy_consensus::{SignableTransaction, Signed, TxEip1559};
use alloy_eips::Encodable2718;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use reth_chainspec::{ChainSpec, EthChainSpec};
use reth_ethereum_primitives::TransactionSigned;
use reth_tasks::Runtime;
use std::sync::Arc;
use support::*;

#[test]
fn pool_build_uses_full_tbd_gas_limit_and_preserves_the_legacy_reserve() -> eyre::Result<()> {
    run_live_test(async {
        // Mutating taiko_payload to subtract 1M on TBD must exclude this 1M-limit transaction
        // from a 1.5M block, even though its actual EVM gas use is much smaller.
        for tbd in [false, true] {
            let genesis: Genesis =
                serde_json::from_str(include_str!("fixtures/historical-genesis.json"))?;
            let mut inner = ChainSpec::builder()
                .chain(genesis.config.chain_id.into())
                .genesis(genesis)
                .with_forks(fixture_chain_spec().inner.hardforks.clone())
                .build();
            inner.paris_block_and_final_difficulty = Some((0, U256::ZERO));
            let spec = Arc::new(TaikoChainSpec { inner });
            let genesis = spec.genesis_hash();
            let a = launch_test_node(spec.clone(), Runtime::test()).await?;
            let b = launch_test_node(spec.clone(), Runtime::test()).await?;
            let ca = a.auth_server_handle().http_client();
            let cb = b.auth_server_handle().http_client();
            for client in [&ca, &cb] {
                fcu(client, if tbd { 3 } else { 2 }, genesis, genesis, None).await?;
            }
            let signer = PrivateKeySigner::from_bytes(&B256::with_last_byte(1))?;
            let ordinary = TxEip1559 {
                chain_id: 167001,
                gas_limit: 1_000_000,
                max_fee_per_gas: 1_000_000_000,
                max_priority_fee_per_gas: 7,
                to: Address::with_last_byte(0x21).into(),
                ..Default::default()
            };
            let signature = signer.sign_hash_sync(&ordinary.signature_hash())?;
            let ordinary: TransactionSigned = Signed::new_unhashed(ordinary, signature).into();
            assert_eq!(
                a.rpc.inject_tx(Bytes::from(ordinary.encoded_2718())).await?,
                *ordinary.hash()
            );
            let mut attrs = fixture_attributes(if tbd { 100 } else { 99 });
            attrs.block_metadata.tx_list = None;
            attrs.block_metadata.gas_limit = 1_500_000;
            let anchor = signed_tx(
                7,
                true,
                get_treasury_address(167001),
                Bytes::copy_from_slice(ANCHOR_V4_SELECTOR),
            );
            if !tbd {
                attrs.anchor_transaction = Some(alloy_rlp::encode(&anchor).into());
            }
            let built = build(&ca, spec.clone(), spec.genesis_header(), 0, attrs).await?;
            assert_eq!(built.block.gas_limit, 1_500_000);
            assert!(built.attrs.block_metadata.tx_list.is_none());
            assert_eq!(
                built.block.body().transactions,
                if tbd { vec![ordinary] } else { vec![anchor] },
                "fork-dependent gas budget must be selected by the production payload builder"
            );
            // canonicalize includes the actual newPayload V4/V2 request and asserts VALID.
            canonicalize(&ca, genesis, &built).await?;
            canonicalize(&cb, genesis, &built).await?;
            assert_eq!(
                vector(&a, &built, spec.genesis_header())?,
                vector(&b, &built, spec.genesis_header())?
            );
        }
        Ok(())
    })
}
