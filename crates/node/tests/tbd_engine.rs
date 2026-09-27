mod support;
use alloy_primitives::B256;
use reth_chainspec::EthChainSpec;
use reth_tasks::Runtime;
use support::*;

#[test]
fn nonvalid_fcu_with_attributes_preserves_upstream_status() -> eyre::Result<()> {
    run_live_test(async {
        let spec = fixture_chain_spec();
        let genesis = spec.genesis_hash();
        let node = launch_test_node(spec, Runtime::test()).await?;
        let client = node.auth_server_handle().http_client();
        for (version, timestamp) in [(2, 99), (3, 100)] {
            assert!(
                fcu(&client, version, genesis, genesis, None)
                    .await?
                    .payload_status
                    .status
                    .is_valid()
            );
            let response = fcu(
                &client,
                version,
                genesis,
                B256::with_last_byte(200),
                Some(fixture_attributes(timestamp)),
            )
            .await?;
            assert!(response.payload_status.status.is_syncing());
            assert_eq!(response.payload_id, None);
            assert_no_origin(&node, 1)?;
        }
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
        // A seven-byte legacy parent crossing TBD still reaches the caught missing-root fallback.
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
        // The same fallback remains available over a normal post-TBD parent.
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
        // A builds the alternative legacy branch. B's retained job remains the TBD child.
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
fn canonical_devnet_tbd_genesis_pending_rpc_uses_simulation_context() -> eyre::Result<()> {
    run_live_test(async {
        use alethia_reth_chainspec::{TAIKO_DEVNET, spec::TaikoDevnetConfigExt};
        use jsonrpsee::{core::client::ClientT, rpc_params};
        use serde_json::{Value, json};
        let spec = std::sync::Arc::new(
            TAIKO_DEVNET.clone_with_devnet_fork_timestamps(0, Some(0))?.unwrap(),
        );
        assert!(spec.genesis_header().extra_data.is_empty());
        let node = launch_test_node(spec, Runtime::test()).await?;
        let http = node.inner.rpc_server_handle().http_client().unwrap();
        let tx = json!({"to": "0x0000000000000000000000000000000000000021"});
        // Gather every result before asserting so RED records all three affected HTTP methods.
        let call = http.request::<Value, _>("eth_call", rpc_params![&tx, "pending"]).await;
        let estimate =
            http.request::<Value, _>("eth_estimateGas", rpc_params![&tx, "pending"]).await;
        let block =
            http.request::<Value, _>("eth_getBlockByNumber", rpc_params!["pending", true]).await;
        assert!(
            call.is_ok() && estimate.is_ok() && block.is_ok(),
            "call={call:?}; estimate={estimate:?}; block={block:?}"
        );
        assert_eq!(call?, json!("0x"));
        assert_eq!(estimate?, json!("0x5208"));
        // No authoritative L1 root is available: the caught local-build failure stays null.
        assert_eq!(block?, Value::Null);
        assert_eq!(http.request::<Value, _>("eth_blockNumber", rpc_params![]).await?, json!("0x0"));
        Ok(())
    })
}

#[test]
fn canonical_genesis_at_tbd_zero_retains_zero_beacon_root() -> eyre::Result<()> {
    run_live_test(async {
        use alloy_primitives::U256;
        use reth_storage_api::{HeaderProvider, StateProvider, StateProviderFactory};
        let spec = fixture_chain_spec_at(0);
        let node = launch_test_node(spec.clone(), Runtime::test()).await?;
        let header = node.inner.provider.header_by_number(0)?.unwrap();
        assert_eq!(&header, spec.genesis_header());
        assert_eq!(header.parent_beacon_block_root, Some(B256::ZERO));
        let state = node.inner.provider.latest()?;
        assert_eq!(
            state
                .storage(alloy_eips::eip4788::BEACON_ROOTS_ADDRESS, B256::ZERO)?
                .unwrap_or_default(),
            U256::ZERO
        );
        let client = node.auth_server_handle().http_client();
        for version in [2, 3] {
            assert!(
                fcu(&client, version, spec.genesis_hash(), spec.genesis_hash(), None)
                    .await?
                    .payload_status
                    .status
                    .is_valid()
            );
        }
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
fn first_failure_derivation_and_actual_import_rejection_preserve_empty_system_writes()
-> eyre::Result<()> {
    run_live_test(async {
        use alethia_reth_block::derived_block::{assemble_filtered_block, execute_derived_block};
        use alethia_reth_primitives::{
            engine::TaikoEngineTypes, payload::builder::decode_recovered_transactions,
        };
        use alloy_consensus::{SignableTransaction, Signed, TxEip4844, TxLegacy};
        use alloy_eips::Encodable2718;
        use alloy_primitives::{Address, Bytes, Signature, U256};
        use alloy_signer::SignerSync;
        use reth_node_api::PayloadTypes;
        use reth_primitives_traits::{RecoveredBlock, SealedBlock, SealedHeader};
        use reth_revm::database::StateProviderDatabase;
        use reth_storage_api::{StateProvider, StateProviderFactory, StateRootProvider};
        let spec = fixture_chain_spec();
        let genesis = spec.genesis_hash();
        let a = launch_test_node(spec.clone(), Runtime::test()).await?;
        let b = launch_test_node(spec.clone(), Runtime::test()).await?;
        let ca = a.auth_server_handle().http_client();
        let cb = b.auth_server_handle().http_client();
        let signer: alloy_signer_local::PrivateKeySigner =
            "0x92954368afd3caa1f3ce3ead0069c1af414054aefe1ef9aeacc1bf426222ce38".parse()?;
        let sign = |nonce, gas_limit, to| {
            let tx = TxLegacy {
                chain_id: Some(167001),
                nonce,
                gas_limit,
                gas_price: 1_000_000_000,
                to: alloy_primitives::TxKind::Call(to),
                ..Default::default()
            };
            let sig = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
            reth_ethereum_primitives::TransactionSigned::from(Signed::new_unhashed(tx, sig))
        };
        let valid = signed_tx(0, false, Address::with_last_byte(0x21), Bytes::new());
        let signature: reth_ethereum_primitives::TransactionSigned = Signed::new_unhashed(
            TxLegacy::default(),
            Signature::new(U256::ZERO, U256::from(1), false),
        )
        .into();
        let blob = TxEip4844 {
            chain_id: 167001,
            gas_limit: 100_000,
            max_fee_per_gas: 1_000_000_000,
            max_fee_per_blob_gas: 1,
            blob_versioned_hashes: vec![B256::repeat_byte(1)],
            ..Default::default()
        };
        let sig = signer.sign_hash_sync(&blob.signature_hash())?;
        let blob: reth_ethereum_primitives::TransactionSigned =
            Signed::new_unhashed(blob, sig).into();
        for (name, first, truncates) in [
            ("nonce", sign(99, 100_000, Address::with_last_byte(0x21)), false),
            ("signature", signature, false),
            ("type", blob, false),
            ("gas", sign(0, 30_000_001, Address::with_last_byte(0x21)), false),
            ("zk", sign(0, 5_000_000, Address::with_last_byte(0x22)), true),
        ] {
            for client in [&ca, &cb] {
                fcu(client, 3, genesis, genesis, None).await?;
            }
            let txs = vec![first, valid.clone()];
            let built = build(
                &ca,
                spec.clone(),
                spec.genesis_header(),
                0,
                with_txs(fixture_attributes(100), &txs),
            )
            .await?;
            let expected = if truncates { vec![] } else { vec![valid.clone()] };
            assert_eq!(built.block.body().transactions, expected, "{name}");
            let raw = built.attrs.block_metadata.tx_list.as_ref().unwrap();
            let recovered = decode_recovered_transactions(raw)?;
            let mut candidate = built.block.clone().into_block();
            candidate.body.transactions = recovered.iter().map(|tx| tx.clone_inner()).collect();
            candidate.header.transactions_root =
                alloy_consensus::proofs::calculate_transaction_root(&candidate.body.transactions);
            let candidate = RecoveredBlock::new_unhashed(
                candidate,
                recovered.iter().map(|tx| tx.signer()).collect(),
            );
            let state = a.inner.provider.history_by_block_hash(genesis)?;
            let parent = SealedHeader::seal_slow(spec.genesis_header().clone());
            let result = execute_derived_block(
                &a.inner.evm_config,
                &parent,
                &candidate,
                StateProviderDatabase::new(&*state),
            )?;
            let root = state.state_root(result.hashed_state)?;
            let filtered = assemble_filtered_block(
                &a.inner.evm_config,
                &parent,
                &candidate,
                result.committed_transactions,
                &result.execution_result,
                result.finalized_block_zk_gas,
                root,
            )?;
            assert_eq!(filtered.header(), built.block.header(), "{name}");
            // The original body is not the derived body: recompute its tx root/hash so import
            // reaches signature/type/execution validation rather than a stale hash comparison.
            let mut original = built.block.clone().into_block();
            original.body.transactions = txs;
            original.header.transactions_root =
                alloy_consensus::proofs::calculate_transaction_root(&original.body.transactions);
            let original = SealedBlock::new_unhashed(original);
            let data = TaikoEngineTypes::block_to_payload(original.clone(), None);
            let direct = a.inner.add_ons_handle.beacon_engine_handle.new_payload(data).await;
            match name {
                // Pinned Reth f2eecc6 payload_validator.rs:1236 classifies recovery failure as
                // Internal. Preserve this upstream rejection; do not duplicate signature recovery.
                "signature" => {
                    assert!(direct.unwrap_err().to_string().to_lowercase().contains("recover"))
                }
                _ => assert!(direct?.status.is_invalid(), "{name}"),
            }
            let mut rpc_original = built.payload.clone();
            rpc_original["transactions"] = serde_json::to_value(
                original
                    .body()
                    .transactions
                    .iter()
                    .map(|tx| Bytes::from(tx.encoded_2718()))
                    .collect::<Vec<_>>(),
            )?;
            rpc_original["blockHash"] = serde_json::to_value(original.hash())?;
            use jsonrpsee::{core::client::ClientT, rpc_params};
            let rpc = cb
                .request::<alloy_rpc_types_engine::PayloadStatus, _>(
                    "engine_newPayloadV4",
                    rpc_params![rpc_original, Vec::<B256>::new(), built.root, Vec::<Bytes>::new()],
                )
                .await;
            match name {
                "signature" => {
                    assert!(rpc.unwrap_err().to_string().contains("Failed to recover the signer"))
                }
                _ => assert!(rpc?.status.is_invalid(), "{name}"),
            }
            canonicalize(&cb, genesis, &built).await?;
            let state = b.inner.provider.latest()?;
            assert_eq!(
                state.storage(
                    alloy_eips::eip4788::BEACON_ROOTS_ADDRESS,
                    B256::from(U256::from(8291))
                )?,
                Some(U256::from(1)),
                "{name}"
            );
            assert_eq!(
                state.storage(alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS, B256::ZERO)?,
                Some(U256::from_be_bytes(genesis.0)),
                "{name}"
            );
            // HTTP tx-list replay executes the original list against real parent proofs.
            let http = b.inner.rpc_server_handle().http_client().unwrap();
            let witness: alloy_rpc_types_debug::ExecutionWitness = http
                .request(
                    "debug_executionWitnessForTxList",
                    rpc_params![alloy_eips::BlockId::hash(built.block.hash()), raw],
                )
                .await?;
            assert!(witness.codes.contains(&alloy_eips::eip4788::BEACON_ROOTS_CODE));
            assert!(witness.codes.contains(&alloy_eips::eip2935::HISTORY_STORAGE_CODE));
        }
        Ok(())
    })
}

#[test]
fn deterministic_handoff_vectors_origin_metadata_and_activation_parent_fee() -> eyre::Result<()> {
    run_live_test(async {
        use alethia_reth_consensus::eip4396::{
            MIN_BASE_FEE, calculate_next_block_eip4396_base_fee,
        };
        use alloy_primitives::{Address, Bytes, U256};
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
        assert_origin(&a, &legacy, 1)?;
        verify_vector("legacy-parent", vector(&b, &legacy, spec.genesis_header())?)?;
        let activation = build(
            &ca,
            spec.clone(),
            legacy.block.header(),
            0,
            with_txs(
                fixture_attributes(100),
                &[signed_tx(1, false, Address::with_last_byte(0x21), Bytes::new())],
            ),
        )
        .await?;
        let actual_fee = activation.block.base_fee_per_gas.unwrap();
        assert_eq!(
            actual_fee,
            calculate_next_block_eip4396_base_fee(
                legacy.block.header(),
                99,
                legacy.block.base_fee_per_gas.unwrap(),
                MIN_BASE_FEE
            )
        );
        let mut wrongly_adjusted = legacy.block.header().clone();
        wrongly_adjusted.gas_limit -= alethia_reth_consensus::validation::ANCHOR_V3_V4_GAS_LIMIT;
        assert_ne!(
            actual_fee,
            calculate_next_block_eip4396_base_fee(
                &wrongly_adjusted,
                99,
                legacy.block.base_fee_per_gas.unwrap(),
                MIN_BASE_FEE
            )
        );
        for client in [&ca, &cb] {
            canonicalize(client, genesis, &activation).await?;
        }
        verify_vector("activation", vector(&b, &activation, legacy.block.header())?)?;
        assert_origin(&a, &activation, 2)?;
        let normal = build(
            &ca,
            spec.clone(),
            activation.block.header(),
            99,
            with_txs(
                fixture_attributes(101),
                &[signed_tx(2, false, Address::with_last_byte(0x21), Bytes::new())],
            ),
        )
        .await?;
        for client in [&ca, &cb] {
            canonicalize(client, genesis, &normal).await?;
        }
        verify_vector("normal", vector(&b, &normal, activation.block.header())?)?;
        let empty =
            build(&ca, spec.clone(), normal.block.header(), 100, fixture_attributes(102)).await?;
        for client in [&ca, &cb] {
            canonicalize(client, genesis, &empty).await?;
        }
        verify_vector("empty", vector(&b, &empty, normal.block.header())?)?;
        let mut attrs = fixture_attributes(103);
        attrs.l1_origin.is_forced_inclusion = true;
        attrs.l1_origin.signature = [7; 65];
        attrs.l1_origin.build_payload_args_id = [3; 8];
        let forced = build(&ca, spec.clone(), empty.block.header(), 101, attrs).await?;
        for client in [&ca, &cb] {
            canonicalize(client, genesis, &forced).await?;
        }
        assert_origin(&a, &forced, 5)?;
        verify_vector("forced", vector(&b, &forced, empty.block.header())?)?;
        let mut attrs = fixture_attributes(104);
        attrs.block_metadata.tx_list = Some(Bytes::from_static(&[1]));
        attrs.l1_origin.l1_block_height = None;
        attrs.l1_origin.l1_block_hash = None;
        let default = build(&ca, spec.clone(), forced.block.header(), 102, attrs).await?;
        assert!(default.block.body().transactions.is_empty());
        assert_eq!(default.block.difficulty, U256::ZERO);
        for client in [&ca, &cb] {
            canonicalize(client, genesis, &default).await?;
        }
        assert_origin(&a, &default, 5)?; // Preconfirmation persists its origin but cannot move the L1/proposal head.
        verify_vector("default-preconfirmation", vector(&b, &default, forced.block.header())?)?;
        for client in [&ca, &cb] {
            fcu(client, 2, genesis, genesis, None).await?;
        }
        let alt =
            build(&ca, spec.clone(), spec.genesis_header(), 0, fixture_attributes(98)).await?;
        for client in [&ca, &cb] {
            canonicalize(client, genesis, &alt).await?;
        }
        verify_vector("reorg-legacy", vector(&b, &alt, spec.genesis_header())?)?;
        let forward = build(
            &ca,
            spec.clone(),
            alt.block.header(),
            0,
            with_txs(
                fixture_attributes(101),
                &[signed_tx(0, false, Address::with_last_byte(0x21), Bytes::new())],
            ),
        )
        .await?;
        for client in [&ca, &cb] {
            canonicalize(client, genesis, &forward).await?;
        }
        verify_vector("reorg-forward", vector(&b, &forward, alt.block.header())?)?;
        Ok(())
    })
}

async fn historical_snapshot(stage: usize, name: &str) -> eyre::Result<()> {
    use alethia_reth_consensus::validation::{
        ANCHOR_V1_SELECTOR, ANCHOR_V2_SELECTOR, ANCHOR_V3_SELECTOR, ANCHOR_V4_SELECTOR,
    };
    use alloy_consensus::{SignableTransaction, Signed, TxEip1559};
    use alloy_primitives::{Address, Bytes};
    use alloy_signer::SignerSync;
    let signer: alloy_signer_local::PrivateKeySigner =
        "0x92954368afd3caa1f3ce3ead0069c1af414054aefe1ef9aeacc1bf426222ce38".parse()?;
    let selector = [
        ANCHOR_V1_SELECTOR,
        ANCHOR_V2_SELECTOR,
        ANCHOR_V3_SELECTOR,
        ANCHOR_V4_SELECTOR,
        ANCHOR_V4_SELECTOR,
    ][stage];
    let spec = historical_chain_spec(stage);
    let genesis = spec.genesis_hash();
    // A fresh runtime and node for every historical fork configuration.
    let node = launch_test_node(spec.clone(), Runtime::test()).await?;
    let client = node.auth_server_handle().http_client();
    fcu(&client, 2, genesis, genesis, None).await?;
    let anchor = TxEip1559 {
        chain_id: 167001,
        gas_limit: if stage < 2 { 250_000 } else { 1_000_000 },
        max_fee_per_gas: 1_000_000_000,
        to: Address::with_last_byte(0x21).into(),
        input: Bytes::copy_from_slice(selector),
        ..Default::default()
    };
    let sig = signer.sign_hash_sync(&anchor.signature_hash())?;
    let anchor = Signed::new_unhashed(anchor, sig).into();
    let attrs = with_txs(
        fixture_attributes(99),
        &[anchor, signed_tx(1, false, Address::with_last_byte(0x21), Bytes::new())],
    );
    let built = build(&client, spec.clone(), spec.genesis_header(), 0, attrs).await?;
    canonicalize(&client, genesis, &built).await?;
    verify_vector(&format!("historical-{name}"), vector(&node, &built, spec.genesis_header())?)?;
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
