toolchain := "1.95.0"
fmt_toolchain := "nightly"

# `cargo sort` lists the Alethia crates explicitly so it skips the vendored
# reth-optimism-trie manifest, which keeps upstream's dependency grouping (mirrors the
# vendored-crate clippy exemption below).
fmt:
  rustup toolchain install {{fmt_toolchain}} --component rustfmt && \
  cargo +{{fmt_toolchain}} fmt && \
  cargo sort --grouped . bin/alethia-reth crates/*

fmt-check:
  rustup toolchain install {{fmt_toolchain}} --component rustfmt && \
  cargo +{{fmt_toolchain}} fmt --check

# The vendored reth-optimism-trie crate keeps upstream's documentation style, so it is linted
# without the missing-docs gates that apply to Alethia's own crates.
clippy:
  rustup toolchain install {{toolchain}} && \
  cargo +{{toolchain}} clippy --workspace --exclude reth-optimism-trie --all-features --no-deps -- -D warnings -D missing_docs -D clippy::missing_docs_in_private_items && \
  cargo +{{toolchain}} clippy -p reth-optimism-trie --all-features --no-deps -- -D warnings

clippy-fix:
  rustup toolchain install {{toolchain}} && \
  cargo +{{toolchain}} clippy --fix --workspace --all-features --no-deps --allow-dirty --allow-staged -- -D warnings

udeps:
  rustup toolchain install nightly && \
  cargo +nightly udeps --all-targets

test:
  rustup toolchain install {{toolchain}} && \
  cargo +{{toolchain}} nextest -v run \
    --workspace --all-features
