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

The default build includes revmc JIT support and requires Rust 1.95 plus LLVM 22. On Ubuntu or
Debian, install LLVM with the same helper used by CI and Docker:

```bash
.github/scripts/install_llvm.sh ubuntu
```

Build by `Cargo`:

```bash
cargo build --release
```

The main binary will be located at `target/release/alethia-reth`.

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

### revmc JIT

Start the node with upstream-compatible JIT flags:

```bash
./target/release/alethia-reth node --jit [OPTIONS]
```

The main tuning flags are `--jit.hot-threshold`, `--jit.worker-count`,
`--jit.code-cache-bytes`, and `--jit.idle-evict-duration`. When the `reth` RPC namespace is
enabled, `reth_jit` accepts `enable`, `disable`, `pause`, `unpause`, or `clear` at runtime.

Taiko's Unzen execution uses consensus-critical zk-gas metering that requires per-opcode
interpreter hooks. JIT dispatch therefore falls back to the interpreter for Unzen blocks and
ordinary RPC call/trace execution. Pre-Unzen canonical execution, payload building, and block
replay use the shared revmc backend. Compiled code bakes in upstream mainnet gas and opcode
semantics, so hardforks are JIT-eligible only through an explicit allowlist: new forks stay
interpreter-only until they are deliberately marked JIT-safe in `TaikoEvmFactory`.

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
