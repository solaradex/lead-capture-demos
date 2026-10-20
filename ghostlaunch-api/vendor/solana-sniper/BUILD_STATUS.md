# Implemented vs unfinished

Implemented: config validation, safe observe defaults, JSONL replay source, normalized event/signal types, normalized creator-buy threshold strategy, basic risk/exit rules, process-local dedupe, execution interface, Jito sendBundle adapter for already-signed payloads, structured logging.

Not implemented: real Yellowstone gRPC, raw Solana/Pump decoders, official Pump account derivations and buy/sell instructions, signing, Jito status polling, landed/failed confirmation, on-chain balance reconciliation, persistent state, production token/liquidity audit, live trading.

This archive is a concrete starting codebase, not a complete production bot. Do not enable live execution.
