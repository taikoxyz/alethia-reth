toolchain := "1.95.0"
fmt_toolchain := "nightly"

fmt:
  rustup toolchain install {{fmt_toolchain}} --component rustfmt && \
  cargo +{{fmt_toolchain}} fmt && \
  cargo +{{fmt_toolchain}} fmt --manifest-path vendor/revmc/Cargo.toml --all && \
  cargo sort --workspace --grouped

fmt-check:
  rustup toolchain install {{fmt_toolchain}} --component rustfmt && \
  cargo +{{fmt_toolchain}} fmt --check && \
  cargo +{{fmt_toolchain}} fmt --manifest-path vendor/revmc/Cargo.toml --all --check

clippy:
  rustup toolchain install {{toolchain}} && \
  cargo +{{toolchain}} clippy --workspace --all-features --no-deps -- -D warnings -D missing_docs -D clippy::missing_docs_in_private_items && \
  cargo +{{toolchain}} clippy --locked --manifest-path vendor/revmc/Cargo.toml -p revmc-runtime -p revmc-codegen --features llvm-prefer-static -- -D warnings

clippy-fix:
  rustup toolchain install {{toolchain}} && \
  cargo +{{toolchain}} clippy --fix --workspace --all-features --no-deps --allow-dirty --allow-staged -- -D warnings

udeps:
  rustup toolchain install nightly && \
  cargo +nightly udeps --all-targets

test:
  rustup toolchain install {{toolchain}} && \
  cargo +{{toolchain}} nextest -v run \
    --workspace --all-features && \
  cargo +{{toolchain}} test --locked --manifest-path vendor/revmc/Cargo.toml \
    -p revmc-runtime --features llvm-prefer-static -- --test-threads=1
