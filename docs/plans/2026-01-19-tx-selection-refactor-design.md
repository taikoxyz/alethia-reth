# Transaction Selection Refactor Design

## Overview

Unify the overlapping transaction selection logic between `execute_anchor_and_pool_transactions` (payload builder) and `tx_pool_content_with_min_tip` (RPC endpoint) into a shared module in `crates/block`.

## Problem

Both functions contain nearly identical logic for:
- Fetching best transactions from pool
- Gas limit checking
- DA size estimation and checking (using `tx_estimated_size_fjord_bytes`)
- Transaction execution via `BlockBuilder`
- Error classification (nonce too low vs other validation errors vs fatal)
- Marking invalid transactions in the pool iterator

Differences that can be unified:
- Cancel checking: builder has it, RPC doesn't → RPC can pass `|| false`
- Multiple lists: RPC supports N lists, builder uses 1 → builder is `max_lists=1`
- Locals/min_tip filtering: RPC has it, builder doesn't → builder uses `None`/`0`

## Solution

### New Module: `crates/block/src/tx_selection.rs`

#### Data Structures

```rust
/// Configuration for transaction selection
pub struct TxSelectionConfig {
    pub base_fee: u64,
    pub gas_limit_per_list: u64,
    pub max_da_bytes_per_list: u64,
    pub max_lists: usize,
    pub min_tip: u64,
    pub locals: Option<Vec<Address>>,
}

/// A successfully executed transaction
pub struct ExecutedTx {
    pub tx: Recovered<TransactionSigned>,
    pub gas_used: u64,
    pub da_size: u64,
}

/// A list of executed transactions with cumulative stats
#[derive(Default)]
pub struct ExecutedTxList {
    pub transactions: Vec<ExecutedTx>,
    pub total_gas_used: u64,
    pub total_da_bytes: u64,
}

/// Outcome of the selection process
pub enum SelectionOutcome {
    Cancelled,
    Completed(Vec<ExecutedTxList>),
}
```

#### Main Function

```rust
pub fn select_and_execute_pool_transactions<B, Pool>(
    builder: &mut B,
    pool: &Pool,
    config: &TxSelectionConfig,
    is_cancelled: impl Fn() -> bool,
) -> Result<SelectionOutcome, BlockExecutionError>
where
    B: BlockBuilder<Primitives = EthPrimitives>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
```

### Implementation Logic

1. Get best transactions iterator from pool
2. For each transaction:
   - Check cancellation
   - Filter by locals (if configured)
   - Filter by min_tip (if > 0)
   - Check gas limit for current list, start new list if needed
   - Check DA size for current list, start new list if needed
   - Execute transaction
   - Classify execution result (success/skip/invalid/fatal)
   - Record successful transactions

## Changes Required

### 1. `crates/block/Cargo.toml`
Add dependencies:
- `op-alloy-flz` (for `tx_estimated_size_fjord_bytes`)
- `reth-transaction-pool` (for `TransactionPool`, `BestTransactionsAttributes`, etc.)

### 2. `crates/block/src/tx_selection.rs`
New file with the shared implementation.

### 3. `crates/block/src/lib.rs`
Export the new module:
```rust
pub mod tx_selection;
```

### 4. `crates/payload/src/builder.rs`
Refactor `execute_anchor_and_pool_transactions`:
- Keep anchor transaction execution (before calling shared function)
- Call `select_and_execute_pool_transactions` with `max_lists=1`
- Calculate `total_fees` from returned `ExecutedTxList`

### 5. `crates/rpc/src/eth/auth.rs`
Refactor `tx_pool_content_with_min_tip`:
- Call `select_and_execute_pool_transactions` with full config
- Convert `Vec<ExecutedTxList>` to `Vec<PreBuiltTxList<RpcTransaction>>`

## Usage Examples

### Payload Builder (builder.rs)
```rust
let config = TxSelectionConfig {
    base_fee: ctx.base_fee,
    gas_limit_per_list: ctx.gas_limit,
    max_da_bytes_per_list: BYTES_PER_BLOB as u64,
    max_lists: 1,
    min_tip: 0,
    locals: None,
};

// Execute anchor first (unique to builder)
builder.execute_transaction(ctx.anchor_tx.clone())?;

// Then select pool transactions
match select_and_execute_pool_transactions(&mut builder, &pool, &config, || cancel.is_cancelled())? {
    SelectionOutcome::Cancelled => return Ok(ExecutionOutcome::Cancelled),
    SelectionOutcome::Completed(lists) => {
        let total_fees = lists[0].transactions.iter()
            .map(|etx| {
                let tip = etx.tx.effective_tip_per_gas(ctx.base_fee).unwrap();
                U256::from(tip) * U256::from(etx.gas_used)
            })
            .sum();
        // ...
    }
}
```

### RPC Endpoint (auth.rs)
```rust
let config = TxSelectionConfig {
    base_fee,
    gas_limit_per_list: block_max_gas_limit,
    max_da_bytes_per_list: max_bytes_per_tx_list,
    max_lists: max_transactions_lists as usize,
    min_tip,
    locals,
};

match select_and_execute_pool_transactions(&mut builder, &self.pool, &config, || false)? {
    SelectionOutcome::Completed(lists) => {
        lists.into_iter()
            .map(|list| PreBuiltTxList {
                tx_list: list.transactions.into_iter()
                    .map(|etx| self.tx_resp_builder.fill_pending(etx.tx))
                    .collect::<Result<_, _>>()?,
                estimated_gas_used: list.total_gas_used,
                bytes_length: list.total_da_bytes,
            })
            .collect()
    }
    SelectionOutcome::Cancelled => unreachable!(),
}
```

## Benefits

1. **Single source of truth** for transaction selection logic
2. **Consistent behavior** between payload building and RPC pre-building
3. **Easier maintenance** - fix bugs or add features in one place
4. **Clear abstraction** - differences are expressed through configuration, not code duplication
