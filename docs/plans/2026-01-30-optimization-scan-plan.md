# Optimization Scan Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Remove remaining runtime panic risks and reduce avoidable allocations/clones similar to 0e2bde4 and f765312.

**Architecture:** Focus on small, localized refactors: make base-fee computation explicitly depend on a validated parent base fee, and preallocate buffers where encoded sizes are known. Avoid changing protocol behavior or public APIs beyond internal helper signatures.

**Tech Stack:** Rust, reth, alloy-rlp, Taiko consensus/block/payload crates.

### Task 1: Make EIP-4396 base fee calculation explicit and non-panicking

**Files:**
- Modify: `crates/consensus/src/eip4396.rs`
- Modify: `crates/consensus/src/validation.rs`
- Test: `crates/consensus/src/validation.rs`

**Step 1: Add/adjust test to match new signature**

```rust
// In crates/consensus/src/validation.rs test module
let parent_base_fee = parent.base_fee_per_gas.unwrap();
let base_fee = calculate_next_block_eip4396_base_fee(&parent, BLOCK_TIME_TARGET, parent_base_fee);
```

**Step 2: Run test to verify it fails (signature mismatch)**

Run: `cargo test -p alethia-reth-consensus`
Expected: FAIL with signature mismatch for `calculate_next_block_eip4396_base_fee`

**Step 3: Update calculation signature and callers**

```rust
// crates/consensus/src/eip4396.rs
pub fn calculate_next_block_eip4396_base_fee<H: BlockHeader>(
    parent: &H,
    parent_block_time: u64,
    parent_base_fee_per_gas: u64,
) -> u64 {
    // ... use parent_base_fee_per_gas instead of unwrap
}

// crates/consensus/src/validation.rs
let parent_base_fee =
    parent.header().base_fee_per_gas().ok_or(ConsensusError::BaseFeeMissing)?;
let expected_base_fee = if parent.number() == 0 {
    SHASTA_INITIAL_BASE_FEE
} else {
    parent_block_time(self.block_reader.as_ref(), parent)
        .map(|block_time| {
            calculate_next_block_eip4396_base_fee(parent.header(), block_time, parent_base_fee)
        })
        .unwrap_or(header_base_fee)
};
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p alethia-reth-consensus`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/consensus/src/eip4396.rs crates/consensus/src/validation.rs
git commit -m "fix(consensus): avoid base fee unwrap in EIP-4396 calc"
```

### Task 2: Preallocate RLP buffer in zlib size estimation

**Files:**
- Modify: `crates/block/src/tx_selection.rs`
- Test: `crates/block/src/tx_selection.rs`

**Step 1: (No new tests) Use existing unit coverage**

No new tests required; change is allocation-only.

**Step 2: Implement preallocation using list_length**

```rust
use alloy_rlp::{encode_list, list_length};

let mut rlp_bytes = Vec::with_capacity(list_length(&txs));
encode_list::<&TransactionSigned, TransactionSigned>(&txs, &mut rlp_bytes);
```

**Step 3: Run tests**

Run: `cargo test -p alethia-reth-block`
Expected: PASS

**Step 4: Commit**

```bash
git add crates/block/src/tx_selection.rs
git commit -m "perf(block): preallocate RLP buffer for zlib sizing"
```

### Task 3: Preallocate withdrawals encoding buffer and remove slice expect

**Files:**
- Modify: `crates/primitives/src/payload/builder.rs`
- Test: `crates/primitives/src/payload/builder.rs`

**Step 1: (No new tests) Use existing unit coverage**

No new tests required; change is allocation/unwrap removal only.

**Step 2: Implement preallocation and infallible ID extraction**

```rust
if let Some(withdrawals) = &attributes.payload_attributes.withdrawals {
    let mut buf = Vec::with_capacity(withdrawals.length());
    withdrawals.encode(&mut buf);
    hasher.update(buf);
}

let mut out = hasher.finalize();
out[0] = payload_version;
let mut id_bytes = [0u8; 8];
id_bytes.copy_from_slice(&out[..8]);
PayloadId::new(id_bytes)
```

**Step 3: Run tests**

Run: `cargo test -p alethia-reth-primitives`
Expected: PASS

**Step 4: Commit**

```bash
git add crates/primitives/src/payload/builder.rs
git commit -m "perf(primitives): preallocate withdrawals RLP and avoid expect"
```

