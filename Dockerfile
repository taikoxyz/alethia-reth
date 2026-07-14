# Trixie rather than bookworm: apt.llvm.org publishes arm64 LLVM 22 packages only for trixie
# (the bookworm arm64 index carries just docs and WASM cross libs), and the runtime stage must
# share the build stage's glibc.
FROM rust:1.95.0-trixie AS build

WORKDIR /app

COPY .github/scripts/install_llvm_ubuntu.sh /tmp/install_llvm.sh

RUN /tmp/install_llvm.sh && rm /tmp/install_llvm.sh && \
  apt-get update && \
  apt-get -y upgrade && \
  apt-get install -y git libclang-dev pkg-config curl build-essential && \
  rm -rf /var/lib/apt/lists/*

COPY ./ .

RUN cargo build --release --locked

FROM debian:trixie-slim

RUN apt-get update && \
  apt-get install -y jq curl ca-certificates && \
  rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=build /app/target/release/alethia-reth ./
