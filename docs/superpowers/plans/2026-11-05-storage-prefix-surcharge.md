# Simple Storage Prefix Surcharge Implementation Plan

> For agentic workers: use subagent-driven-development or executing-plans for the scoped tasks below.

**Goal:** Carry the accepted conservative low32 comparison envelope into node-observable storage work and validate a simple tariff without reproducing guest hash layouts.

**Architecture:** Count transaction-local storage materialization attempts by owner and raw-key low32 group, reusing cached-slot facts already captured by the authoritative interpreter hook. Seed any pre-existing physical storage keys once at transaction start. Retain all computational work across reverts and clears and reset at the transaction boundary. Counts may conservatively exceed distinct-key counts; no new full-key dedup lookup is required.

**Tech Stack:** Existing Rust zkGas accumulator/runtime hooks; retained SP1 GasVM observer and offline Python validators.

**Spec:** Accepted one-prefix research envelope: U=4*N+28*M and C=ceil((N+7*M)/8). Here production N/M are upper counts of materializations, including initial slots and potential failed new-slot attempts, rather than exact distinct-ever-seen counts. These upper counts conservatively contain each physical candidate set. The existing research component proof and calibration limits remain applicable.

## Global Constraints

- Keep the existing model/tariffs/Unzen execution unchanged until the missing short-prefix calibration has been checked.
- Use the existing maintained shadow-pricing worktree and primary runtime path. Preserve dirty primary checkout files.
- Do not build guest/SDK/ELF merely for host observer changes. Do not deploy or merge.
- Keep normal repeated-key accesses cheap: reuse cached slot presence; update prefix groups only for potential new materializations.
- Initial journal storage must be seeded once; never infer all work from final changed state or final physical keys.
- Use checked integer arithmetic and explicit independent diagnostic incompleteness; observation failure must not alter existing pricing/execution.
- Maintain a 576 MiB free-disk floor. Coordinate compiler output with the sampling process; no broad Cargo/guest rebuild without measured capacity.
- Treat candidate arithmetic, new native quotes, and actual SP1 PGU as distinct evidence.

## Review Focus

- Already cached slots at transaction start, then clear/refill: initial entries and every new attempt remain included.
- Reverted warmness and failed/skip-cold accesses: counts may overestimate, never silently exclude recorded work.
- Same owner in different transactions: reset per-owner groups and counters; aggregate only completed scopes.
- Legacy rejection after an executed opcode and genuine inspector execution: preserve existing observation timing and path parity.
- Short-prefix hash concentration: byte-envelope math alone does not certify fixed calls, probes, growth or the PGU coefficient.

## Task 1: Cheap native diagnostics

Files: `crates/evm/src/zk_gas/pricing/storage_journal.rs`, `runtime.rs`, `pricing/accumulator.rs`, the transaction-start wiring in `alloy.rs`, related tests, and the pricing documentation section.

- [x] Add focused failing tests for distinct-prefix/identical-prefix inputs, warm repeats, conservative repeated vacant attempts, initial cached slots, clear/refill, rollback, scope reset, overflow and Off/plain/inspected parity.
- [x] Extend the existing unpriced ledger with materialization upper counts and one ordered low32 group counter per owner; retain existing occupancy diagnostics.
- [x] Reuse the existing pre-step cached-slot presence fact; do not add storage-key database/HashMap lookups.
- [x] Seed initial physical keys once while observing, without an extra DB load. Include only materialized initial slots, not a witness/global-resource surrogate.
- [x] At scope finish compute checked C and C*(A+N+J), with J the existing cold-potential-plus-SSTORE reserve. Export enough typed diagnostics to audit the input counts.
- [x] Independently review the complete diff and independently test meaningful lifecycle/integration paths. Run resource-bounded focused checks and report any full-Cargo limitation.

## Task 2: Minimal missing calibration

Files: maintained benchmark catalog helper and tests, retained data-only fixtures/evidence and report.

- [ ] Construct a bounded short-prefix but hash-concentrated catalog; keep the selected runtime, ELF and SDK fixed.
- [ ] Verify bytecode intent, honest minimal storage/code input and native/public-output parity before one actual measurement.
- [ ] Use the existing bounded sampler and source/input fingerprints; no implicit retry, giant trace load, or relaxed memory/disk limits.
- [ ] Read back the actual guest seed/layout realization, actual PGU and literal old/new node estimates. Preserve negative or incomplete realization as such.

## Task 3: Apply a simple tariff only after evidence supports it

- [ ] Select a conservative coefficient using concentrated long/short-prefix and insertion/rollback cases; retain normal historical large-table compatibility checks.
- [ ] Generate the model successor through the authoritative model generator; do not hand-edit artifacts.
- [ ] Integrate the computed units into node-known cost with the existing transaction commit/reject/fallback semantics.
- [ ] Verify retained native regression data and clearly distinguish an actual repriced model from offline candidate arithmetic.
- [ ] Complete independent review/testing, then update the maintained PR and working log with the exact evidence and remaining limits. Do not merge/deploy.

## Checkpoint

Task 1 passed 202 complete EVM source tests, 10 focused ledger tests, strict production metadata/Clippy/docs checks, independent source review, and five independent actual-EVM lifecycle oracles. These are cached-extern default/std checks, not a locked full-workspace/all-features or guest build certification. Task 2 completed one short-prefix development SP1 execution: 831,917,069 normalized PGU; the old node quote was 159,815,076. The p256 candidate still underquoted by101,266,793. The simple coefficient policy (maximum required over zero/long/short cases, times5/4, rounded upward to64) selects384; its short-case candidate is1,016,067,876. Formal model integration and historical repricing remain outstanding; no tariff has changed at this checkpoint.
