use std::{convert::Infallible, sync::Arc};

use alloy_primitives::{Address, B256, Bytes, U256};
use reth_engine_local::LocalPayloadAttributesBuilder;
use reth_ethereum::EthPrimitives;
use reth_evm::ConfigureEvm;
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_api::{
    AddOnsContext, FullNodeComponents, FullNodeTypes, NodeAddOns, NodeTypes,
    PayloadAttributesBuilder, PayloadTypes,
};
use reth_node_builder::{
    DebugNode, Node, NodeAdapter, NodeComponentsBuilder,
    components::{BasicPayloadServiceBuilder, ComponentsBuilder},
    rpc::{EngineValidatorAddOn, EngineValidatorBuilder, RethRpcAddOns, RpcAddOns, RpcHandle},
};
use reth_node_ethereum::node::EthereumPoolBuilder;
use reth_rpc::eth::core::EthRpcConverterFor;
use reth_storage_api::EthStorage;
use reth_trie_db::MerklePatriciaTrie;

use crate::{
    block::{assembler::TaikoBlockAssembler, factory::TaikoBlockExecutorFactory},
    chainspec::spec::TaikoChainSpec,
    evm::{config::TaikoNextBlockEnvAttributes, factory::TaikoEvmFactory},
    node::builder_components::{
        api_builder::TaikoEthApiBuilder, consensus::TaikoConsensusBuilder,
        engine_api::TaikoEngineApiBuilder, engine_validator::TaikoEngineValidatorBuilder,
        executor::TaikoExecutorBuilder, network::TaikoNetworkBuilder,
        payload::TaikoPayloadBuilderBuilder,
    },
    payload::{
        attributes::{RpcL1Origin, TaikoBlockMetadata, TaikoPayloadAttributes},
        builder::TAIKO_PACAYA_BLOCK_GAS_LIMIT,
        engine::TaikoEngineTypes,
    },
    rpc::{engine::validator::TaikoEngineValidator, eth::types::TaikoEthApi},
};

pub mod builder_components;

/// The main node type for a Taiko network node, implementing the `NodeTypes` trait.
#[derive(Debug, Clone, Default)]
pub struct TaikoNode;

impl NodeTypes for TaikoNode {
    /// The node's primitive types, defining basic operations and structures.
    type Primitives = EthPrimitives;
    /// The type used for configuration of the EVM.
    type ChainSpec = TaikoChainSpec;
    /// The type used to perform state commitment operations.
    type StateCommitment = MerklePatriciaTrie;
    /// The type responsible for writing chain primitives to storage.
    type Storage = EthStorage;
    /// The node's engine types, defining the interaction with the consensus engine.
    type Payload = TaikoEngineTypes;
}

/// Taiko custom addons which configuring RPC types.
pub struct TaikoAddOns<
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = TaikoChainSpec,
                StateCommitment = MerklePatriciaTrie,
                Storage = EthStorage,
                Payload = TaikoEngineTypes,
            >,
            Evm: ConfigureEvm<
                Primitives = EthPrimitives,
                Error = Infallible,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
                BlockExecutorFactory = TaikoBlockExecutorFactory<
                    RethReceiptBuilder,
                    Arc<TaikoChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
    EV,
>(pub RpcAddOns<N, TaikoEthApiBuilder, EV, TaikoEngineApiBuilder<EV>>);

impl<N, EV> Default for TaikoAddOns<N, EV>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = TaikoChainSpec,
                StateCommitment = MerklePatriciaTrie,
                Storage = EthStorage,
                Payload = TaikoEngineTypes,
            >,
            Evm: ConfigureEvm<
                Primitives = EthPrimitives,
                Error = Infallible,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
                BlockExecutorFactory = TaikoBlockExecutorFactory<
                    RethReceiptBuilder,
                    Arc<TaikoChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
    EV: Default,
{
    /// Creates a new instance of `TaikoAddOns` with default configurations.
    fn default() -> Self {
        let add_ons = RpcAddOns::new(
            TaikoEthApiBuilder::default(),
            EV::default(),
            TaikoEngineApiBuilder::default(),
            Default::default(),
        );

        TaikoAddOns(add_ons)
    }
}

impl<N, EV> NodeAddOns<N> for TaikoAddOns<N, EV>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = TaikoChainSpec,
                StateCommitment = MerklePatriciaTrie,
                Storage = EthStorage,
                Payload = TaikoEngineTypes,
            >,
            Evm: ConfigureEvm<
                Primitives = EthPrimitives,
                Error = Infallible,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
                BlockExecutorFactory = TaikoBlockExecutorFactory<
                    RethReceiptBuilder,
                    Arc<TaikoChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
    EV: EngineValidatorBuilder<N>,
{
    /// Handle to add-ons.
    type Handle = RpcHandle<N, TaikoEthApi<N, EthRpcConverterFor<N>>>;

    /// Configures and launches the add-ons.
    async fn launch_add_ons(
        self,
        ctx: reth_node_api::AddOnsContext<'_, N>,
    ) -> eyre::Result<Self::Handle> {
        self.0.launch_add_ons(ctx).await
    }
}

impl<N, EV> RethRpcAddOns<N> for TaikoAddOns<N, EV>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = TaikoChainSpec,
                StateCommitment = MerklePatriciaTrie,
                Storage = EthStorage,
                Payload = TaikoEngineTypes,
            >,
            Evm: ConfigureEvm<
                Primitives = EthPrimitives,
                Error = Infallible,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
                BlockExecutorFactory = TaikoBlockExecutorFactory<
                    RethReceiptBuilder,
                    Arc<TaikoChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
    EV: EngineValidatorBuilder<N>,
{
    /// eth API implementation.
    type EthApi = TaikoEthApi<N, EthRpcConverterFor<N>>;

    /// Returns a mutable reference to RPC hooks.
    fn hooks_mut(&mut self) -> &mut reth_node_builder::rpc::RpcHooks<N, Self::EthApi> {
        self.0.hooks_mut()
    }
}

impl<N, EV> EngineValidatorAddOn<N> for TaikoAddOns<N, EV>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = TaikoChainSpec,
                StateCommitment = MerklePatriciaTrie,
                Storage = EthStorage,
                Payload = TaikoEngineTypes,
            >,
            Evm: ConfigureEvm<
                Primitives = EthPrimitives,
                Error = Infallible,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
                BlockExecutorFactory = TaikoBlockExecutorFactory<
                    RethReceiptBuilder,
                    Arc<TaikoChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
    EV: EngineValidatorBuilder<N>,
{
    /// The Validator type to use for the engine API.
    type Validator = TaikoEngineValidator;

    /// Creates the engine validator for an engine API based node.
    async fn engine_validator(&self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::Validator> {
        TaikoEngineValidatorBuilder::default().build(ctx).await
    }
}

impl<N> Node<N> for TaikoNode
where
    N: FullNodeTypes<
        Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = TaikoChainSpec,
            StateCommitment = MerklePatriciaTrie,
            Storage = EthStorage,
            Payload = TaikoEngineTypes,
        >,
    >,
{
    /// The type that builds the node's components.
    type ComponentsBuilder = ComponentsBuilder<
        N,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<TaikoPayloadBuilderBuilder>,
        TaikoNetworkBuilder,
        TaikoExecutorBuilder,
        TaikoConsensusBuilder,
    >;

    /// Exposes the customizable node add-on types.
    type AddOns = TaikoAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
        TaikoEngineValidatorBuilder,
    >;

    /// Returns a [`NodeComponentsBuilder`] for the node.
    fn components_builder(&self) -> Self::ComponentsBuilder {
        ComponentsBuilder::default()
            .node_types()
            .pool(EthereumPoolBuilder::default())
            .executor(TaikoExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::new(TaikoPayloadBuilderBuilder))
            .network(TaikoNetworkBuilder)
            .consensus(TaikoConsensusBuilder::default())
    }

    /// Returns the node add-ons.
    fn add_ons(&self) -> Self::AddOns {
        TaikoAddOns::default()
    }
}

impl<
    N: FullNodeComponents<
            Types = Self,
            Evm: ConfigureEvm<
                Primitives = EthPrimitives,
                Error = Infallible,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
                BlockExecutorFactory = TaikoBlockExecutorFactory<
                    RethReceiptBuilder,
                    Arc<TaikoChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
> DebugNode<N> for TaikoNode
{
    /// RPC block type. Used by [`DebugConsensusClient`] to fetch blocks and submit them to the
    /// engine.
    type RpcBlock = alloy_rpc_types_eth::Block;

    /// Converts an RPC block to a primitive block.
    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_ethereum_primitives::Block {
        rpc_block.into_consensus().convert_transactions()
    }

    /// Creates a payload attributes builder for local mining in dev mode.
    ///
    ///  It will be used by the `LocalMiner` when dev mode is enabled.
    ///
    /// The builder is responsible for creating the payload attributes that define how blocks should
    /// be constructed during local mining.
    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl PayloadAttributesBuilder<
        <<Self as reth_node_api::NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
    > {
        LocalPayloadAttributesBuilder::new(Arc::new(chain_spec.clone()))
    }
}

/// Implement `PayloadAttributesBuilder` for `LocalPayloadAttributesBuilder<TaikoChainSpec>`,
/// to build `TaikoPayloadAttributes` from the local payload attributes builder.
impl PayloadAttributesBuilder<TaikoPayloadAttributes>
    for LocalPayloadAttributesBuilder<TaikoChainSpec>
{
    /// Return a new payload attribute from the builder.
    fn build(&self, timestamp: u64) -> TaikoPayloadAttributes {
        TaikoPayloadAttributes {
            payload_attributes: self.build(timestamp),
            base_fee_per_gas: U256::ZERO,
            block_metadata: TaikoBlockMetadata {
                beneficiary: Address::random(),
                timestamp: U256::from(timestamp),
                gas_limit: TAIKO_PACAYA_BLOCK_GAS_LIMIT,
                mix_hash: B256::random(),
                tx_list: Bytes::new(),
                extra_data: Bytes::new(),
            },
            l1_origin: RpcL1Origin {
                block_id: U256::ZERO,
                l2_block_hash: B256::ZERO,
                l1_block_hash: None,
                l1_block_height: None,
                build_payload_args_id: [0; 8],
                is_forced_inclusion: false,
                signature: [0; 65],
            },
        }
    }
}
