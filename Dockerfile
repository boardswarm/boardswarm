ARG BINARY=boardswarm

## stage 1: builder
FROM rust:slim-bookworm AS builder
ARG BINARY
ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update && \
    apt-get install --yes \
      pkg-config \
      protobuf-compiler \
      libudev-dev \
      libssl-dev && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/boardswarm

COPY . .

RUN cargo build \
      --release \
      --bin $BINARY

## stage 2: runner
FROM debian:bookworm-slim
ARG BINARY
ARG DEBIAN_FRONTEND=noninteractive
ENV RUST_LOG=info

EXPOSE 16421

LABEL org.opencontainers.image.title="boardswarm"
LABEL org.opencontainers.image.description="gRPC server exposing APIs useful for interacting with development boards"
LABEL org.opencontainers.image.source="https://github.com/boardswarm/boardswarm"

RUN apt update && \
    apt install --yes \
      libudev1 \
      libssl3 && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/boardswarm/target/release/$BINARY /usr/local/bin/$BINARY

ENTRYPOINT ["/usr/local/bin/$BINARY"]
