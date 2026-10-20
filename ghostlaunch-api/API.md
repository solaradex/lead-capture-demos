# launch-api — HTTP API for the Pump.fun launch bundler

`launch-api` is a second binary in the `launch-bundle` Cargo package
(`src/bin/launch_api.rs`). It exposes the bundler as an HTTP API so launch
platforms can embed anti-snipe launches in their own coin-creation flow.

It is **non-custodial**: the server never sees or holds private keys. It
builds the exact same instructions as the `launch-bundle` CLI
(byte-identical messages — proven by the
`unsigned_message_matches_signed_message` test), but returns them
**unsigned**. The caller signs with their own keys and submits through
their own Jito setup.

## Running

```bash
cd launch-bundle
cargo build --release --bin launch-api

# Dev (no auth):
./target/release/launch-api

# Production:
LAUNCH_API_KEYS="key1,key2" PORT=3000 ./target/release/launch-api
```

| Env var          | Default | Purpose                                              |
|------------------|---------|------------------------------------------------------|
| `PORT`           | `3000`  | Listen port                                          |
| `LAUNCH_API_KEYS`| (unset) | Comma-separated API keys. Unset = auth disabled      |

If `LAUNCH_API_KEYS` is unset the server logs a warning and skips auth
(dev mode). `GET /v1/health` never requires auth.

## Auth

Send your key on every API call (except `/v1/health`):

```
X-API-Key: <your-key>
```

Missing or wrong key → `401 Unauthorized`.

## Endpoints

Current version: **0.3.0** (0.2.0 added stealth; 0.3.0 adds radar).

### GET /v1/health

```json
{ "status": "ok", "version": "0.3.0" }
```

### POST /v1/bundle/build

Builds the unsigned launch bundle: one `create_v2` transaction plus one
buy transaction per `initial_buys` entry (and an optional Jito tip
transaction), all sharing one recent blockhash.

**Request:**

```json
{
  "token_name": "My Coin",
  "token_symbol": "MC",
  "token_uri": "https://example.com/metadata.json",
  "creator_pubkey": "2gzurBnwbG8CamD5o7Pj2fYUCTMoaDDUdoUw7HJGS4LM",
  "initial_buys": [
    { "wallet": "2gzurBnwbG8CamD5o7Pj2fYUCTMoaDDUdoUw7HJGS4LM", "sol_amount": 1.0 },
    { "wallet": "9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7", "sol_amount": 0.5 }
  ],
  "slippage_bps": 500,
  "tip_lamports": 0,
  "tip_account": "96gYZGLnJYVFmbjzopPSU6QiEV5fGqCDTy4aGgB6oZ7I5",
  "rpc_url": "https://api.devnet.solana.com"
}
```

| Field            | Required | Default                        | Notes                                     |
|------------------|----------|--------------------------------|-------------------------------------------|
| `token_name`     | yes      | —                              | ≤ 32 chars                                |
| `token_symbol`   | yes      | —                              | ≤ 10 chars                                |
| `token_uri`      | yes      | —                              | Metadata JSON URL                         |
| `creator_pubkey` | yes      | —                              | Pays for creation; signs create + tip     |
| `initial_buys`   | yes      | —                              | 1–4 entries; wallets must be unique       |
| `slippage_bps`   | no       | `500`                          | ≤ 10000                                   |
| `tip_lamports`   | no       | `0`                            | `tip_account` required when > 0           |
| `tip_account`    | no       | —                              | Jito tip destination                      |
| `rpc_url`        | no       | `https://api.devnet.solana.com`| Used for fee recipient + blockhash        |
| `stealth`        | no       | —                              | Stealth distribution object (see below)   |

**Response (`200`):**

```json
{
  "mint_pubkey": "Bp1ErGRE9pQAMit1eayzXHyfN1vvujgXuPkVeheD6Hqi",
  "mint_secret_key": "base58-64-byte-secret-ONE-TIME",
  "blockhash": "GH7ome3Yf7R8...",
  "expires_in_slots": 150,
  "transactions": [
    {
      "index": 0,
      "kind": "create",
      "base64": "...unsigned tx...",
      "required_signers": ["<creator_pubkey>", "<mint_pubkey>"]
    },
    {
      "index": 1,
      "kind": "buy",
      "base64": "...unsigned tx...",
      "required_signers": ["<buyer_wallet>"]
    }
  ]
}
```

- `mint_secret_key` is returned **once**. The server generates the mint
  keypair per request and does not store it. Save it immediately — it must
  sign the create transaction.
- `required_signers` lists the signature slots **in order**. Sign slot `i`
  with the private key for `required_signers[i]`.
- `expires_in_slots: 150` — submit before the blockhash expires or rebuild.

**Errors:** `400` validation failure (reuses the CLI's plan validation),
`401` bad/missing API key, `422` malformed JSON, `502` RPC failure.

### Stealth distribution (`stealth` object, v0.2.0)

Passing `stealth` makes the initial buys unlinkable to trackers: multi-hop
funding through fresh ephemeral wallets, ±jittered buy amounts, and buys
spread across slots as separate transactions. Full design + rationale:
`STEALTH.md`.

**Request addition:**

```json
{
  "stealth": {
    "funding_hops": 2,
    "spread_slots": 3,
    "amount_jitter_bps": 500,
    "funding_buffer_sol": 0.015,
    "amount_seed": 99
  }
}
```

All fields optional. Defaults: 2 hops, 3-slot spread, ±500 bps jitter,
0.015 SOL buffer. `spread_slots: 0` keeps buys in the Jito bundle (funding
+ jitter still apply). `amount_seed` makes jitter deterministic — pass the
same seed when you build funding first and the bundle later (two calls) so
amounts match.

**Response addition** (`stealth` object alongside `transactions`):

- `funding_transactions` — unsigned plain SOL transfers, **in submit
  order**. Each is signed by its sender: the creator for each chain's first
  hop, then the hop wallets (see `hop_secret_keys`).
- `hop_secret_keys` — one-time base58 secrets for the ephemeral hop
  wallets. The server never stores them. Save, fund, discard.
- `executed_buy_amounts_sol` — the jittered amounts actually built.
- `buy_schedule` — per buy: `buy_index`, `transaction_index` (into
  `transactions`), `target_slot_offset`, `amount_sol`, `wallet`.
- `total_funding_sol` — what the creator must send across all first hops.
- `execution_note` — human-readable execution order.

Execution order: (1) sign + submit funding txs in order via direct RPC,
confirming each; (2) submit `transactions[0]` (create, + tip) via Jito
**alone** — do not bundle the buys when `spread_slots > 0`; (3) submit each
buy at/after `create_slot + target_slot_offset` via direct RPC.

**Two-call pattern** (funding takes longer than one blockhash lifetime):
call 1 with `stealth` + `amount_seed` → submit only the funding txs;
call 2 (at launch) with the same `amount_seed` → fresh blockhash/mint/hops
but identical jittered amounts, so the funding covers the buys exactly.

### POST /v1/bundle/simulate

Same request format as `/v1/bundle/build`. Builds the bundle and
simulates every transaction via `simulateTransaction` (`sigVerify=false`).
Submits nothing.

**Response (`200`):**

```json
{
  "mint_pubkey": "Bp1ErGRE9pQAMit1eayzXHyfN1vvujgXuPkVeheD6Hqi",
  "results": [
    { "index": 0, "kind": "create", "success": true, "error": null, "logs": ["..."] },
    { "index": 1, "kind": "buy", "success": false, "error": "...", "logs": ["..."] }
  ]
}
```

Note: the mint here is a throwaway (its secret is discarded, not
returned) — this endpoint is for validation only. Call `/v1/bundle/build`
for the real thing. With `stealth`, the creator-signed funding legs are
simulated first (kind `"funding"`); hop legs are skipped — their sender is
unfunded until the prior hop confirms.

### GET /v1/radar/{mint} — post-launch snapshot (v0.3.0)

Read-only monitor: pulls the bonding-curve trade tape, flags snipers
(buys within `sniper_slots` of creation, default 2, known wallets excluded)
and whales (single buys/sells ≥ `whale_sol`, default 5.0), plus holder
analytics (top-10 shares, holder count, growth series). See RADAR.md.

Query params (all optional): `rpc_url`, `sniper_slots`, `whale_sol`,
`limit` (≤1000), `known` (comma-separated wallets).

```bash
curl "http://localhost:3000/v1/radar/<MINT>?whale_sol=1&known=<CREATOR>"
```

### POST /v1/radar/watch — start a watch window (v0.3.0)

```json
{
  "mint": "<MINT>",
  "rpc_url": "https://devnet.helius-rpc.com/?api-key=<key>",
  "duration_secs": 1800,
  "poll_interval_secs": 30,
  "sniper_slots": 2,
  "whale_sol": 5.0,
  "known_wallets": ["<CREATOR>"]
}
```

Returns `{ watch_id, ... }`. Polls in the background until the deadline.

### GET /v1/radar/watch/{id} — watch status (v0.3.0)

Returns `{ watch_id, mint, started_at_unix, polls_completed, watching,
latest: <RadarReport|null>, error }`.

## The platform signing flow

1. `POST /v1/bundle/build` → get unsigned transactions + mint secret.
2. Rebuild each transaction from its base64, sign every slot in
   `required_signers` order:
   - `create`: sign with the **creator key** (slot 0) and the **mint key**
     from `mint_secret_key` (slot 1).
   - each `buy`: sign with that buyer's wallet key (slot 0).
   - `tip` (if present): sign with the creator key (slot 0).
3. Submit the signed transactions as one Jito bundle, in order, before the
   blockhash expires (`expires_in_slots`).
4. Recommended: call `/v1/bundle/simulate` first and refuse to submit if
   any transaction fails simulation — same as the CLI's protection gate.

```bash
# Full example (devnet, dev mode — no auth):
curl -s -X POST http://localhost:3000/v1/bundle/build \
  -H 'Content-Type: application/json' \
  -d '{
    "token_name": "My Coin",
    "token_symbol": "MC",
    "token_uri": "https://example.com/metadata.json",
    "creator_pubkey": "2gzurBnwbG8CamD5o7Pj2fYUCTMoaDDUdoUw7HJGS4LM",
    "initial_buys": [
      {"wallet": "2gzurBnwbG8CamD5o7Pj2fYUCTMoaDDUdoUw7HJGS4LM", "sol_amount": 1.0}
    ]
  }' | python3 -m json.tool
```

## Notes for operators

- The unsigned transactions are standard Solana v0 transactions with
  64-zero-byte placeholder signatures. Any Solana SDK can sign them
  (`VersionedTransaction` with signatures filled in slot order).
- Compute budget: create/buys use 300,000 CU at 50,000 µlamports (same as
  the CLI); the tip uses 10,000 CU.
- Buys use `buy_exact_quote_in_v3` with the buyback fee recipient, Token-2022
  ATAs, and per-buyer ATA creation — identical to the CLI path.
- Jito bundles hold at most 5 transactions: the API enforces
  1 create + up to 4 buys (+ tip counts toward the 5).
