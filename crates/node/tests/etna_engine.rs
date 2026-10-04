mod support;
use alloy_primitives::B256;
use reth_chainspec::EthChainSpec;
use reth_tasks::Runtime;
use support::*;

#[test]
fn legacy_osaka_sidecar_rejection_does_not_poison_an_honest_hash() -> eyre::Result<()> {
    run_live_test(async {
        use alethia_reth_primitives::engine::TaikoEngineTypes;
        use alloy_primitives::U256;
        use alloy_rpc_types_engine::PayloadStatus;
        use jsonrpsee::{core::client::ClientT, rpc_params};
        use reth_node_api::PayloadTypes;
        use reth_storage_api::{StateProvider, StateProviderFactory};

        let spec = fixture_chain_spec();
        let genesis = spec.genesis_hash();
        let a = launch_test_node(spec.clone(), Runtime::test()).await?;
        let b = launch_test_node(spec.clone(), Runtime::test()).await?;
        let ca = a.auth_server_handle().http_client();
        let cb = b.auth_server_handle().http_client();
        for client in [&ca, &cb] {
            fcu(client, 2, genesis, genesis, None).await?;
        }
        let honest =
            build(&ca, spec.clone(), spec.genesis_header(), 0, fixture_attributes(99)).await?;
        let data = TaikoEngineTypes::block_to_payload(honest.block.clone(), None);
        let mut tampered = data.clone();
        tampered.taiko_sidecar.osaka.as_mut().unwrap().parent_beacon_block_root =
            B256::with_last_byte(7);
        let rejected: PayloadStatus =
            cb.request("reth_newPayload", rpc_params![tampered, false, false]).await?;
        assert!(rejected.status.is_invalid(), "{rejected:?}");
        // B has never imported this hash: a late state-root failure would poison its cache.
        let recovered: PayloadStatus =
            cb.request("reth_newPayload", rpc_params![&data, false, false]).await?;
        assert!(recovered.status.is_valid(), "honest hash was poisoned: {recovered:?}");
        assert!(import(&ca, &honest).await?.status.is_valid());
        assert!(import(&cb, &honest).await?.status.is_valid());
        for client in [&ca, &cb] {
            fcu(client, 2, genesis, honest.block.hash(), None).await?;
        }
        let state = b.inner.provider.latest()?;
        assert_eq!(
            state.storage(alloy_eips::eip4788::BEACON_ROOTS_ADDRESS, B256::from(U256::from(99)))?,
            Some(U256::from(99))
        );
        assert_eq!(
            state
                .storage(alloy_eips::eip4788::BEACON_ROOTS_ADDRESS, B256::from(U256::from(8290)))?
                .unwrap_or_default(),
            U256::ZERO
        );
        drop(state);
        // The same direct route must retain the nonzero root once Etna activates.
        let etna = build(&ca, spec, honest.block.header(), 0, fixture_attributes(100)).await?;
        let etna_data = TaikoEngineTypes::block_to_payload(etna.block.clone(), None);
        let status: PayloadStatus =
            cb.request("reth_newPayload", rpc_params![etna_data, false, false]).await?;
        assert!(status.status.is_valid(), "{status:?}");
        canonicalize(&cb, genesis, &etna).await?;
        Ok(())
    })
}

#[test]
fn live_two_node_build_import_state_and_empty_roundtrip() -> eyre::Result<()> {
    run_live_test(async {
        use alloy_primitives::{Address, Bytes, U256};
        use reth_storage_api::{BlockReader, ReceiptProvider, StateProvider, StateProviderFactory};
        let spec = fixture_chain_spec();
        let genesis = spec.genesis_hash();
        let a = launch_test_node(spec.clone(), Runtime::test()).await?;
        let b = launch_test_node(spec.clone(), Runtime::test()).await?;
        let ca = a.auth_server_handle().http_client();
        let cb = b.auth_server_handle().http_client();
        for client in [&ca, &cb] {
            assert!(fcu(client, 3, genesis, genesis, None).await?.payload_status.status.is_valid());
        }
        let tx = signed_tx(0, false, Address::with_last_byte(0x21), Bytes::new());
        let activation = build(
            &ca,
            spec.clone(),
            spec.genesis_header(),
            0,
            with_txs(fixture_attributes(100), &[tx]),
        )
        .await?;
        assert!(activation.block.difficulty > U256::ZERO);
        assert!(activation.payload["headerDifficulty"].is_number());
        for client in [&ca, &cb] {
            let status = import(client, &activation).await?;
            assert!(status.status.is_valid(), "{status:?}");
            assert!(
                fcu(client, 3, genesis, activation.block.hash(), None)
                    .await?
                    .payload_status
                    .status
                    .is_valid()
            );
        }
        let sa = a.inner.provider.latest()?;
        let sb = b.inner.provider.latest()?;
        for (address, slot, want) in [
            (alloy_eips::eip4788::BEACON_ROOTS_ADDRESS, 100, U256::from(100)),
            (alloy_eips::eip4788::BEACON_ROOTS_ADDRESS, 8291, U256::from(1)),
            (alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS, 0, U256::from_be_bytes(genesis.0)),
        ] {
            let slot = B256::from(U256::from(slot));
            assert_eq!(sa.storage(address, slot)?, Some(want));
            assert_eq!(sb.storage(address, slot)?, Some(want));
        }
        assert_eq!(
            a.inner.provider.receipts_by_block(activation.block.hash().into())?,
            b.inner.provider.receipts_by_block(activation.block.hash().into())?
        );
        let imported = b.inner.provider.block_by_hash(activation.block.hash())?.unwrap();
        assert_eq!(imported.header, activation.block.header().clone());
        let empty =
            build(&ca, spec.clone(), activation.block.header(), 0, fixture_attributes(101)).await?;
        assert_eq!(empty.block.difficulty, U256::ZERO);
        assert_eq!(empty.payload["headerDifficulty"], 0);
        assert!(empty.block.body().transactions.is_empty());
        assert!(import(&cb, &empty).await?.status.is_valid());
        assert!(
            fcu(&cb, 3, genesis, empty.block.hash(), None).await?.payload_status.status.is_valid()
        );
        Ok(())
    })
}

#[test]
fn cross_fork_reorg_retains_job_routing_and_rolls_back_system_storage() -> eyre::Result<()> {
    run_live_test(async {
        use alloy_primitives::{Address, Bytes, U256};
        use alloy_rpc_types_engine::{ExecutionPayloadEnvelopeV2, ExecutionPayloadEnvelopeV5};
        use jsonrpsee::{core::client::ClientT, rpc_params};
        use reth_storage_api::{StateProvider, StateProviderFactory};
        let spec = fixture_chain_spec();
        let genesis = spec.genesis_hash();
        let a = launch_test_node(spec.clone(), Runtime::test()).await?;
        let b = launch_test_node(spec.clone(), Runtime::test()).await?;
        let ca = a.auth_server_handle().http_client();
        let cb = b.auth_server_handle().http_client();
        for client in [&ca, &cb] {
            fcu(client, 2, genesis, genesis, None).await?;
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
            0,
            with_txs(fixture_attributes(99), &[anchor]),
        )
        .await?;
        for client in [&ca, &cb] {
            canonicalize(client, genesis, &legacy).await?;
        }
        let http = a.inner.rpc_server_handle().http_client().unwrap();
        // A seven-byte legacy parent crossing Etna still reaches the caught missing-root fallback.
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
            0,
            with_txs(
                fixture_attributes(100),
                &[signed_tx(1, false, Address::with_last_byte(0x21), Bytes::new())],
            ),
        )
        .await?;
        for client in [&ca, &cb] {
            canonicalize(client, genesis, &activation).await?;
        }
        assert_execution_parity(&b, &activation).await?;
        // EIP-4396 at the boundary uses the raw legacy parent, including its anchor gas.
        {
            use alethia_reth_consensus::{
                eip4396::{MIN_BASE_FEE, calculate_next_block_eip4396_base_fee},
                validation::ANCHOR_V3_V4_GAS_LIMIT,
            };
            let parent_fee = legacy.block.base_fee_per_gas.unwrap();
            let actual_fee = activation.block.base_fee_per_gas.unwrap();
            assert_eq!(
                actual_fee,
                calculate_next_block_eip4396_base_fee(
                    legacy.block.header(),
                    99,
                    parent_fee,
                    MIN_BASE_FEE
                )
            );
            let mut reserve_adjusted = legacy.block.header().clone();
            reserve_adjusted.gas_limit -= ANCHOR_V3_V4_GAS_LIMIT;
            assert_ne!(
                actual_fee,
                calculate_next_block_eip4396_base_fee(
                    &reserve_adjusted,
                    99,
                    parent_fee,
                    MIN_BASE_FEE
                )
            );
        }
        // The same fallback remains available over a normal post-Etna parent.
        assert_eq!(
            http.request::<serde_json::Value, _>(
                "eth_getBlockByNumber",
                rpc_params!["pending", true]
            )
            .await?,
            serde_json::Value::Null
        );
        // A's last resolved job remains legacy while B builds intervening blocks.
        let old: ExecutionPayloadEnvelopeV2 =
            ca.request("engine_getPayloadV2", rpc_params![legacy.id]).await?;
        assert_eq!(
            serde_json::to_value(old.execution_payload)?["blockHash"],
            legacy.payload["blockHash"]
        );
        assert!(
            ca.request::<serde_json::Value, _>("engine_getPayloadV5", rpc_params![legacy.id])
                .await
                .unwrap_err()
                .to_string()
                .contains("Unsupported fork")
        );
        // A builds the alternative legacy branch. B's retained job remains the Etna child.
        fcu(&ca, 2, genesis, genesis, None).await?;
        let alternative =
            build(&ca, spec.clone(), spec.genesis_header(), 0, fixture_attributes(98)).await?;
        for client in [&ca, &cb] {
            canonicalize(client, genesis, &alternative).await?;
        }
        for node in [&a, &b] {
            let state = node.inner.provider.latest()?;
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
        assert!(
            cb.request::<serde_json::Value, _>("engine_getPayloadV2", rpc_params![activation.id])
                .await
                .unwrap_err()
                .to_string()
                .contains("Unsupported fork")
        );
        let forward =
            build(&ca, spec.clone(), alternative.block.header(), 0, fixture_attributes(101))
                .await?;
        for client in [&ca, &cb] {
            canonicalize(client, genesis, &forward).await?;
        }
        let state = b.inner.provider.latest()?;
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
fn malicious_commitments_and_direct_tree_input_are_checked_during_execution() -> eyre::Result<()> {
    run_live_test(async {
        use alethia_reth_primitives::engine::TaikoEngineTypes;
        use alloy_primitives::{Address, Bytes, U256};
        use alloy_rpc_types_engine::PayloadStatus;
        use jsonrpsee::{core::client::ClientT, rpc_params};
        use reth_node_api::PayloadTypes;
        use reth_primitives_traits::SealedBlock;
        let spec = fixture_chain_spec();
        let genesis = spec.genesis_hash();
        let a = launch_test_node(spec.clone(), Runtime::test()).await?;
        let b = launch_test_node(spec.clone(), Runtime::test()).await?;
        let ca = a.auth_server_handle().http_client();
        let cb = b.auth_server_handle().http_client();
        for client in [&ca, &cb] {
            fcu(client, 3, genesis, genesis, None).await?;
        }
        let built = build(
            &ca,
            spec.clone(),
            spec.genesis_header(),
            0,
            with_txs(
                fixture_attributes(100),
                &[signed_tx(0, false, Address::with_last_byte(0x21), Bytes::new())],
            ),
        )
        .await?;
        let mut wrong = built.block.clone().into_block();
        wrong.header.difficulty += U256::from(1);
        let hash = wrong.header.hash_slow();
        let mut payload = built.payload.clone();
        payload["headerDifficulty"] = u64::try_from(wrong.header.difficulty)?.into();
        payload["blockHash"] = serde_json::to_value(hash)?;
        let status: PayloadStatus = cb
            .request(
                "engine_newPayloadV4",
                rpc_params![payload, Vec::<B256>::new(), built.root, Vec::<Bytes>::new()],
            )
            .await?;
        assert!(status.status.is_invalid());
        assert!(format!("{status:?}").contains("zk gas header difficulty mismatch"), "{status:?}");
        // An already-invalid head plus attributes is an ordinary INVALID FCU, not an internal
        // error.
        for (version, timestamp) in [(2, 99), (3, 101)] {
            let invalid =
                fcu(&cb, version, genesis, hash, Some(fixture_attributes(timestamp))).await?;
            assert!(invalid.payload_status.status.is_invalid());
            assert!(invalid.payload_id.is_none());
            assert_no_origin(&b, 1)?;
        }
        let changed_root: PayloadStatus = cb
            .request(
                "engine_newPayloadV4",
                rpc_params![
                    &built.payload,
                    Vec::<B256>::new(),
                    B256::with_last_byte(9),
                    Vec::<Bytes>::new()
                ],
            )
            .await?;
        assert!(changed_root.status.is_invalid());
        assert!(format!("{changed_root:?}").to_lowercase().contains("hash"));
        // A legacy 7-byte extraData is invalid from Etna on, even with a recomputed block hash.
        let mut short = built.block.clone().into_block();
        short.header.extra_data = Bytes::from_static(&[0; 7]);
        let mut payload = built.payload.clone();
        payload["extraData"] = serde_json::to_value(&short.header.extra_data)?;
        payload["blockHash"] = serde_json::to_value(short.header.hash_slow())?;
        let status: PayloadStatus = cb
            .request(
                "engine_newPayloadV4",
                rpc_params![payload, Vec::<B256>::new(), built.root, Vec::<Bytes>::new()],
            )
            .await?;
        assert!(status.status.is_invalid(), "{status:?}");
        assert!(format!("{status:?}").contains("extraData"), "{status:?}");
        let valid_direct = TaikoEngineTypes::block_to_payload(built.block.clone(), None);
        for sentinel in 0..2 {
            let mut data = valid_direct.clone();
            if sentinel == 0 {
                data.taiko_sidecar.block_access_list = Some(Bytes::new());
            } else {
                data.taiko_sidecar.slot_number = Some(1);
            }
            let status = a.inner.add_ons_handle.beacon_engine_handle.new_payload(data).await?;
            assert!(status.status.is_invalid(), "{status:?}");
        }
        let status = a.inner.add_ons_handle.beacon_engine_handle.new_payload(valid_direct).await?;
        assert!(status.status.is_valid(), "{status:?}");
        for field in ["blockAccessList", "slotNumber", "withdrawalsRoot"] {
            let mut payload = built.payload.clone();
            payload[field] = serde_json::json!("0x01");
            assert!(
                cb.request::<PayloadStatus, _>(
                    "engine_newPayloadV4",
                    rpc_params![payload, Vec::<B256>::new(), built.root, Vec::<Bytes>::new()]
                )
                .await
                .is_err()
            );
        }
        canonicalize(&cb, genesis, &built).await?;
        fcu(&ca, 3, genesis, built.block.hash(), None).await?;
        assert_execution_parity(&a, &built).await?;
        assert_execution_parity(&b, &built).await?;
        // Recomputing the hash does not rescue a wrong commitment on the direct tree path either.
        let status = a
            .inner
            .add_ons_handle
            .beacon_engine_handle
            .new_payload(TaikoEngineTypes::block_to_payload(SealedBlock::new_unhashed(wrong), None))
            .await?;
        assert!(status.status.is_invalid());
        Ok(())
    })
}

#[test]
fn full_block_trace_charges_ordinary_fees_to_golden_touch_and_checkpoint_calls() -> eyre::Result<()>
{
    run_live_test(async {
        use alloy_primitives::{Address, Bytes, U256};
        use jsonrpsee::{core::client::ClientT, rpc_params};
        use reth_storage_api::{ReceiptProvider, StateProvider, StateProviderFactory};
        let spec = fixture_chain_spec();
        let genesis = spec.genesis_hash();
        let node = launch_test_node(spec.clone(), Runtime::test()).await?;
        let client = node.auth_server_handle().http_client();
        fcu(&client, 3, genesis, genesis, None).await?;
        let mut checkpoint =
            alloy_primitives::keccak256(b"revealCheckpoint(uint48,bytes32,bytes32)")[..4].to_vec();
        checkpoint.extend_from_slice(&[0; 96]);
        let txs = [
            signed_tx(0, false, Address::with_last_byte(0x21), Bytes::new()),
            signed_tx(1, false, Address::with_last_byte(0x21), checkpoint.into()),
        ];
        let built = build(
            &client,
            spec.clone(),
            spec.genesis_header(),
            0,
            with_txs(fixture_attributes(100), &txs),
        )
        .await?;
        canonicalize(&client, genesis, &built).await?;
        assert_execution_parity(&node, &built).await?;
        let http = node.inner.rpc_server_handle().http_client().unwrap();
        let traces: serde_json::Value = http
            .request(
                "debug_traceBlockByHash",
                rpc_params![
                    built.block.hash(),
                    serde_json::json!({"tracer":"prestateTracer", "tracerConfig":{"diffMode":true}})
                ],
            )
            .await?;
        let receipts = node.inner.provider.receipts_by_block(built.block.hash().into())?.unwrap();
        assert_eq!(receipts.len(), 2);
        assert_eq!(traces.as_array().unwrap().len(), 2);
        let sender = Address::from(alethia_reth_primitives::addresses::TAIKO_GOLDEN_TOUCH_ADDRESS);
        let beneficiary = built.block.beneficiary;
        let mut cumulative = 0;
        for (receipt, trace) in receipts.iter().zip(traces.as_array().unwrap()) {
            let gas = receipt.cumulative_gas_used - cumulative;
            cumulative = receipt.cumulative_gas_used;
            let result = &trace["result"];
            let balance = |section: &str, address: Address| -> U256 {
                serde_json::from_value(
                    result[section][address.to_string().to_lowercase()]["balance"].clone(),
                )
                .unwrap()
            };
            assert_eq!(
                balance("pre", sender) - balance("post", sender),
                U256::from(gas) * U256::from(built.block.base_fee_per_gas.unwrap() + 7)
            );
            let pre = result["pre"][beneficiary.to_string().to_lowercase()]["balance"].clone();
            let before: U256 =
                if pre.is_null() { U256::ZERO } else { serde_json::from_value(pre)? };
            assert_eq!(balance("post", beneficiary) - before, U256::from(gas * 7));
        }
        let state = node.inner.provider.latest()?;
        assert_eq!(
            state.account_balance(&sender)?.unwrap(),
            U256::from(1_000_000_000_000_000_000_000u128) -
                U256::from(built.block.gas_used) *
                    U256::from(built.block.base_fee_per_gas.unwrap() + 7)
        );
        Ok(())
    })
}

#[test]
fn canonical_devnet_etna_genesis_keeps_zero_root_and_serves_pending_simulation() -> eyre::Result<()>
{
    run_live_test(async {
        use alethia_reth_chainspec::{TAIKO_DEVNET, spec::TaikoDevnetConfigExt};
        use jsonrpsee::{core::client::ClientT, rpc_params};
        use reth_storage_api::HeaderProvider;
        use serde_json::{Value, json};
        let spec = std::sync::Arc::new(
            TAIKO_DEVNET.clone_with_devnet_fork_timestamps(0, Some(0))?.unwrap(),
        );
        assert!(spec.genesis_header().extra_data.is_empty());
        let node = launch_test_node(spec.clone(), Runtime::test()).await?;
        let header = node.inner.provider.header_by_number(0)?.unwrap();
        assert_eq!(&header, spec.genesis_header());
        assert_eq!(header.parent_beacon_block_root, Some(B256::ZERO));
        let client = node.auth_server_handle().http_client();
        for version in [2, 3] {
            let status =
                fcu(&client, version, spec.genesis_hash(), spec.genesis_hash(), None).await?;
            assert!(status.payload_status.status.is_valid(), "{status:?}");
        }
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
        let http = node.inner.rpc_server_handle().http_client().unwrap();
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
        assert_eq!(http.request::<Value, _>("eth_blockNumber", rpc_params![]).await?, json!("0x0"));
        Ok(())
    })
}
