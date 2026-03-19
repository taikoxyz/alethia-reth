FROM rust:1.93.1-bookworm AS build

WORKDIR /app

RUN apt-get update && \
  apt-get -y upgrade && \
  apt-get install -y git libclang-dev pkg-config curl build-essential && \
  rm -rf /var/lib/apt/lists/*

COPY ./ .

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && \
  apt-get install -y jq curl ca-certificates && \
  rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=build /app/target/release/alethia-reth ./
