toolchain := "1.94.1"
fmt_toolchain := "nightly"

fmt:
  rustup toolchain install {{fmt_toolchain}} --component rustfmt && \
  cargo +{{fmt_toolchain}} fmt && \
  cargo sort --workspace --grouped

fmt-check:
  rustup toolchain install {{fmt_toolchain}} --component rustfmt && \
  cargo +{{fmt_toolchain}} fmt --check

clippy:
  rustup toolchain install {{toolchain}} && \
  cargo +{{toolchain}} clippy --workspace --all-features --no-deps -- -D warnings -D missing_docs -D clippy::missing_docs_in_private_items

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
