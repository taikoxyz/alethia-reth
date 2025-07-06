use std::sync::Arc;

use reth::{chainspec::ChainSpec, primitives::Block};
use reth_ethereum::{Receipt, TransactionSigned};
use reth_evm::{
    block::{BlockExecutionError, BlockExecutorFactory},
    eth::EthBlockExecutionCtx,
    execute::{BlockAssembler, BlockAssemblerInput},
};
use reth_evm_ethereum::EthBlockAssembler;

#[derive(Clone, Debug)]
pub struct TaikoBlockAssembler {
    block_assembler: EthBlockAssembler<ChainSpec>,
}

impl TaikoBlockAssembler {
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self {
            block_assembler: EthBlockAssembler::new(chain_spec),
        }
    }

    pub fn chain_spec(&self) -> Arc<ChainSpec> {
        self.block_assembler.chain_spec.clone()
    }

    pub fn with_extra_data(mut self, extra_data: Vec<u8>) -> Self {
        self.block_assembler.extra_data = extra_data.into();
        self
    }
}

impl<F> BlockAssembler<F> for TaikoBlockAssembler
where
    F: for<'a> BlockExecutorFactory<
            ExecutionCtx<'a> = EthBlockExecutionCtx<'a>,
            Transaction = TransactionSigned,
            Receipt = Receipt,
        >,
{
    type Block = Block;

    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, F>,
    ) -> Result<Self::Block, BlockExecutionError> {
        // self.with_extra_data(input.execution_ctx.extra_data);
        // TODO: add extra data to the block header
        Ok(self
            .block_assembler
            .assemble_block(input)?
            .map_header(From::from))
    }
}
