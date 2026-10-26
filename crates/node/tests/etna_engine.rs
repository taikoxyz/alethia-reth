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
            // `alternative` is an Unzen block: EIP-4788 stores its timestamp with the zero root.
            assert_eq!(
                state.storage(
                    alloy_eips::eip4788::BEACON_ROOTS_ADDRESS,
                    B256::from(U256::from(98))
                )?,
                Some(U256::from(98))
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
        // An empty Etna block runs EIP-2935/4788 without gas or zk gas.
        assert_eq!((forward.block.gas_used, forward.block.difficulty), (0, U256::ZERO));
        assert_eq!(forward.payload["headerDifficulty"], 0);
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
        let spec = std::sync::Arc::new(TAIKO_DEVNET.clone_with_devnet_etna_timestamp(0).unwrap());
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
        // Simulation meters zk gas against the full block budget and reports the zk limit as an
        // error, not a result: modexp with a 1,024-byte modulus and an all-ones 32-byte exponent
        // costs ~8.4M gas, which at multiplier 154 is far above the 100M zk budget.
        let mut input = Vec::with_capacity(96 + 32 + 32 + 1024);
        for len in [32_u64, 32, 1024] {
            input.extend_from_slice(&alloy_primitives::U256::from(len).to_be_bytes::<32>());
        }
        input.extend_from_slice(&[0x02; 32]);
        input.extend_from_slice(&[0xff; 32]);
        input.extend_from_slice(&[0x01; 1024]);
        let modexp = json!({
            "to": "0x0000000000000000000000000000000000000005",
            "data": alloy_primitives::Bytes::from(input),
            "gas": "0x989680",
        });
        for method in ["eth_call", "eth_estimateGas"] {
            let result = http.request::<Value, _>(method, rpc_params![&modexp, "latest"]).await;
            let Err(jsonrpsee::core::ClientError::Call(e)) = result else {
                panic!("{method} must report the zk limit as a call error: {result:?}");
            };
            assert_eq!(e.code(), -32603, "{method}: {e:?}");
            assert!(e.message().contains("zk gas limit exceeded"), "{method}: {e:?}");
        }
        Ok(())
    })
}

#[test]
fn etna_mixed_build_puts_the_forced_transaction_first_within_the_byte_limit() -> eyre::Result<()> {
    run_live_test(async {
        use alethia_reth_primitives::etna::execution_block_length;
        use alloy_primitives::{Address, Bytes};
        let spec = fixture_chain_spec();
        let genesis = spec.genesis_hash();
        let node = launch_test_node(spec.clone()).await?;
        let client = node.auth_server_handle().http_client();
        let http = node.rpc_server_handle().http_client().unwrap();
        fcu(&client, genesis, genesis, None).await?;
        let target = Address::with_last_byte(0x21);
        // A pooled transaction of ~2 KB and a small golden-touch forced transaction.
        let p = signed_tx_with_key(POOL_SENDER_KEY, 0, target, Bytes::from(vec![0; 2_000]));
        assert_eq!(send_raw(&http, &p).await?, *p.tx_hash());
        let fi = signed_tx(0, false, target, Bytes::new());
        let (fi_hash, p_hash) = (*fi.tx_hash(), *p.tx_hash());
        let attrs = fixture_attributes(100);
        let build_on_genesis = |attrs| build(&client, spec.clone(), spec.genesis_header(), attrs);
        let hashes = |built: &Built| {
            built.block.body().transactions.iter().map(|tx| *tx.tx_hash()).collect::<Vec<_>>()
        };
        let listed = |txs: &[_], max_block_bytes: u64| {
            let mut attrs = with_txs(attrs.clone(), txs);
            attrs.max_block_bytes = Some(max_block_bytes);
            attrs
        };

        // Mixed: the forced transaction is transaction 0 and the pool fills the rest.
        let mixed = build_on_genesis(pool_mode(attrs.clone(), Some(&fi), 124_000)).await?;
        assert_eq!(hashes(&mixed), [fi_hash, p_hash]);
        let length = execution_block_length(mixed.block.header(), &mixed.block.body().transactions);
        assert!(length <= 124_000, "{length}");
        let length = u64::try_from(length)?;

        // Pool only: a job differing only in `forcedTransaction` gets its own ID.
        let pool = build_on_genesis(pool_mode(attrs.clone(), None, 124_000)).await?;
        assert_ne!(pool.id, mixed.id);
        assert_eq!(hashes(&pool), [p_hash]);

        // The header reserve, live: 16 bytes above the built length still holds both
        // transactions; one byte below it leaves the pooled transaction out.
        let roomy = build_on_genesis(pool_mode(attrs.clone(), Some(&fi), length + 16)).await?;
        assert_eq!(hashes(&roomy), [fi_hash, p_hash]);
        assert_eq!(roomy.block.hash(), mixed.block.hash());
        let tight = build_on_genesis(pool_mode(attrs.clone(), Some(&fi), length - 1)).await?;
        assert_eq!(hashes(&tight), [fi_hash]);

        // Given list through the same call path: the byte budget leaves out a listed
        // transaction that no longer fits.
        let listed_roomy = build_on_genesis(listed(&[fi.clone(), p.clone()], length + 16)).await?;
        assert_eq!(listed_roomy.block.hash(), mixed.block.hash());
        let listed_tight = build_on_genesis(listed(&[fi.clone(), p.clone()], length - 1)).await?;
        assert_eq!(hashes(&listed_tight), [fi_hash]);
        let listed_small = build_on_genesis(listed(std::slice::from_ref(&p), 1_024)).await?;
        assert_eq!(hashes(&listed_small), Vec::<B256>::new());

        // A forced transaction with a nonce gap is not transaction 0, and its given-list build
        // alone is empty: the execution-layer condition for skipping the entry.
        let fi_gap = signed_tx(5, false, target, Bytes::new());
        let gapped = build_on_genesis(pool_mode(attrs.clone(), Some(&fi_gap), 124_000)).await?;
        assert_eq!(hashes(&gapped), [p_hash]);
        let alone = build_on_genesis(listed(&[fi_gap], 124_000)).await?;
        assert_eq!(hashes(&alone), Vec::<B256>::new());

        // The smallest accepted bound leaves no room for the pooled transaction.
        let small = build_on_genesis(pool_mode(attrs.clone(), None, 1_024)).await?;
        assert_eq!(hashes(&small), Vec::<B256>::new());

        // Etna attributes without `maxBlockBytes` are rejected before any job starts.
        let mut unbounded = pool_mode(attrs.clone(), None, 124_000);
        unbounded.max_block_bytes = None;
        let error = fcu(&client, genesis, genesis, Some(unbounded))
            .await
            .expect_err("an Etna build without maxBlockBytes must be rejected");
        assert_eq!(rpc_error_code(&error), Some(-32602), "{error:?}");
        assert!(format!("{error:?}").contains("maxBlockBytes"), "{error:?}");
        Ok(())
    })
}

/// Builds empty Etna blocks at `timestamps` on the node's head `chain.last()`, each made canonical
/// with head = safe = finalized, as a driver with instant finality sends them.
async fn extend_finalized(
    client: &impl jsonrpsee::core::client::ClientT,
    spec: std::sync::Arc<alethia_reth_chainspec::spec::TaikoChainSpec>,
    chain: &mut Vec<alloy_consensus::Header>,
    timestamps: &[u64],
) -> eyre::Result<()> {
    for &timestamp in timestamps {
        let parent = chain.last().unwrap();
        let grandparent_timestamp = chain.iter().rev().nth(1).map_or(0, |header| header.timestamp);
        let built = build_at(
            client,
            spec.clone(),
            parent,
            grandparent_timestamp,
            parent.hash_slow(),
            fixture_attributes(timestamp),
        )
        .await?;
        canonicalize(client, built.block.hash(), &built).await?;
        chain.push(built.block.header().clone());
    }
    Ok(())
}

#[test]
fn etna_one_shot_rewind_moves_the_head_back_and_survives_restarts() -> eyre::Result<()> {
    run_live_test(async {
        use reth_provider::{BlockNumReader, DatabaseProviderFactory};
        let spec = fixture_chain_spec();
        let genesis = spec.genesis_hash();
        let datadir = Datadir::new();

        // A normal EL follows a driver with instant finality to block 3.
        let el = launch_at(spec.clone(), &datadir, EngineMode::Normal).await?;
        let client = el.node.auth_server_handle().http_client();
        let http = el.node.rpc_server_handle().http_client().unwrap();
        fcu(&client, genesis, genesis, None).await?;
        let mut chain = vec![spec.genesis_header().clone()];
        extend_finalized(&client, spec.clone(), &mut chain, &[100, 101, 102]).await?;
        let (c, old_child) = (chain[1].hash_slow(), chain[2].hash_slow());
        // Without the flags the rewind FCU (head = safe = finalized = c) is VALID but moves
        // nothing, so the driver must check that block c + 1 is gone.
        let status = fcu(&client, c, c, None).await?;
        assert!(status.payload_status.status.is_valid(), "{status:?}");
        assert_eq!(head_number(&http).await?, 3);
        assert_eq!(hash_at(&http, 2).await?, Some(old_child));
        el.stop().await?;

        // One restart with the rewind flags serves exactly one FCU: the rewind to c. The
        // shutdown persisted blocks 1..3, so the rewind removes blocks from disk.
        let el = launch_at(spec.clone(), &datadir, EngineMode::Rewind).await?;
        assert_eq!(el.node.provider.database_provider_ro()?.last_block_number()?, 3);
        let client = el.node.auth_server_handle().http_client();
        let http = el.node.rpc_server_handle().http_client().unwrap();
        let status = fcu(&client, c, c, None).await?;
        assert!(status.payload_status.status.is_valid(), "{status:?}");
        assert_eq!(status.payload_id, None);
        assert_eq!(head_number(&http).await?, 1);
        assert_eq!(hash_at(&http, 1).await?, Some(c));
        assert_eq!(hash_at(&http, 2).await?, None);
        el.stop().await?;

        // A normal restart keeps the rewind, with c as the latest, safe and finalized block.
        let el = launch_at(spec.clone(), &datadir, EngineMode::Normal).await?;
        assert_eq!(el.node.provider.database_provider_ro()?.last_block_number()?, 1);
        let client = el.node.auth_server_handle().http_client();
        let http = el.node.rpc_server_handle().http_client().unwrap();
        assert_eq!(head_number(&http).await?, 1);
        assert_eq!(hash_at(&http, 1).await?, Some(c));
        assert_eq!(hash_at(&http, 2).await?, None);
        for tag in ["safe", "finalized"] {
            assert_eq!(hash_of_tag(&http, tag).await?, Some(c), "{tag}");
        }
        // Repeating the rewind FCU is now a no-op, and c + 1 is built on c as usual.
        let status = fcu(&client, c, c, None).await?;
        assert!(status.payload_status.status.is_valid(), "{status:?}");
        chain.truncate(2);
        extend_finalized(&client, spec.clone(), &mut chain, &[110]).await?;
        let new_child = chain[2].hash_slow();
        assert_ne!(new_child, old_child);
        assert_eq!(chain[2].parent_hash, c);
        assert_eq!(head_number(&http).await?, 2);
        assert_eq!(hash_at(&http, 2).await?, Some(new_child));
        el.stop().await?;

        // Another normal restart keeps the new chain.
        let el = launch_at(spec.clone(), &datadir, EngineMode::Normal).await?;
        let http = el.node.rpc_server_handle().http_client().unwrap();
        assert_eq!(head_number(&http).await?, 2);
        assert_eq!(hash_at(&http, 1).await?, Some(c));
        assert_eq!(hash_at(&http, 2).await?, Some(new_child));
        assert_eq!(hash_at(&http, 3).await?, None);
        el.stop().await?;
        Ok(())
    })
}

#[test]
fn etna_fcu_to_a_canonical_ancestor_builds_on_it_and_unwinds_only_with_the_rewind_flags()
-> eyre::Result<()> {
    run_live_test(async {
        let spec = fixture_chain_spec();
        let genesis = spec.genesis_hash();
        let datadir = Datadir::new();
        let el = launch_at(spec.clone(), &datadir, EngineMode::Normal).await?;
        let client = el.node.auth_server_handle().http_client();
        let http = el.node.rpc_server_handle().http_client().unwrap();
        fcu(&client, genesis, genesis, None).await?;
        let mut chain = vec![spec.genesis_header().clone()];
        extend_finalized(&client, spec.clone(), &mut chain, &[100, 101, 102]).await?;
        let c = chain[1].hash_slow();
        let child_of_c = fixture_attributes(110);
        // A normal EL answers an FCU with attributes to c, below the finalized block 3, with a
        // build on c (never -38006) and keeps its head.
        let probe =
            build_at(&client, spec.clone(), &chain[1], chain[0].timestamp, c, child_of_c.clone())
                .await?;
        assert_eq!(probe.block.parent_hash, c);
        assert_eq!(head_number(&http).await?, 3);
        el.stop().await?;

        // With the flags the same FCU also moves the head back to c at once, which is why they
        // must never stay on.
        let el = launch_at(spec.clone(), &datadir, EngineMode::Rewind).await?;
        let client = el.node.auth_server_handle().http_client();
        let http = el.node.rpc_server_handle().http_client().unwrap();
        let built =
            build_at(&client, spec.clone(), &chain[1], chain[0].timestamp, c, child_of_c).await?;
        assert_eq!(built.block.hash(), probe.block.hash());
        assert_eq!(head_number(&http).await?, 1);
        assert_eq!(hash_at(&http, 2).await?, None);
        canonicalize(&client, built.block.hash(), &built).await?;
        assert_eq!(head_number(&http).await?, 2);
        assert_eq!(hash_at(&http, 2).await?, Some(built.block.hash()));
        el.stop().await?;

        let el = launch_at(spec.clone(), &datadir, EngineMode::Normal).await?;
        let http = el.node.rpc_server_handle().http_client().unwrap();
        assert_eq!(head_number(&http).await?, 2);
        assert_eq!(hash_at(&http, 2).await?, Some(built.block.hash()));
        assert_eq!(hash_at(&http, 3).await?, None);
        el.stop().await?;
        Ok(())
    })
}
