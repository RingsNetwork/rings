# syntax=docker/dockerfile:1.7

FROM rust:1.94-trixie AS builder

WORKDIR /src
COPY Cargo.lock Cargo.toml README.md rust-toolchain.toml ./
COPY crates ./crates
COPY examples ./examples

RUN cargo build -p rings-node --bin rings --features node --no-default-features --release

FROM debian:trixie-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        tini \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/rings /usr/local/bin/rings
COPY docker/cluster/rings-cluster-entrypoint.sh /usr/local/bin/rings-cluster

RUN chmod 0755 /usr/local/bin/rings /usr/local/bin/rings-cluster

ENV RINGS_ALLOW_RANDOM_KEYS=true \
    RINGS_BASE_EXTERNAL_PORT=51000 \
    RINGS_BASE_INTERNAL_PORT=50000 \
    RINGS_CLUSTER_DIR=/var/lib/rings-cluster \
    RINGS_CONNECT_TOPOLOGY=ring \
    RINGS_ICE_SERVERS=stun://stun.l.google.com:19302 \
    RINGS_LOG_LEVEL=info \
    RINGS_NETWORK_ID=1 \
    RINGS_NODE_COUNT=3 \
    RINGS_READY_RETRIES=60 \
    RINGS_READY_SLEEP_SECONDS=1 \
    RINGS_RUNTIME=current-thread \
    RINGS_SESSION_TTL_SECONDS=2592000 \
    RINGS_STABILIZE_INTERVAL=3

EXPOSE 51000-51018/tcp

ENTRYPOINT ["tini", "--", "/usr/local/bin/rings-cluster"]
