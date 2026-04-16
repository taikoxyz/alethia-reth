## 背景

历史块 `debug_executionWitness` 在 archive 节点上仍然会明显退化，原因不是“数据被 prune 了”，而是
`reth` 的历史 witness/proof 路径会在每次请求时都从当前 DB tip 往回重建到目标块父状态。

对单块 RPC 来说，这条路径是：

1. 先拿目标块父状态重执行目标块。
2. 再调用历史 `StateProvider::witness()`。
3. `witness()` 内部先 `revert_state()`。
4. `revert_state()` 会从 `target_parent + 1 .. tip` 扫 changesets / hashed-state reverts。

因此，对连续窗口 `[start, end]` 来说，最贵的“从 tip 回退到 start 父块”这件事会被重复做很多次。

`raiko2` 当前正好是按连续窗口拉 witness，所以适合在节点侧做一个 range RPC，把最贵的历史回退摊薄到整段窗口。

## 目标

增加一个自定义 RPC：

`taiko_executionWitnessRange(start_block, end_block) -> Vec<ExecutionWitness>`

它的语义是：

- 输入是闭区间 `[start, end]`
- 输出按块号升序返回
- 只针对 canonical block
- 保留现有单块 `debug_executionWitness` 语义，不去改 upstream 行为

## 核心思路

range RPC 里把“执行态”和“witness/proof 态”拆开处理。

### 1. 执行态

执行时使用：

`reth_chain_state::MemoryOverlayStateProvider`

基准状态固定在 `start - 1` 这个父块上，然后把窗口里已经执行过的块结果叠在内存里。

这样执行 `start + 1`、`start + 2`、`...` 时，不需要每块都重新从磁盘构造中间状态。

### 2. Witness / Proof 态

对同一个 anchor，也就是 `start - 1`，只做一次最贵的历史回退准备：

- `changeset_cache.get_or_compute_range(start ..= tip)` 得到 trie reverts
- `reth_trie_db::from_reverts_auto(start ..)` 得到 hashed-state reverts

然后对窗口内每个块：

1. 复用这份共享的 base reverts
2. 再叠加“本窗口前面已经执行过的块”的 sorted hashed-state / trie updates
3. 最后只把“当前块”的 hashed post-state 当成 target 来算 witness

这样能把原本“每块一次 `start..tip` 回退”变成“每个窗口一次 `start..tip` 回退”。

## 每块执行流

对窗口里的单个块 `n`：

1. 用 anchor=`start - 1` 的历史状态，加上 `[start, n-1]` 的内存 overlay，重执行块 `n`
2. 记录本块执行期间访问到的 account/storage/code preimages
3. 基于“共享 base reverts + `[start, n-1]` overlay”生成块 `n` 的 witness
4. 计算块 `n` 的 state root / trie updates
5. 把块 `n` 的 execution output、sorted hashed-state、sorted trie updates 放进窗口 overlay，供下一块复用

## 为什么 archive 也需要这个优化

archive 能保证“历史状态可用”，但不代表“历史 witness 快”。

这里的问题不是 availability，而是历史 proof/witness 的构造复杂度：

- archive 解决的是“有没有数据”
- 这次优化解决的是“每次是不是都从 tip 重新回退”

所以即便只考虑 archive，range RPC 仍然有意义。

## 代码落点

- `crates/rpc/src/eth/eth.rs`
  - 增加 `taiko_executionWitnessRange`
  - 实现共享 historical reverts、窗口 overlay、逐块 witness 生成
- `bin/alethia-reth/src/main.rs`
  - 给 `taiko_` namespace 注入 `evm_config`
  - 注入一份本地 `ChangesetCache`

## 预期收益

如果 `raiko2` 拉的是连续窗口，那么这版 RPC 的收益主要来自：

- 把最贵的历史回退从“按块重复”改成“按窗口复用”
- 避免窗口内每个块都重新走完整的历史 witness 热路径
- 让连续块执行天然复用前面块的执行结果

它不能让 witness 变成“便宜操作”，但应该能显著改善当前这种离 tip 几千块也要等很多分钟的情况。

## 风险和边界

- 大窗口仍然可能重，优化是摊薄，不是免费
- 这条 RPC 假设请求的是连续 canonical blocks
- 窗口 overlay 的块顺序必须保持正确，否则后续执行态会错
- 如果计算出的 state root 和区块头不一致，说明 overlay 组合逻辑有 bug，必须直接报错

## 验证

- `cargo check -p alethia-reth-rpc`
- focused unit tests：range 参数校验、header 附带逻辑
- `cargo test -p alethia-reth-rpc`
