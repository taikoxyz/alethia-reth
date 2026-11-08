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
`engine_newPayloadV4`, for Unzen and Etna), payload normalization, the Etna fork rules, and devnet
configuration, see the [Etna Engine API guide](docs/engine-api-etna.md).

## Custom devnet genesis with Etna

No built-in genesis predeploys the EIP-4788 (`0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02`) or
EIP-2935 (`0x0000F90827F1C53a10cb7A02335B175320002935`) system contracts, and canonical devnet's
`alloc` still holds the pre-Etna `Anchor`. Do not only add the two system contracts to that alloc:
with Etna active, the old `anchorV4` stays callable with the public golden-touch key, so anyone
could save a forged L1 checkpoint. Build the genesis with `build_etna_genesis.py` from the draft
[taikoxyz/taiko-mono#22286](https://github.com/taikoxyz/taiko-mono/pull/22286), which is not on
taiko-mono `main` yet. In a checkout of that branch, set `chainId` to `167001`, `etnaTimestamp` to
`"0x0"` and `remoteSignalService` to a deployed L1 SignalService in
`packages/protocol/test/genesis/data/genesis_config.js`. Then run, from `packages/protocol` (pnpm
and Foundry required), with `ALETHIA_RETH` set to this repository and `L1_SIGNAL_SERVICE` to that
address:

```sh
pnpm install && pnpm genesis:gen
python3 script/layer2/build_etna_genesis.py \
  --base "$ALETHIA_RETH/crates/chainspec/src/genesis/devnet.json" \
  --predeploys test/genesis/data/genesis_alloc.json \
  --predeploy-config test/genesis/data/genesis_config.json \
  --l1-signal-service "$L1_SIGNAL_SERVICE" \
  --output "$ALETHIA_RETH/etna-devnet.json"
```

The script overlays the generated L2 predeploys (including the Etna `Anchor` and `SignalServiceL2`)
and both system contracts onto `alloc`, drops the derived `stateRoot`, and refuses an `Anchor` whose
`etnaTimestamp` is not 0. From this repository, start a fresh node with the
[Etna override](docs/engine-api-etna.md#devnet-activation-override) at that same timestamp, `0`:

```sh
./target/release/alethia-reth node --chain ./etna-devnet.json \
  --devnet-etna-timestamp 0 --datadir ./etna-devnet-data
```

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
