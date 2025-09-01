# Changelog

## [0.2.0](https://github.com/TatsujinLabs/taiko-reth/compare/v0.1.0...v0.2.0) (2025-08-25)


### Features

* **chainspec:** introduce `TaikoSpecId` ([#48](https://github.com/TatsujinLabs/taiko-reth/issues/48)) ([fabe734](https://github.com/TatsujinLabs/taiko-reth/commit/fabe734c01182cda6e0c0ef5ad81a08b1cd2e17d))
* **claude:** fix claude workflow and add taiko-reth-developer agent ([#46](https://github.com/TatsujinLabs/taiko-reth/issues/46)) ([b329d93](https://github.com/TatsujinLabs/taiko-reth/commit/b329d9359315ea6fae413416f31a16a4fcc5729a))
* **consensus:** improve `anchor` selector constants ([#52](https://github.com/TatsujinLabs/taiko-reth/issues/52)) ([fdbe004](https://github.com/TatsujinLabs/taiko-reth/commit/fdbe004ae6381e0c27095543cd14c49238f87a81))
* **consensus:** introduce `validate_anchor_transaction_in_block` for `TaikoBeaconConsensus` ([#47](https://github.com/TatsujinLabs/taiko-reth/issues/47)) ([c22afcb](https://github.com/TatsujinLabs/taiko-reth/commit/c22afcb12fbde4ffc69151aced0a2b8551c09b85))
* **executor:** add `execute_block` method with transaction validation for `prover` ([#58](https://github.com/TatsujinLabs/taiko-reth/issues/58)) ([ba2835f](https://github.com/TatsujinLabs/taiko-reth/commit/ba2835fd0e6f0bac37ee1430e7804843bce0991e))
* **rpc:** remove an unnecessary constant `COMPRESSION_ESTIMATION_SAFETY_COEF` ([#51](https://github.com/TatsujinLabs/taiko-reth/issues/51)) ([5e336ee](https://github.com/TatsujinLabs/taiko-reth/commit/5e336ee002817bbebb9edc428b137dd34e363514))


### Bug Fixes

* **evm:** fix a potential panic for `taiko_revm_spec` ([#54](https://github.com/TatsujinLabs/taiko-reth/issues/54)) ([5f4dba5](https://github.com/TatsujinLabs/taiko-reth/commit/5f4dba5fe67a555832f88fc03a2b4539df9fbfdc))


### Chores

* **block:** add some comments for `execute_block` ([#59](https://github.com/TatsujinLabs/taiko-reth/issues/59)) ([ad1028a](https://github.com/TatsujinLabs/taiko-reth/commit/ad1028aa5ab556878565345e8bdccbe0576b69d7))
* **ci:** build `linux/arm64` images in CI ([#44](https://github.com/TatsujinLabs/taiko-reth/issues/44)) ([e632c49](https://github.com/TatsujinLabs/taiko-reth/commit/e632c49d787fdf4f2aeae4aa96d51003d7f90db0))
* **ci:** improve `docker-build` workflow ([#42](https://github.com/TatsujinLabs/taiko-reth/issues/42)) ([87c523b](https://github.com/TatsujinLabs/taiko-reth/commit/87c523b13402005b635261cf2375f4789a6e4092))
* **ci:** revert `docker-build` action updates ([#45](https://github.com/TatsujinLabs/taiko-reth/issues/45)) ([ab588e8](https://github.com/TatsujinLabs/taiko-reth/commit/ab588e80a7738511f10696db09949ae703122bd6))
* **repo:** bump `reth` dependency to `v1.6.0` ([#16](https://github.com/TatsujinLabs/taiko-reth/issues/16)) ([6852971](https://github.com/TatsujinLabs/taiko-reth/commit/6852971cd530ec47ee0944b7c7d84cb6948cbb1f))


### Documentation

* **repo:** update `README.md` ([#50](https://github.com/TatsujinLabs/taiko-reth/issues/50)) ([a5119bd](https://github.com/TatsujinLabs/taiko-reth/commit/a5119bdd4dc24edcf64faa90b4af61aa2f33345b))


### Tests

* **block:** add more tests for `TaikoBlockExecutorFactory` ([#49](https://github.com/TatsujinLabs/taiko-reth/issues/49)) ([f7d66f0](https://github.com/TatsujinLabs/taiko-reth/commit/f7d66f0838bd20e18b77968bcf7310bec534f46b))

## [0.1.0](https://github.com/TatsujinLabs/taiko-reth/compare/v0.0.9...v0.1.0) (2025-07-27)


### Features

* add `StoredL1OriginTable` table ([c81b0a2](https://github.com/TatsujinLabs/taiko-reth/commit/c81b0a2a1ec44d98207e272801bb89fd730acfdb))
* add Auth RPCs ([e9b4dd6](https://github.com/TatsujinLabs/taiko-reth/commit/e9b4dd60979197490318eb424b1aa27f05309375))
* add tx_pool_content_with_min_tip ([79cde25](https://github.com/TatsujinLabs/taiko-reth/commit/79cde25c37d641205f7fb1188c35e27789c69404))
* **block:** use `load_cache_account` to load `TAIKO_GOLDEN_TOUCH_ADDRESS` account ([011d72f](https://github.com/TatsujinLabs/taiko-reth/commit/011d72f7aa0862a0867a2ff8d35d9a73be1bce1a))
* **chainspec:** improve chainspec pkg ([730faa9](https://github.com/TatsujinLabs/taiko-reth/commit/730faa93aab904655317e6495d99b4b6a1ef63b6))
* **chainspec:** introduce `TaikoExecutorSpec` trait ([#29](https://github.com/TatsujinLabs/taiko-reth/issues/29)) ([50d81fd](https://github.com/TatsujinLabs/taiko-reth/commit/50d81fda88fda7b15e5b6351e1f7115f522af6a1))
* **chainspec:** introduce `TaikoHardforks` trait ([fcb5e4e](https://github.com/TatsujinLabs/taiko-reth/commit/fcb5e4e89149ecb39b75014271f986ce99d67b30))
* **chainspec:** introduce Hekla testnet chainspec ([e83b5d0](https://github.com/TatsujinLabs/taiko-reth/commit/e83b5d0bec4b9b54da1eb92e012fa1a9e4fb2273))
* **cli:** improve `cli` pkg comments ([e3cf865](https://github.com/TatsujinLabs/taiko-reth/commit/e3cf8655773fe2ebcaa2df2e3ca3fd10bee1afc1))
* **consensus:** introduce `validate_against_parent_eip4936_base_fee` check ([8d50d47](https://github.com/TatsujinLabs/taiko-reth/commit/8d50d478301cb15121153bfb3c5c68c132868a75))
* **db:** improve `StoredL1Origin` `Compact` impl ([444f922](https://github.com/TatsujinLabs/taiko-reth/commit/444f9228eda60d3602afa47458a85503461dad28))
* **eth:** introduce `tx.commit` after db operation ([4683302](https://github.com/TatsujinLabs/taiko-reth/commit/4683302329337115ea47e25ead9a3e8326abc35f))
* **evm:** better `extra_execution_ctx` error handling ([ef4707c](https://github.com/TatsujinLabs/taiko-reth/commit/ef4707c3055dca0f6f56e9374c4d8606f5342759))
* **evm:** improve `Anchor` checks in handler ([#36](https://github.com/TatsujinLabs/taiko-reth/issues/36)) ([eec6204](https://github.com/TatsujinLabs/taiko-reth/commit/eec6204905c60e1e298d478a738065a1c2d44d9f))
* **evm:** improve `Anchor` transaction checks ([#35](https://github.com/TatsujinLabs/taiko-reth/issues/35)) ([89e9918](https://github.com/TatsujinLabs/taiko-reth/commit/89e9918c3151b817bdbeea33d60e1b3a5cc73016))
* **evm:** improve `Handler` ([efa9909](https://github.com/TatsujinLabs/taiko-reth/commit/efa9909e353603494c9de04f4df6dab415b10038))
* **evm:** improve `validate_against_state_and_deduct_caller` ([#19](https://github.com/TatsujinLabs/taiko-reth/issues/19)) ([9aee6fc](https://github.com/TatsujinLabs/taiko-reth/commit/9aee6fc3c262fd7e51d51e34af8f0459ff6890c1))
* **evm:** improve anchor transaction handling ([95f4403](https://github.com/TatsujinLabs/taiko-reth/commit/95f44034578a1b17194ce1403e91d35defc50379))
* **evm:** improve nonce checks in `reimburse_caller` ([#34](https://github.com/TatsujinLabs/taiko-reth/issues/34)) ([4205639](https://github.com/TatsujinLabs/taiko-reth/commit/420563922f8201228c091ab564f5bb9b909bb093))
* **evm:** introduce `extra_execution_ctx` ([31c8caa](https://github.com/TatsujinLabs/taiko-reth/commit/31c8caa38954cd7797aba4a3344ce9d45c0aaab4))
* **evm:** introduce more `inline` methods ([c8cbf3c](https://github.com/TatsujinLabs/taiko-reth/commit/c8cbf3cf44b1716928cb83712431fe2e3052f888))
* **evm:** more updates for `EvmHandler` to align the `taiko-geth` checks ([#20](https://github.com/TatsujinLabs/taiko-reth/issues/20)) ([62f6e39](https://github.com/TatsujinLabs/taiko-reth/commit/62f6e3938c2fd26e473224b8571cdc256905f7bc))
* **evm:** remove some unused code ([bbd7340](https://github.com/TatsujinLabs/taiko-reth/commit/bbd73403689d3f9c36706fcb8785bee5b96c7518))
* **factory:** decode `basefee_share_pctg` from `extra_data` ([23fd33e](https://github.com/TatsujinLabs/taiko-reth/commit/23fd33e9caa6ecc8eb00cc06dcbb07e8a0e15145))
* fix compiler error ([a68be33](https://github.com/TatsujinLabs/taiko-reth/commit/a68be33f3507813248ba79c651dcfc978607173c))
* implement `add_ons` later ([ad3bd4c](https://github.com/TatsujinLabs/taiko-reth/commit/ad3bd4cf3e11dbc5c264b485406c6b0b7b5df447))
* implement `PayloadBuilder` ([6f469c2](https://github.com/TatsujinLabs/taiko-reth/commit/6f469c23d517b9413ad22611986ad22836043796))
* implement `PayloadBuilderAttributes` ([c4ab8c0](https://github.com/TatsujinLabs/taiko-reth/commit/c4ab8c0c926dd266f485ac8a040198fb334b8c80))
* implement `taiko_payload` ([507161b](https://github.com/TatsujinLabs/taiko-reth/commit/507161befbdd32b1ee521c40aa007acc8a4b44e0))
* implement DebugNode for TaikoNode ([a16fb96](https://github.com/TatsujinLabs/taiko-reth/commit/a16fb9601f29fd31485648a498ea09521502257f))
* implement TaikoAddOns ([52600b1](https://github.com/TatsujinLabs/taiko-reth/commit/52600b1cbdddf378f9842e14e7a53a7741f97e9a))
* improve block assembler ([8e7049a](https://github.com/TatsujinLabs/taiko-reth/commit/8e7049a032b1a676035a696966b7e3e06eb6c12c))
* improve engine API ([6045379](https://github.com/TatsujinLabs/taiko-reth/commit/6045379564e6b38479811962ca9197af5823d165))
* improve TaikoPayloadBuilder ([3d900d1](https://github.com/TatsujinLabs/taiko-reth/commit/3d900d1c3766436b65a494f7a04f64b3efecae25))
* initial commit ([70f7ebb](https://github.com/TatsujinLabs/taiko-reth/commit/70f7ebbff5761d96136d816b32e64f56904a3a88))
* introduce `chainspec` ([6c40ccc](https://github.com/TatsujinLabs/taiko-reth/commit/6c40ccce0cdff98998bb0ca2042c1095b83b1ad1))
* introduce `cli` package ([ebae161](https://github.com/TatsujinLabs/taiko-reth/commit/ebae16187355d108cf58ee4249ba8e9ddf23dbcb))
* introduce `cli` package ([70bd39d](https://github.com/TatsujinLabs/taiko-reth/commit/70bd39d74d46fa64ba0c719ca404c7befa8d116c))
* introduce `COMPRESSION_ESTIMATION_SAFTY_COEF` ([95da36c](https://github.com/TatsujinLabs/taiko-reth/commit/95da36cd67f5ac4b3938b47ff12dec22a7eebd74))
* introduce `TaikoApiError::GethNotFound ` ([52168f4](https://github.com/TatsujinLabs/taiko-reth/commit/52168f4638f8dbdf4ff92dc4b3885b2a98be5464))
* introduce `TaikoBlockExecutor` ([694d0d1](https://github.com/TatsujinLabs/taiko-reth/commit/694d0d1f5bc43e89f6e7c4fb43c8b41925e6f536))
* introduce `TaikoChainSpec` ([34ec64e](https://github.com/TatsujinLabs/taiko-reth/commit/34ec64eba0bf397b555bb967f3cf33a09bca3831))
* introduce `TaikoChainSpecParser` ([5762ee6](https://github.com/TatsujinLabs/taiko-reth/commit/5762ee6533780ae57a14c1318d91102214cad728))
* introduce `TaikoConsensusBuilder` ([b4fc3aa](https://github.com/TatsujinLabs/taiko-reth/commit/b4fc3aab1e7e444df200fd81bee2fc94006e85df))
* introduce `TaikoEngineApiBuilder` ([fd8042c](https://github.com/TatsujinLabs/taiko-reth/commit/fd8042cd910239c3b127fde188822798ebda2354))
* introduce `TaikoEthApi` ([f9ba4e2](https://github.com/TatsujinLabs/taiko-reth/commit/f9ba4e2f31ba12467ba887f501a97520a2892b14))
* introduce `TaikoExecutionData` ([1706435](https://github.com/TatsujinLabs/taiko-reth/commit/1706435281d1219c2d61a300430510595b19bdb3))
* introduce `TaikoNextBlockEnvAttributes` ([9059378](https://github.com/TatsujinLabs/taiko-reth/commit/905937838f3d4d38e0ef698b2d7c23278033dff4))
* introduce consensus validation ([c1e4f72](https://github.com/TatsujinLabs/taiko-reth/commit/c1e4f7294617abe50daffc8ec26ed78623da0d22))
* introduce devnet json ([23cbd80](https://github.com/TatsujinLabs/taiko-reth/commit/23cbd8004a86c40b70e3cd019a711dd9e6fac973))
* introduce TaikoChainSpec ([d04ad7d](https://github.com/TatsujinLabs/taiko-reth/commit/d04ad7d6997bc1054e3cba8c9b84e152bca090b8))
* introduce TaikoExtApi ([0ac9836](https://github.com/TatsujinLabs/taiko-reth/commit/0ac983690fab09808605d997d651e803cf3459cb))
* **payload:** compatibility changes for `TaikoExecutionData` ([ba3ea3d](https://github.com/TatsujinLabs/taiko-reth/commit/ba3ea3d462ffb76e4cfa7e344787d75f7ea6178c))
* **payload:** improve `withdrawals` checks ([dabec5e](https://github.com/TatsujinLabs/taiko-reth/commit/dabec5e7c2a584b453037507d6873b5c5aa30ca6))
* **payload:** use `alloy_primitives::Bytes` ([6a11c4a](https://github.com/TatsujinLabs/taiko-reth/commit/6a11c4a91376bb409fe898cbdb68790ebd56411e))
* remove an unused file ([2554523](https://github.com/TatsujinLabs/taiko-reth/commit/25545238d032f7bdad9440980cb24924f9cfedf8))
* **repo:** improve error handling ([bcb6a0b](https://github.com/TatsujinLabs/taiko-reth/commit/bcb6a0b76ea996cd7891f1cae195e8e3169e88b6))
* **rpc:** implement `txPoolContentWithMinTip` API ([c6e83ef](https://github.com/TatsujinLabs/taiko-reth/commit/c6e83efbf46e1c1b8d3902cb8dd4d95c012b8651))
* **rpc:** improve `TaikoEngineApi` APIs ([d527ff9](https://github.com/TatsujinLabs/taiko-reth/commit/d527ff9f9b7b684ec902e6f39b44e7509a4c81ea))
* **rpc:** improve block hash fetching ([b2ecf0d](https://github.com/TatsujinLabs/taiko-reth/commit/b2ecf0de1a135b64a31f471e5b5fc8108254a97f))
* **rpc:** improve state provider error handling ([4b30a6b](https://github.com/TatsujinLabs/taiko-reth/commit/4b30a6b39972e1c5c4bfd794e20809facd152053))
* **rpc:** introduce `taikoAuth_setL1OriginSignature` RPC ([e7c163a](https://github.com/TatsujinLabs/taiko-reth/commit/e7c163a04d7938c99f6e159f8a914bbca07e56a1))
* **rpc:** rename `L1Origin` to `RpcL1Origin` ([f6194c1](https://github.com/TatsujinLabs/taiko-reth/commit/f6194c164b48cef64c967c4057f9ed3eb194bf03))
* **rpc:** use `GethNotFound` when no item found in database ([8a08ed2](https://github.com/TatsujinLabs/taiko-reth/commit/8a08ed2fa8a6691fe7798991120ecd102448dc7b))
* two more impls ([75e9d8c](https://github.com/TatsujinLabs/taiko-reth/commit/75e9d8c834e1c5ae163327d43b809a6fcb49343d))
* update `build_evm` ([e120516](https://github.com/TatsujinLabs/taiko-reth/commit/e120516763fb786a7219196a35d700680e3ec5f6))
* update `TaikoEngineApi` ([8b40007](https://github.com/TatsujinLabs/taiko-reth/commit/8b400076fbef3202d5cfb34cab2c43b7ad5c677f))
* update `TaikoEngineValidator` ([c7602e0](https://github.com/TatsujinLabs/taiko-reth/commit/c7602e060644eaa6b2976c624e7a4e78fbf6837b))
* update evm creation ([b45c339](https://github.com/TatsujinLabs/taiko-reth/commit/b45c33904910a75010e8d9710a5cd8cdfc116edb))
* update execution ([8ddcaec](https://github.com/TatsujinLabs/taiko-reth/commit/8ddcaec83139b50981529bded6ec5ad76aca4456))
* update factory ([8e69190](https://github.com/TatsujinLabs/taiko-reth/commit/8e691907457563c0bb2e5f3ab43b750ab0321f7e))
* update fork_choice_updated_v2 ([8a07146](https://github.com/TatsujinLabs/taiko-reth/commit/8a07146ed7477999a079a6ffdc058c6667989931))
* update l1Origin APIs ([5be6c89](https://github.com/TatsujinLabs/taiko-reth/commit/5be6c890d0515f5f1fe1b02348f6777d4bd930a3))
* update lib.rs ([7f827ba](https://github.com/TatsujinLabs/taiko-reth/commit/7f827ba009a1b00b2697f639a742def31a4c0e3e))
* update namespace ([7d78977](https://github.com/TatsujinLabs/taiko-reth/commit/7d78977b208dc097a6083774fed697519a99d347))
* update payload ([f075fb5](https://github.com/TatsujinLabs/taiko-reth/commit/f075fb5e530cd9d5a4950b8a60a49dbeb751d060))
* update payload id ([b33b05b](https://github.com/TatsujinLabs/taiko-reth/commit/b33b05b7408a89abcd6da3d2ac49fbff2e3a407b))
* update payload input ([fa40bc2](https://github.com/TatsujinLabs/taiko-reth/commit/fa40bc2b6cb728ed6572be5d3d377be95a0a34e4))
* update Provider ([107b1b1](https://github.com/TatsujinLabs/taiko-reth/commit/107b1b1f7a924f4896a069d2bad0960997299fee))
* update TaikoAddOns ([a657e13](https://github.com/TatsujinLabs/taiko-reth/commit/a657e13277f097a48649683e2020f47e690815a7))
* use `BuildOutcome::Freeze` for payload builder ([ef116f6](https://github.com/TatsujinLabs/taiko-reth/commit/ef116f65699e79a2f2316fd7380f948ad578117a))
* use `TaikoEngineApiServer` trait ([f4aec97](https://github.com/TatsujinLabs/taiko-reth/commit/f4aec97c659bff26ffb17928776776ae59a85ae6))
* use `TaikoPayloadBuilderBuilder` ([4234a29](https://github.com/TatsujinLabs/taiko-reth/commit/4234a29f2887377768a03c89a24e2c1a8370f7e6))


### Bug Fixes

* **evm:** fix `reimburse_caller` implementation in EVM handler ([#17](https://github.com/TatsujinLabs/taiko-reth/issues/17)) ([238b4d9](https://github.com/TatsujinLabs/taiko-reth/commit/238b4d9cc4211c6eb0a926b99700d515df002c18))
* **evm:** fix basefee sharing ([eba256a](https://github.com/TatsujinLabs/taiko-reth/commit/eba256a48a2e650c8170a5390622fe64ba8a5bc0))
* **evm:** touch `treasury_account` in `reward_beneficiary` ([2e51bce](https://github.com/TatsujinLabs/taiko-reth/commit/2e51bce7074e0d197500ad18555593b75b963be3))
* **factory:** fix `golden_touch_address_initialial_nonce` before execution ([33ddd2d](https://github.com/TatsujinLabs/taiko-reth/commit/33ddd2d359f2bedc8b46d7d45cb95f707ecd1ba0))
* fix `evm_env` ([e085cb6](https://github.com/TatsujinLabs/taiko-reth/commit/e085cb601cff99a71aba50b0cb7bbaf61a0ba39a))
* fix hex decoding ([9f7d804](https://github.com/TatsujinLabs/taiko-reth/commit/9f7d80464e2cde3d1bee9474e463ec8927455435))
* fix mainnet genesis JSON ([101fb28](https://github.com/TatsujinLabs/taiko-reth/commit/101fb284d0244455bd1979e6c9b0fdcc598f1d6a))
* fix RPC return types ([473cd19](https://github.com/TatsujinLabs/taiko-reth/commit/473cd199f6ea06459a2d64f936a56a4bbc9a32d5))
* **repo:** fix a typo in `Cargo.toml` ([#15](https://github.com/TatsujinLabs/taiko-reth/issues/15)) ([b70eda7](https://github.com/TatsujinLabs/taiko-reth/commit/b70eda70de15d74e333d9c8d36641025138596c3))
* **rpc:** fix state provider database in `tx_pool_content_with_min_tip` RPC ([d588a65](https://github.com/TatsujinLabs/taiko-reth/commit/d588a65c341cbf821f7c88beb08b262d5c129027))
* **rpc:** improve `set_head_l1_origin` return type ([152d020](https://github.com/TatsujinLabs/taiko-reth/commit/152d02086efefd4e3918fcc04ac98742a17cdd06))


### Chores

* **bin:** update comments in `main.rs` ([689e68a](https://github.com/TatsujinLabs/taiko-reth/commit/689e68a5607fb798400f29c4e761f86c463f3eee))
* **ci:** add `Dockerfile` ([4227b25](https://github.com/TatsujinLabs/taiko-reth/commit/4227b258bed79bbbb2c9b4022a4c921cd493b499))
* **ci:** create `rust.yml` ([484163f](https://github.com/TatsujinLabs/taiko-reth/commit/484163f1aec95e88d6e1bd39a1ab4af4c646dc91))
* **ci:** introduce `[profile.dev]` ([d2d0b70](https://github.com/TatsujinLabs/taiko-reth/commit/d2d0b70dd59f7422f1e286be46b2a91bc885ac03))
* **ci:** introduce `docker-build.yml` in workflow ([#14](https://github.com/TatsujinLabs/taiko-reth/issues/14)) ([05c013e](https://github.com/TatsujinLabs/taiko-reth/commit/05c013eed5cfb29cf0c0606af1b68c946a427cf7))
* **ci:** update `.release-please-manifest.json` ([#33](https://github.com/TatsujinLabs/taiko-reth/issues/33)) ([c145f3f](https://github.com/TatsujinLabs/taiko-reth/commit/c145f3f29dd978e2fa069aeefafbb10a87edbb02))
* **ci:** update `docker-build` workflow ([4bef5df](https://github.com/TatsujinLabs/taiko-reth/commit/4bef5dff5114c5d803c28728da7e852a68537442))
* **ci:** update `release-please` manifest ([#32](https://github.com/TatsujinLabs/taiko-reth/issues/32)) ([bb1c36f](https://github.com/TatsujinLabs/taiko-reth/commit/bb1c36f679cb9f161bed9b9c17a98a2f4780cad7))
* **ci:** update `release-please` workflow ([#28](https://github.com/TatsujinLabs/taiko-reth/issues/28)) ([7f615cc](https://github.com/TatsujinLabs/taiko-reth/commit/7f615cc03898ae0ccd78cd49b2f204a900eb71d4))
* **ci:** update `release-please` workflow ([#30](https://github.com/TatsujinLabs/taiko-reth/issues/30)) ([9a80fe2](https://github.com/TatsujinLabs/taiko-reth/commit/9a80fe247334a4ee2a4498abfe9de1befa54989e))
* **consensus:** update checks ([480ecb6](https://github.com/TatsujinLabs/taiko-reth/commit/480ecb6657f9998627d4ecbef0da452c33b34515))
* **db:** add more comments for `StoredL1Origin` ([fdb2ce9](https://github.com/TatsujinLabs/taiko-reth/commit/fdb2ce9388a5bc839c0d6197ee9b597606149168))
* **docker:** use `nightly` as default tag ([#18](https://github.com/TatsujinLabs/taiko-reth/issues/18)) ([d80dc0b](https://github.com/TatsujinLabs/taiko-reth/commit/d80dc0b11b4f5d82c54cfc9eaf68360d44c046d0))
* **evm:** update comments in `evm` pkg ([110f248](https://github.com/TatsujinLabs/taiko-reth/commit/110f248bc8e8bcc57df86d7b83677d39ef8cd913))
* **factory:** remove unused code ([4fd90f7](https://github.com/TatsujinLabs/taiko-reth/commit/4fd90f7edba425d555fb56f98820aa7cbcb40596))
* **factory:** update comments in `factory` pkg ([f78de75](https://github.com/TatsujinLabs/taiko-reth/commit/f78de75f443568dc510692f68e2a60e03363a6f0))
* **payload:** improve payload builder ([360e32f](https://github.com/TatsujinLabs/taiko-reth/commit/360e32f2a2fa04ace7cc4ff47eac3d0bc7123025))
* remove an unused mod ([a4312bb](https://github.com/TatsujinLabs/taiko-reth/commit/a4312bba1317c05303d9b9502e77cbbb5b37c18f))
* **repo:** improve logging messages ([a7ef5c9](https://github.com/TatsujinLabs/taiko-reth/commit/a7ef5c9b4da09eb6c98e48ec3fd930ec9c99a94c))
* **repo:** introduce `clippy` rules ([#23](https://github.com/TatsujinLabs/taiko-reth/issues/23)) ([ec0080e](https://github.com/TatsujinLabs/taiko-reth/commit/ec0080e59ca5f3dd0091646d325ca127373bae48))
* **repo:** introduce `release-please` ([#26](https://github.com/TatsujinLabs/taiko-reth/issues/26)) ([05db995](https://github.com/TatsujinLabs/taiko-reth/commit/05db99557cce56ed2c860202aa1666f9c76262d3))
* **repo:** introduce `rustfmt.toml` ([#22](https://github.com/TatsujinLabs/taiko-reth/issues/22)) ([a3b9272](https://github.com/TatsujinLabs/taiko-reth/commit/a3b9272d1b8b7e81132fc814b7c6f60d57b6b0cf))
* **repo:** introduce `typos.yaml` ([#24](https://github.com/TatsujinLabs/taiko-reth/issues/24)) ([ccf5d88](https://github.com/TatsujinLabs/taiko-reth/commit/ccf5d883c8dd38bb4c708c39b3465ad86a3ebb86))
* **repo:** remove some unnecessary bounds ([721a2e7](https://github.com/TatsujinLabs/taiko-reth/commit/721a2e731b4e2880bb0291358a5289df87ed5387))
* **repo:** update `reth` dependencies to `v1.5.1` ([#9](https://github.com/TatsujinLabs/taiko-reth/issues/9)) ([c155d15](https://github.com/TatsujinLabs/taiko-reth/commit/c155d158e7902cb78986cc142fb5bf00987b56a6))
* **repo:** updates based on Claude Code report ([#41](https://github.com/TatsujinLabs/taiko-reth/issues/41)) ([b9485c1](https://github.com/TatsujinLabs/taiko-reth/commit/b9485c1fb80582b2b9f3125dea5a03df16832a63))
* **rpc:** fix lint warnings ([393a85c](https://github.com/TatsujinLabs/taiko-reth/commit/393a85c8c61ef0bc7e9b75b94990a5a4c57b2f56))
* **rpc:** improve comments in `rpc` pkg ([67564cf](https://github.com/TatsujinLabs/taiko-reth/commit/67564cf74cbc442b1b1f7882db0c0eb3439d643a))
* **rpc:** improve provider error handling ([545d707](https://github.com/TatsujinLabs/taiko-reth/commit/545d707b109474aa37c96ca577b3d76df3026749))
* **rust:** update `Cargo.toml` ([c22a490](https://github.com/TatsujinLabs/taiko-reth/commit/c22a490b3afaedb3f32e07af3ca90d57e0834842))
* update genesis json ([17481e8](https://github.com/TatsujinLabs/taiko-reth/commit/17481e83135b4022356d84f8b3ac0c74e874ca65))


### Documentation

* **db:** update comments in `db` pkg ([0ddc4d3](https://github.com/TatsujinLabs/taiko-reth/commit/0ddc4d31eb5fce7115df2651c9e93f8a5669c532))
* **README:** add CI badge in `README` ([#25](https://github.com/TatsujinLabs/taiko-reth/issues/25)) ([d246f27](https://github.com/TatsujinLabs/taiko-reth/commit/d246f2736f6a64ce8b7b6c845fada0a2ade84fbe))
* **repo:** improve building docs ([517a04c](https://github.com/TatsujinLabs/taiko-reth/commit/517a04c24b1abee1629da7e199df0e66b34ed935))
* **repo:** update `README` ([f763f94](https://github.com/TatsujinLabs/taiko-reth/commit/f763f94f40875ea32efa6c8086aedf6e0cde0dbe))
* **repo:** update `README` ([c0a31b8](https://github.com/TatsujinLabs/taiko-reth/commit/c0a31b840fca20d70a01a87594083e7c8bee791c))
* **repo:** update README ([573aa97](https://github.com/TatsujinLabs/taiko-reth/commit/573aa9771dd2af48f06c5e1f952d14b78faef12c))
* **repo:** update README.md ([ca3fea2](https://github.com/TatsujinLabs/taiko-reth/commit/ca3fea209c9eabaac476c2faf1e2ec0a7deca620))
* **repo:** update URLs in `README` ([#13](https://github.com/TatsujinLabs/taiko-reth/issues/13)) ([bb06ddc](https://github.com/TatsujinLabs/taiko-reth/commit/bb06ddc3744bbf48f6c20d5ce14bd1979054874a))
* **rpc:** add more comments in `tx_pool_content_with_min_tip` ([5ed6b33](https://github.com/TatsujinLabs/taiko-reth/commit/5ed6b33bb2b1421f3ad6d198bb14b447449c4533))


### Code Refactoring

* **block:** refactor `block` package ([91e00d2](https://github.com/TatsujinLabs/taiko-reth/commit/91e00d2143e631a45f32c134bf68e5f04a94af1f))
* **evm:** move the execution traits to `execution.rs` ([dd1f754](https://github.com/TatsujinLabs/taiko-reth/commit/dd1f75421f12bd69cd92e5f435436125bd10edc5))
* **rpc:** introduce `rpc/engine` package ([6835504](https://github.com/TatsujinLabs/taiko-reth/commit/6835504540becfc25b3aa974addd0335111f7225))


### Tests

* add debug info ([145c6ad](https://github.com/TatsujinLabs/taiko-reth/commit/145c6ad30304172f1aea6691e622af3fd28d1e26))
* add genesis hash test ([cd28bef](https://github.com/TatsujinLabs/taiko-reth/commit/cd28befb2d7b55573826ed96c37c02d2236d9be2))
* **block:** add more unit tests ([068a027](https://github.com/TatsujinLabs/taiko-reth/commit/068a02766e8964fa40a6b2eab5f2d711e659731a))
* **chainspec:** add more unit tests ([1ad7b7a](https://github.com/TatsujinLabs/taiko-reth/commit/1ad7b7a00d0336f3b95741aac2b27e7cf1357054))
* **chainspec:** fix parser tests ([6ea1a46](https://github.com/TatsujinLabs/taiko-reth/commit/6ea1a4656a6829fe02c0cfd45ed779162eab662e))
* **cli:** add tests for `TaikoTables` ([763f6cd](https://github.com/TatsujinLabs/taiko-reth/commit/763f6cd7943459e29af92db39429bfe659a5a7fe))
* **consensus:** add more block validation unit tests ([3e445cd](https://github.com/TatsujinLabs/taiko-reth/commit/3e445cdde490cc7e783a00961094145166185588))
* **consensus:** fix test errors ([c6835ec](https://github.com/TatsujinLabs/taiko-reth/commit/c6835ec0b2088f08093f981d03e9bb8f896e36e3))
* **db:** add more tests for `StoredL1Origin` compression ([5e24160](https://github.com/TatsujinLabs/taiko-reth/commit/5e241608a31207a50479ceae1daa40fbac9a9e0d))
* **evm:** add more evm unit tests ([d2baede](https://github.com/TatsujinLabs/taiko-reth/commit/d2baedec7169b6d1afd66ebdb396eb674e5808e0))
* **evm:** fix an unit test ([#37](https://github.com/TatsujinLabs/taiko-reth/issues/37)) ([fda652a](https://github.com/TatsujinLabs/taiko-reth/commit/fda652a0eacb0578d0719fe6dabb97bc66886c38))
* **rpc:** update more tests ([bb2e735](https://github.com/TatsujinLabs/taiko-reth/commit/bb2e7353f228b56a67a6d273ca7a24358d2aa417))


### Workflow

* **repo:** enable `Claude Code` in workflow ([#39](https://github.com/TatsujinLabs/taiko-reth/issues/39)) ([d612ae5](https://github.com/TatsujinLabs/taiko-reth/commit/d612ae5febd3484740f1a8cbf714b71af8d2f770))
