# Solana Sniper — observe-first rebuild

This archive is a structured Rust foundation, **not a production-ready live trading bot**. It implements configuration validation, normalized event types, replay ingestion, creator-buy strategy evaluation, basic risk checks, event deduplication, an execution interface, and a Jito adapter that accepts already-signed transaction payloads.

It deliberately does NOT include live Yellowstone subscriptions, raw Pump instruction decoding, a wallet signer, validated buy/sell transaction builders, Jito status polling, on-chain balance reconciliation, or durable position storage. No private key is loaded. Live mode is refused.

## Run
```bash
cp .env.example .env
cargo test
cargo run
```

Replay fixture events are illustrative normalized events, not real blockchain data.

## Architecture
Yellowstone adapter (TODO) -> normalized events -> strategy -> risk policy -> observe/paper decision -> execution interface. The replay provider is currently the only working event source.

## Before live execution
Implement and test the real Yellowstone subscription and reconnect policy; raw Solana transaction decoding including v0 address lookup tables and inner instructions; current Pump account layouts, fees, buy/sell builders; isolated signing; Jito bundle status/landing verification; actual fill and balance reconciliation; persistent idempotency and position state; paper trading and controlled integration tests.

Never place private keys in `.env.example`, logs, Git, or Docker images. A bundle ID or submission acknowledgement is not proof of a fill.
