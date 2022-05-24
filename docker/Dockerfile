FROM ekidd/rust-musl-builder:stable as builder
# without this step, build fails with an os error 13 permission denied
# https://blog.logrocket.com/packaging-a-rust-web-service-using-docker/
RUN USER=root cargo new --bin ipfs_indexer
WORKDIR /home/rust/src/ipfs_indexer
COPY ./Cargo.lock ./Cargo.lock
COPY ./Cargo.toml ./Cargo.toml
COPY ./src ./src

RUN cargo build --release
RUN rm ./target/release/deps/* && rm src/*.rs

FROM alpine:3.16 as indexer
COPY --from=builder /home/rust/src/ipfs_indexer/target/x86_64-unknown-linux-musl/release/ipfs_indexer /usr/local/bin/ipfs_indexer
CMD /usr/local/bin/ipfs_indexer ipfs:8080