# GhostLaunch Radar — post-launch monitoring

Radar watches a token's bonding curve after launch and answers the three
questions every creator and platform asks: **who sniped it, are whales
moving in, and how is the holder base shaping up?**

It is **read-only**: it fetches signatures + transactions via JSON-RPC and
decodes Pump `TradeEvent`s from program logs. It never submits
transactions and never needs private keys.

## How it works

1. **Find the curve**: derive the bonding-curve PDA for the mint
   (`decode::bonding_curve_pda` — same derivation the bundler uses).
2. **Pull the tape**: `getSignaturesForAddress` on the curve (newest
   first), then `getTransaction` per signature. Failed transactions are
   skipped.
3. **Decode trades**: every `Program data: <base64>` log line is tested
   against the TradeEvent discriminator; matches for this mint become
   trade records (wallet, buy/sell, SOL, tokens, slot, block time).
4. **Creation slot**: the bonding-curve account is born in the create
   transaction, so the *oldest* signature touching the curve is the true
   creation slot — more accurate than the first trade's slot.
5. **Analyze**: sniper/whale flags + holder math (see heuristics below).

## Detection heuristics

| Signal | Rule | Default | Rationale |
|--------|------|---------|-----------|
| Sniper | buy within `sniper_slot_window` slots of creation, excluding known wallets | 2 slots | Bundle launches land create+buys in the same slot; a buy in block 0–2 is overwhelmingly a bot, not a human |
| Whale | single buy ≥ `whale_threshold` | 5 SOL | Configurable per token |
| Whale sell | single sell ≥ `whale_threshold` | 5 SOL | Dump warning |

Known wallets (creator, initial buyers) are passed in by the caller and are
**never** flagged as snipers — they're echoed in the report for transparency.

Holder analytics: net token balance per wallet (buys add, sells subtract),
top-10 holders with share in basis points, total holder count, and a
cumulative holder-growth series (first-seen block time → holder count).

## Library (`src/radar.rs`)

```rust
let cfg = radar::RadarConfig {
    rpc_url: "https://devnet.helius-rpc.com/?api-key=<key>".into(),
    sniper_slot_window: 2,
    whale_threshold_lamports: 5 * radar::LAMPORTS_PER_SOL,
    max_signatures: 1000,
};
let known: HashSet<Pubkey> = [creator_pubkey].into_iter().collect();
let report: radar::RadarReport = radar::snapshot(&cfg, &mint, &known).await?;
```

`fetch_trades` + `analyze_trades` are public separately if you want to
cache the tape and re-analyze with different thresholds.

## API (v0.3.0)

All radar routes sit behind the same `X-API-Key` auth as the bundle routes
(`GET /v1/health` stays exempt).

### GET /v1/radar/{mint} — on-demand snapshot

Query params (all optional):

| Param | Default | Meaning |
|-------|---------|---------|
| `rpc_url` | devnet default | RPC endpoint (must be reachable from the server) |
| `sniper_slots` | 2 | sniper window in slots |
| `whale_sol` | 5.0 | whale threshold in SOL |
| `limit` | 1000 | max signatures to scan (≤ 1000) |
| `known` | — | comma-separated known wallets (creator/initial buyers) |

```bash
curl "http://localhost:3000/v1/radar/<MINT>?whale_sol=1&known=<CREATOR>"
```

Returns the full `RadarReport` JSON: mint, bonding_curve, create_slot,
trade/buy/sell counts, buy volume, snipers[], whales[], holder_count,
top_holders[], holder_growth[], known_wallets[].

### POST /v1/radar/watch — start a watch window

```json
{
  "mint": "<MINT>",
  "rpc_url": "https://devnet.helius-rpc.com/?api-key=<key>",
  "duration_secs": 1800,
  "poll_interval_secs": 30,
  "sniper_slots": 2,
  "whale_sol": 5.0,
  "limit": 1000,
  "known_wallets": ["<CREATOR>", "<BUYER>"]
}
```

Returns `{ watch_id, mint, duration_secs, poll_interval_secs, status }`.
The server polls in the background until the deadline; each poll replaces
the stored snapshot.

### GET /v1/radar/watch/{id} — watch status

Returns `{ watch_id, mint, started_at_unix, polls_completed, watching,
latest: <RadarReport|null>, error: <string|null> }`. 404 for unknown IDs.

## Devnet proof (Oct 19, 2026)

`src/bin/radar_proof.rs` (`cargo run --release --bin radar-proof`):

- Created token `Gk82a6DhBcDM5H7bGi1rsM8ERKQKXhhKYno1YzgJ4Hbe`
  (create tx `kYp5577ckCqaUVL1ujcLK1qgVh2YHT6XyNhuJx3Dz95bbmr2J9zJ6ssaNH9yKJocxT162a7617CF1KJLDjgM8D1`).
- Generated tape: creator buy 0.05 SOL → sniper buy 0.02 SOL → (15s) →
  organic buy 0.03 SOL → whale buy 0.25 SOL.
- Radar verified: true create_slot from address history (513003953);
  sniper flagged (earliest non-creator buy); creator excluded via
  known_wallets; whale flagged at 0.247 SOL ≥ 0.2 threshold; 4 holders;
  top-holder shares sum to ~10000 bps; holder growth 1→2→3→4; volume
  0.346 SOL. All assertions passed.
- Watch flow verified live: 3 polls, no errors, latest report populated.

## Limitations (honest)

- **Sniper window is slot-based.** On mainnet the Jito bundle lands
  create+buys in the same slot, so the default window of 2 is right. On
  devnet (sequential RPC submission, no Jito) trades spread over dozens
  of slots — the proof used a wider window for that reason.
- **Legacy sells don't simulate** on the upgraded bonding-curve program
  (`Custom 6074` in simulation). The radar *decodes* sells fine (same
  TradeEvent path, covered by unit tests), but the proof tape is
  buys-only. Fixing the sell instruction belongs to the sniper crate's
  tx_builder and is out of scope here.
- **getSignaturesForAddress caps at 1000** per call. Very active tokens
  need pagination (not yet implemented — `limit` maxes at 1000).
- Polling, not websocket: each snapshot is N+1 RPC calls (1 signature
  list + 1 tx fetch per signature, 120ms apart to be gentle). Fine for
  watch windows; not a firehose.
- Token-2022 mints only (what the bundler creates). Raydium/PumpSwap
  post-migration pools are not tracked — that's a future radar extension.
