FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml ./
COPY src ./src
COPY fixtures ./fixtures
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/solana-sniper /usr/local/bin/solana-sniper
COPY fixtures ./fixtures
ENV RUN_MODE=observe EXECUTION_ENABLED=false REPLAY_FILE=fixtures/events.jsonl
CMD ["solana-sniper"]
