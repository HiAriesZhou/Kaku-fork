FROM rust:1.95-slim AS builder
WORKDIR /app

# Copy only the relay crate (standalone, no workspace deps). The lockfile
# pins the same dependency versions CI checks.
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/kaku-relay /usr/local/bin/kaku-relay
ENV RUST_LOG=kaku_relay=info
EXPOSE 8080
CMD ["kaku-relay"]
