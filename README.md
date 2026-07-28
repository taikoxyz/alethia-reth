# alethia-reth

[![CI](https://github.com/taikoxyz/alethia-reth/actions/workflows/ci.yml/badge.svg)](https://github.com/taikoxyz/alethia-reth/actions/workflows/ci.yml)

A high-performance Rust execution client for the Taiko protocol, built on top of [Reth](https://github.com/paradigmxyz/reth) powerful [`NodeBuilder` API](https://reth.rs/introduction/why-reth#infinitely-customizable), designed to deliver the best possible developer and maintenance experience.

## Getting Started

### 1. Clone the Repository

```bash
git clone https://github.com/taikoxyz/alethia-reth.git
cd alethia-reth
```

### 2. Build

Build by `Cargo`:

```bash
cargo build --release
```

The main binary will be located at `target/release/alethia-reth`.

The default build includes revmc JIT support and requires Rust 1.95 plus LLVM 22. On Ubuntu or
Debian, install LLVM with the same helper used by CI and Docker:

```bash
.github/scripts/install_llvm.sh ubuntu
```

On macOS, `brew install llvm@22` and put its `bin` directory on `PATH` while building. To build
without the LLVM toolchain dependency, disable default features:

```bash
cargo build --release -p alethia-reth-bin --no-default-features
```

### 3. Run Checks and Tests

To ensure everything is set up correctly, run the checks and tests:

```bash
just test
```

## Running the Node

To run the compiled node:

```bash
./target/release/alethia-reth [OPTIONS]
```

To see available command-line options and subcommands, run:

```bash
./target/release/alethia-reth --help
```

_(Note: Replace `[OPTIONS]` with the necessary configuration flags for your setup. Refer to the `--help` output for details.)_

### revmc JIT

Start the node with upstream-compatible JIT flags:

```bash
./target/release/alethia-reth node --jit [OPTIONS]
```

The main tuning flags are `--jit.hot-threshold`, `--jit.worker-count`,
`--jit.code-cache-bytes`, and `--jit.idle-evict-duration`. When the `reth` RPC namespace is
enabled, the upstream `reth_jit` method accepts `enable`, `disable`, `pause`, `unpause`, or
`clear` at runtime.

Taiko's Unzen execution uses consensus-critical zk-gas metering that requires per-opcode
interpreter hooks. JIT dispatch therefore falls back to the interpreter for Unzen blocks and
for ordinary RPC call, trace, simulation, estimation, and pending-block execution. Canonical
Engine execution, payload building, and block replay opt in to the shared revmc backend on
pre-Unzen forks. Compiled code bakes in upstream mainnet gas and opcode semantics, so hardforks
are JIT-eligible only through an explicit allowlist: new forks stay interpreter-only until they
are deliberately marked JIT-safe in `TaikoEvmFactory`.

revmc is pinned in `Cargo.toml` to the exact revision the reth pin locks, which carries the
upstream compiled-vs-interpreter divergence fixes revmc#394 (stack sync for diverging builtins)
and revmc#400 (LOG memory operands in gas analysis) on top of revmc#391 (non-blocking runtime
controls) and revmc#395 (dynamic-gas failure ordering). Keep the two pins matched when bumping
reth.

## Docker

### 1. Build the Docker Image

```bash
docker build -t alethia-reth .
```

### 2. Run the Docker Container

```bash
docker run -it --rm alethia-reth [OPTIONS]
```

_(Note: You might need to map ports (`-p`), mount volumes (`-v`) for data persistence, or pass environment variables (`-e`) depending on your node's configuration needs.)_

## Configuration

Alethia-reth uses reth-compatible CLI options plus Taiko chain presets.

### Chain Selection

Use `--chain` with one of the supported presets:
- `mainnet`
- `taiko-hoodi`
- `devnet`
- `masaya`

### Common Runtime Flags

- `--datadir <path>` to set node data location.
- `--http` / `--ws` to enable RPC transports.
- `--authrpc.addr <ip>` and `--authrpc.port <port>` for Engine API auth RPC.
- `--metrics <addr:port>` to expose Prometheus metrics.

Use `./target/release/alethia-reth --help` for the full option list and defaults.

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
