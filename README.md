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

_(Details about specific configuration files, environment variables, or command-line arguments required for typical operation will be added here as the project evolves. For now, please refer to the `--help` output of the binary.)_

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
