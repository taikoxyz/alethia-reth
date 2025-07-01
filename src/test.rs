// use reth::tasks::TaskExecutor;
// use reth_node_api::FullNodeTypes;
// use reth_node_builder::{
//     LaunchNode, NodeAdapter, NodeBuilderWithComponents, NodeComponentsBuilder, WithLaunchContext,
//     rpc::RethRpcAddOns,
// };
// pub struct TestWithLaunchContext<Builder> {
//     pub builder: Builder,
// }

// impl<T, CB, AO> TestWithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>
// where
//     T: FullNodeTypes,
//     CB: NodeComponentsBuilder<T>,
//     AO: RethRpcAddOns<NodeAdapter<T, CB::Components>>,
// {
//     /// Launches the node with the given launcher.
//     pub fn launch_with<L>(self, launcher: L) -> eyre::Result<L::Node>
//     where
//         L: LaunchNode<NodeBuilderWithComponents<T, CB, AO>>,
//     {
//         // launcher.launch_node(self.builder)
//         todo!()
//     }
// }
