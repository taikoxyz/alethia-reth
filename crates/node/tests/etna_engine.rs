mod support;
use alloy_primitives::B256;
use reth_chainspec::EthChainSpec;
use support::*;

#[test]
fn cross_fork_lifecycle_checks_inputs_traces_and_rolls_back_system_storage() -> eyre::Result<()> {
    run_live_test(async {
        use alethia_reth_primitives::engine::TaikoEngineTypes;
        use alloy_primitives::{Address, Bytes, U256};
        use alloy_rpc_types_engine::{ExecutionPayloadEnvelopeV5, PayloadStatus};
        use jsonrpsee::{core::client::ClientT, rpc_params};
        use reth_node_api::PayloadTypes;
        use reth_storage_api::{ReceiptProvider, StateProvider, StateProviderFactory};
        let spec = fixture_chain_spec();
        let genesis = spec.genesis_hash();
        let a = launch_test_node(spec.clone()).await?;
        let b = launch_test_node(spec.clone()).await?;
        let ca = a.auth_server_handle().http_client();
        let cb = b.auth_server_handle().http_client();
        for client in [&ca, &cb] {
            fcu(client, genesis, genesis, None).await?;
        }
        let anchor = signed_tx(
            0,
            true,
            Address::with_last_byte(0x21),
            Bytes::copy_from_slice(alethia_reth_consensus::validation::ANCHOR_V4_SELECTOR),
        );
        let legacy = build(
            &ca,
            spec.clone(),
            spec.genesis_header(),
            with_txs(fixture_attributes(99), &[anchor]),
        )
        .await?;
        canonicalize(&ca, genesis, &legacy).await?;
        // B first sees `legacy` on the direct route: a rejected legacy Osaka sidecar must not
        // poison the honest hash while reth converts and executes concurrently.
        let data = TaikoEngineTypes::block_to_payload(legacy.block.clone(), None);
        let mut tampered = data.clone();
        tampered.taiko_sidecar.osaka.as_mut().unwrap().parent_beacon_block_root =
            B256::with_last_byte(7);
        let rejected: PayloadStatus =
            cb.request("reth_newPayload", rpc_params![tampered, false, false]).await?;
        assert!(rejected.status.is_invalid(), "{rejected:?}");
        let recovered: PayloadStatus =
            cb.request("reth_newPayload", rpc_params![&data, false, false]).await?;
        assert!(recovered.status.is_valid(), "honest hash was poisoned: {recovered:?}");
        canonicalize(&cb, genesis, &legacy).await?;
        let http = a.rpc_server_handle().http_client().unwrap();
        // A pending Etna child of the seven-byte legacy parent has no L1 root: null, not an error.
        assert_eq!(
            http.request::<serde_json::Value, _>(
                "eth_getBlockByNumber",
                rpc_params!["pending", true]
            )
            .await?,
            serde_json::Value::Null
        );
        let activation = build(
            &cb,
            spec.clone(),
            legacy.block.header(),
            with_txs(
                fixture_attributes(100),
                &[signed_tx(1, false, Address::with_last_byte(0x21), Bytes::new())],
            ),
        )
        .await?;
        // The anchor bytes are copied from the FCU attributes; consensus checks only the length.
        assert_eq!(activation.block.extra_data, activation.attrs.block_metadata.extra_data);
        // A has not seen `activation`: forged commitments are INVALID statuses, not RPC errors.
        let v4 = |payload: serde_json::Value| {
            ca.request::<PayloadStatus, _>(
                "engine_newPayloadV4",
                rpc_params![payload, Vec::<B256>::new(), activation.root, Vec::<Bytes>::new()],
            )
        };
        let mut wrong = activation.block.clone().into_block();
        wrong.header.difficulty += U256::from(1);
        let mut payload = activation.payload.clone();
        payload["headerDifficulty"] = u64::try_from(wrong.header.difficulty)?.into();
        payload["blockHash"] = serde_json::to_value(wrong.header.hash_slow())?;
        let status = v4(payload).await?;
        assert!(status.status.is_invalid(), "{status:?}");
        assert!(format!("{status:?}").contains("zk gas header difficulty mismatch"), "{status:?}");
        let mut short = activation.block.clone().into_block();
        short.header.extra_data = Bytes::from_static(&[0; 7]);
        let mut payload = activation.payload.clone();
        payload["extraData"] = serde_json::to_value(&short.header.extra_data)?;
        payload["blockHash"] = serde_json::to_value(short.header.hash_slow())?;
        let status = v4(payload).await?;
        assert!(status.status.is_invalid(), "{status:?}");
        assert!(format!("{status:?}").contains("extraData"), "{status:?}");
        for client in [&ca, &cb] {
            canonicalize(client, genesis, &activation).await?;
        }
        assert_execution_parity(&b, &activation).await?;
        // Replay tracing charges the index-0 golden-touch transaction ordinary fees.
        let traces: serde_json::Value = http
            .request(
                "debug_traceBlockByHash",
                rpc_params![
                    activation.block.hash(),
                    serde_json::json!({"tracer":"prestateTracer", "tracerConfig":{"diffMode":true}})
                ],
            )
            .await?;
        let receipts = a.provider.receipts_by_block(activation.block.hash().into())?.unwrap();
        let gas = U256::from(receipts[0].cumulative_gas_used);
        let result = &traces[0]["result"];
        let balance = |section: &str, address: Address| -> U256 {
            serde_json::from_value(
                result[section][address.to_string().to_lowercase()]["balance"].clone(),
            )
            .unwrap_or_default()
        };
        let sender = Address::from(alethia_reth_primitives::addresses::TAIKO_GOLDEN_TOUCH_ADDRESS);
        let beneficiary = activation.block.beneficiary;
        assert_eq!(
            balance("pre", sender) - balance("post", sender),
            gas * U256::from(activation.block.base_fee_per_gas.unwrap() + 7)
        );
        assert_eq!(balance("post", beneficiary) - balance("pre", beneficiary), gas * U256::from(7));
        // A's last resolved Unzen job stays retrievable while B builds intervening blocks.
        let old: ExecutionPayloadEnvelopeV5 =
            ca.request("engine_getPayloadV5", rpc_params![legacy.id]).await?;
        assert_eq!(normalize_v5(old), legacy.payload);
        // A builds the alternative legacy branch. B's retained job remains the Etna child.
        fcu(&ca, genesis, genesis, None).await?;
        let alternative =
            build(&ca, spec.clone(), spec.genesis_header(), fixture_attributes(98)).await?;
        for client in [&ca, &cb] {
            canonicalize(client, genesis, &alternative).await?;
        }
        for node in [&a, &b] {
            let state = node.provider.latest()?;
            assert_eq!(
                state
                    .storage(
                        alloy_eips::eip4788::BEACON_ROOTS_ADDRESS,
                        B256::from(U256::from(8291))
                    )?
                    .unwrap_or_default(),
                U256::ZERO
            );
            assert_eq!(
                state
                    .storage(
                        alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
                        B256::from(U256::from(1))
                    )?
                    .unwrap_or_default(),
                U256::ZERO
            );
            assert_eq!(
                state.storage(alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS, B256::ZERO)?,
                Some(U256::from_be_bytes(genesis.0))
            );
        }
        let old: ExecutionPayloadEnvelopeV5 =
            cb.request("engine_getPayloadV5", rpc_params![activation.id]).await?;
        assert_eq!(normalize_v5(old), activation.payload);
        let forward =
            build(&ca, spec.clone(), alternative.block.header(), fixture_attributes(101)).await?;
        for client in [&ca, &cb] {
            canonicalize(client, genesis, &forward).await?;
        }
        let state = b.provider.latest()?;
        assert_eq!(
            state
                .storage(alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS, B256::from(U256::from(1)))?,
            Some(U256::from_be_bytes(alternative.block.hash().0))
        );
        assert_eq!(
            state
                .storage(alloy_eips::eip4788::BEACON_ROOTS_ADDRESS, B256::from(U256::from(8292)))?,
            Some(U256::from(1))
        );
        assert_execution_parity(&b, &forward).await?;
        Ok(())
    })
}

#[test]
fn devnet_etna_genesis_serves_preselection_and_pending_simulation() -> eyre::Result<()> {
    run_live_test(async {
        use alethia_reth_chainspec::{TAIKO_DEVNET, spec::TaikoDevnetConfigExt};
        use jsonrpsee::{core::client::ClientT, rpc_params};
        use serde_json::{Value, json};
        let spec = std::sync::Arc::new(
            TAIKO_DEVNET.clone_with_devnet_fork_timestamps(0, Some(0))?.unwrap(),
        );
        let node = launch_test_node(spec.clone()).await?;
        let client = node.auth_server_handle().http_client();
        let status = fcu(&client, spec.genesis_hash(), spec.genesis_hash(), None).await?;
        assert!(status.payload_status.status.is_valid(), "{status:?}");
        // Preselection over an Etna genesis needs no beacon root: it never runs system calls.
        let lists: Value = client
            .request(
                "taikoAuth_txPoolContent",
                rpc_params![
                    alloy_primitives::Address::with_last_byte(0x42),
                    1u64,
                    30_000_000u64,
                    120_000u64,
                    Option::<Vec<alloy_primitives::Address>>::None,
                    1u64
                ],
            )
            .await?;
        assert!(lists.is_array(), "{lists}");
        let http = node.rpc_server_handle().http_client().unwrap();
        let tx = json!({"to": "0x0000000000000000000000000000000000000021"});
        assert_eq!(
            http.request::<Value, _>("eth_call", rpc_params![&tx, "pending"]).await?,
            json!("0x")
        );
        assert_eq!(
            http.request::<Value, _>("eth_estimateGas", rpc_params![&tx, "pending"]).await?,
            json!("0x5208")
        );
        // No authoritative L1 root exists for a local pending block, so it stays null.
        assert_eq!(
            http.request::<Value, _>("eth_getBlockByNumber", rpc_params!["pending", true]).await?,
            Value::Null
        );
        Ok(())
    })
}
