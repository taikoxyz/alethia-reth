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

### Common Runtime Flags

- `--datadir <path>` to set node data location.
- `--http` / `--ws` to enable RPC transports.
- `--authrpc.addr <ip>` and `--authrpc.port <port>` for Engine API auth RPC.
- `--metrics <addr:port>` to expose Prometheus metrics.

Use `./target/release/alethia-reth --help` for the full option list and defaults.

For the Engine API driver contract (only `engine_forkchoiceUpdatedV3`, `engine_getPayloadV5`, and
`engine_newPayloadV4`, for Unzen and Etna), payload normalization, the Etna fork rules, devnet
configuration, and release prerequisites, see the [Etna Engine API guide](docs/engine-api-etna.md).

## Custom devnet genesis with Etna

Start from [`crates/chainspec/src/genesis/devnet.json`](crates/chainspec/src/genesis/devnet.json)
and change only `alloc` to add predeploy code, balances, nonces, or storage. Every other parsed
Genesis field, including the complete `config` and its extra fields, must match canonical devnet.
Keep the existing config even where the canonical Taiko fork table supplies a different schedule.
For example, after saving an alloc-only variant as `custom-devnet.json`, start a fresh node with:

```sh
./target/release/alethia-reth node --chain ./custom-devnet.json \
  --devnet-etna-timestamp 0 --datadir ./custom-devnet-data
```

The explicit flag restores canonical devnet's Taiko and Ethereum forks (Unzen/Osaka at genesis)
and activates Etna at the requested Unix timestamp. `0` activates Etna at genesis; a nonzero value
keeps Unzen until that timestamp. The genesis header is rebuilt from that fork table and the
custom allocation, so changing `alloc` changes the state root and genesis hash. The built-in
`--chain devnet` genesis stays unchanged.

For Etna root/history system calls, include the canonical EIP-2935 and EIP-4788 runtime bytecode
at `0x0000f90827f1c53a10cb7a02335b175320002935` and
`0x000f3df6d732807ef1319fb7b8bb8522d0beac02`; the
[test fixture](crates/node/tests/fixtures/etna-genesis.json) contains both runtimes, but its other
Genesis fields are not an alloc-only devnet template. A vault predeploy also needs its runtime
and initialized storage in `alloc` (the devnet treasury address is
`0x1670010000000000000000000000000000010001`). System-call storage writes begin in block 1.

Without the flag, custom JSON follows the generic Ethereum parser and does not activate Taiko
Etna. Other networks and changes outside `alloc` retain their parsed schedule and log a warning
when the override is requested. Use a fresh data directory for a changed genesis; see the
[Etna Engine API guide](docs/engine-api-etna.md#devnet-activation-override) for driver requirements.

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
