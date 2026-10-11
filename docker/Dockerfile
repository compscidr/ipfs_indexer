FROM rust:1-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# optional: commit shown in /version and /status (.git isn't in the build context)
ARG GIT_SHA=""
RUN GIT_SHA="$GIT_SHA" cargo build --release

FROM debian:bookworm-slim
# rustls verifies against the system trust store
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/ipfs_indexer /usr/local/bin/ipfs_indexer
# localhost inside a container is unreachable, so listen on all interfaces
ENV BIND_ADDR=0.0.0.0:9090
EXPOSE 9090
# args (optional): gateway, e.g. `ipfs:8080` for a local node
ENTRYPOINT ["/usr/local/bin/ipfs_indexer"]
