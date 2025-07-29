# Claude Context for Taiko-Reth

This document provides context and guidelines for Claude when working on the taiko-reth project.

## Project Overview

Taiko-reth is a high-performance Rust execution client for the Taiko protocol, built on top of Reth's powerful NodeBuilder API. It's designed to deliver the best possible developer and maintenance experience for running Taiko nodes.

## Key Technologies

- **Language**: Rust (requires version 1.87+)
- **Framework**: Built on [Reth](https://github.com/paradigmxyz/reth) v1.6.0
- **Protocol**: Taiko (Ethereum-based rollup)
- **Build System**: Cargo

## Project Structure

```
taiko-reth/
├── src/
│   ├── bin/           # Binary entry point
│   ├── block/         # Block processing logic
│   ├── chainspec/     # Chain specifications
│   ├── cli/           # Command-line interface
│   ├── consensus/     # Consensus logic
│   ├── db/            # Database models and compression
│   ├── evm/           # EVM configuration and execution
│   ├── network/       # P2P networking
│   ├── payload/       # Payload building and attributes
│   └── rpc/           # RPC API implementations
├── .claude/
│   └── commands/      # Custom Claude commands
└── target/            # Build artifacts
```

## Development Guidelines

### Code Style
- Follow Rust idioms and best practices
- Use the project's existing patterns and conventions
- No unnecessary comments unless explicitly requested
- Maintain consistent formatting with `rustfmt`

### Testing
- Run tests with `cargo test`
- Ensure all tests pass before marking work as complete
- Add tests for new functionality when appropriate

### Building
- Debug build: `cargo build`
- Release build: `cargo build --release`
- The main binary is located at `target/release/taiko-reth`

### Common Commands
- `cargo check` - Check if the project compiles
- `cargo test` - Run all tests
- `cargo fmt` - Format code
- `cargo clippy` - Run linter

## Important Considerations

1. **Performance**: This is a high-performance client, so efficiency matters
2. **Security**: Never expose or log secrets/keys
3. **Compatibility**: Ensure changes maintain compatibility with Reth APIs
4. **Error Handling**: Use proper Rust error handling patterns

## Custom Commands

The project includes custom Claude commands in `.claude/commands/`:
- `improve-pr-desc.md` - Improve GitHub PR descriptions

## Dependencies

Key dependencies from Cargo.toml:
- Reth crates (v1.6.0)
- Alloy libraries for Ethereum types
- Tokio for async runtime
- Clap for CLI parsing

## Network Support

The project supports multiple Taiko networks:
- Mainnet
- Hekla (testnet)
- Devnet

Genesis configurations are stored in `src/chainspec/genesis/`.