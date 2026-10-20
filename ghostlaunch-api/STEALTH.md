# Stealth distribution — unlinkable launch buys

The stealth layer makes a launch's initial buys **not look like one bundled
entity** to on-chain trackers. This is the launch system's differentiator
against creator-facing bundlers (Smithii et al.): instead of one funder,
one bundle, and round-number buys, a stealth launch shows several
independently-funded wallets buying different amounts across several
slots — indistinguishable from organic early flow.

## What trackers look for (and what we break)

| Tracker heuristic | Without stealth | With stealth |
|---|---|---|
| One funder pays N buyer wallets directly | creator → buyer transfers, trivially clustered | Each buyer is funded through 1–3 **fresh ephemeral hops**; the creator never touches a buyer wallet |
| Identical / round-number buy amounts | `1.0`, `0.5` SOL — exact plan values | Each amount jittered ±`amount_jitter_bps` (default ±5%) — `0.506565124`, `0.972570428` |
| Buys land in the same slot/bundle as creation | 1 Jito bundle: `[create, buy, buy…]` | Buys spread across `[0, spread_slots]` slots as **separate transactions**, never in the Jito bundle with `create` |
| Hop wallets link back | n/a | Every hop ends at **exactly 0** (keeps one 5,000-lamport fee back when forwarding) — no dust to cluster on; hops are never reused |

## Components

### 1. Multi-hop funding (`src/stealth.rs` → `plan_stealth`)

Per buyer, the planner generates `funding_hops` fresh keypairs and derives
the transfer chain:

```
creator → hop₀ → hop₁ → … → hopₙ₋₁ → buyer
```

Transfer amounts shrink by exactly one transfer fee (5,000 lamports) per
hop, so every hop ends at zero and the buyer ends with exactly
`jittered_buy + funding_buffer`:

- `creator → hop₀`: `need + H·fee`
- `hopᵢ → hopᵢ₊₁`: `need + (H−1−i)·fee`
- `hopₙ₋₁ → buyer`: `need`

where `need = jittered_buy_lamports + funding_buffer_lamports`.

Funding transfers are **plain SOL transfers** — no compute-budget
instructions, exactly 5,000 lamports fee — so they look like ordinary
wallet activity and the fee math is exact. They are built and submitted
**before** the launch via direct RPC (sequentially, each confirmed before
the next), never inside the Jito bundle.

### 2. Randomized buy amounts

Uniform jitter in `±amount_jitter_bps` (default 500 = ±5%; recommended
operating range ±3–8%, hard cap ±20%). The jitter is drawn **once per
build** and shared between the funding math and the buy transactions, so
funding always covers the actual buy.

**Deterministic seeds.** Pass `amount_seed` and the jitter becomes
deterministic: same seed + same plan ⇒ same jittered amounts. This powers
the two-call API pattern (below).

### 3. Randomized buy timing

With `spread_slots > 0`, each buy gets a `target_slot_offset` in
`[0, spread_slots]` (even coverage ± 1 slot of jitter) and is returned as a
**separate transaction**. The Jito bundle then holds only `[create]`
(+ tip). The platform submits each buy via direct RPC at/after
`create_slot + offset`, signed by the buyer wallet.

With `spread_slots = 0`, buys stay in the Jito bundle (classic behavior)
but funding + jitter still apply.

**Tradeoff (read this):** spread buys lose Jito atomicity — a sniper could
theoretically land between `create` and a later buy. What you buy is
unlinkability: the buys no longer share a bundle, a slot, an amount, or a
funder with each other. For most launches the bigger threat is post-launch
tracker labeling ("dev bundle wallets"), not the 1–3 slot window.

## Configuration

Plan file (`stealth` section, all optional):

```json
{
  "stealth": {
    "funding_hops": 2,
    "spread_slots": 3,
    "amount_jitter_bps": 500,
    "funding_buffer_sol": 0.015,
    "amount_seed": 42
  }
}
```

| Field | Default | Range | Notes |
|---|---|---|---|
| `funding_hops` | `2` | 1–3 | Fresh ephemeral wallets per buyer per launch |
| `spread_slots` | `3` | 0–120 | 0 = buys stay in the Jito bundle |
| `amount_jitter_bps` | `500` | 0–2000 | ±basis points on each buy amount |
| `funding_buffer_sol` | `0.015` | 0–1 | Per-buyer headroom for ATA rent + fees |
| `amount_seed` | — | u64 | Deterministic jitter (two-call API pattern) |

Validation is shared: the CLI plan validator and the API both reject bad
configs before touching RPC.

## CLI flow (`launch-bundle`)

With a `stealth` section in the plan:

1. **Funding.** Signed funding transactions are built (own blockhash) and
   simulated; with `--execute` they are submitted sequentially via direct
   RPC — each simulated right before submission (a hop only exists after
   the previous hop confirms) and confirmed before the next.
2. **Launch.** The bundle is rebuilt with a fresh blockhash:
   `[create]` (+ tip) for Jito, spread buys separately.
3. **Submit.** The Jito bundle goes first; then each spread buy is submitted
   via direct RPC once `current_slot ≥ create_slot + target_slot_offset`.

Dry-run prints the full funding plan (per-buyer jittered amounts, slot
schedule, total creator funding) and simulates everything submittable.

## API flow (`POST /v1/bundle/build` + `stealth` object)

Request:

```json
{
  "token_name": "My Coin",
  "token_symbol": "MC",
  "token_uri": "https://example.com/metadata.json",
  "creator_pubkey": "<creator>",
  "initial_buys": [{"wallet": "<buyer>", "sol_amount": 0.5}],
  "rpc_url": "https://api.devnet.solana.com",
  "stealth": {
    "funding_hops": 2,
    "spread_slots": 3,
    "amount_jitter_bps": 500,
    "funding_buffer_sol": 0.015,
    "amount_seed": 99
  }
}
```

Response adds a `stealth` object:

```json
{
  "mint_pubkey": "...",
  "mint_secret_key": "ONE-TIME",
  "blockhash": "...",
  "expires_in_slots": 150,
  "transactions": [
    {"index": 0, "kind": "create", "base64": "...", "required_signers": ["<creator>", "<mint>"]},
    {"index": 1, "kind": "buy", "base64": "...", "required_signers": ["<buyer>"]}
  ],
  "stealth": {
    "funding_transactions": [
      {"index": 0, "kind": "funding", "base64": "...", "required_signers": ["<creator>"]},
      {"index": 1, "kind": "funding", "base64": "...", "required_signers": ["<hop0>"]}
    ],
    "funding_blockhash": "...",
    "funding_expires_in_slots": 150,
    "hop_secret_keys": [{"pubkey": "<hop0>", "secret_key": "ONE-TIME-BASE58"}],
    "executed_buy_amounts_sol": [0.506565124],
    "buy_schedule": [{"buy_index": 0, "transaction_index": 1, "target_slot_offset": 1, "amount_sol": 0.506565124, "wallet": "<buyer>"}],
    "total_funding_sol": 0.526575124,
    "execution_note": "1) Sign each funding tx with its sender's key..."
  }
}
```

**Non-custodial.** The server never holds creator or buyer keys. Hop
wallets are ephemeral and launch-scoped: the server generates them and
returns their secrets **one time** (same pattern as `mint_secret_key`) —
save them, fund, then discard. Hop keys are never reused across launches.

**Execution order** (also spelled out in `execution_note`):

1. Sign each funding tx with its sender's key (creator first, then hop
   secrets) and submit **in order** via direct RPC, confirming each.
2. Sign `transactions[0]` (create, + tip if present) with creator + mint
   keys and submit via Jito **alone** — do not bundle the buys.
3. Submit each buy at/after `create_slot + target_slot_offset` via direct
   RPC, signed by the buyer wallet. (With `spread_slots: 0`, submit
   `transactions[]` as one Jito bundle instead — the buys are already in
   it, carrying jittered amounts.)

**Two-call pattern.** Funding N buyers × H hops takes longer than one
blockhash lifetime. For that, split it:

1. Call `/v1/bundle/build` with `stealth` (including `amount_seed`) →
   submit **only** `stealth.funding_transactions`, then discard the rest.
2. At launch time, call `/v1/bundle/build` again with the **same**
   `amount_seed` → fresh blockhash, fresh mint, fresh hops, but
   **identical jittered amounts** (verified by test and live), so the
   funding you already sent covers the buys exactly. Use this response's
   `transactions` + `stealth.buy_schedule`.

`/v1/bundle/simulate` accepts the same `stealth` object and simulates the
creator-signed funding legs plus the bundle (hop legs are skipped — their
sender is unfunded until the prior hop confirms).

## Proven on devnet (Oct 19, 2026)

`src/bin/stealth_proof.rs` ran the full funding chain on devnet via Helius:

- 2 buyers × 2 hops = 6 transfers; every transfer simulated ok and
  **confirmed on-chain**, sequentially.
- Buyer end balances exactly `jittered + buffer`
  (0.065133 / 0.096092 SOL); **all 4 hop wallets ended at exactly 0**.
- Seeded rebuild produced identical jittered amounts (two-call pattern).
- API verified live: `/v1/bundle/build` with `stealth` returned funding
  txs + one-time hop secrets + buy schedule; two calls with the same
  `amount_seed` returned identical amounts with fresh mint/hops.

Funder-signed devnet signatures (chain [0]/[3] of the two chains):

- `3LmCAdq1DcJAERVkxYcviyngQAvf7mJcgtcA6JpPo6o7sTTsvzLQbiadJ8f1rmtqo88ySrRcSPK1w2vdNbahHafh`
- `LG58FprULSNwtEwkzwMzbxifRcruwfuDa5PnMYYQaiQ8oFuvharEq1CNRAV85bRYsKL3RirJx1Kp7uAtRapkv4c`

## Design notes / limitations

- **Buy simulation vs. fresh mint.** Buy transactions reference a mint that
  doesn't exist until `create` lands, so `simulate` reports
  `AccountNotFound` for buys — same as the pre-stealth CLI/API behavior.
  This is expected, not a defect.
- **Hop-leg simulation.** A hop wallet is unfunded until the previous hop
  confirms, so hop legs can only be simulated sequentially at execution
  time (the CLI `--execute` path and the proof do exactly this).
- **Spread cap (120 slots).** One blockhash covers ~150 slots; the cap
  keeps funding + bundle inside a single build call's blockhash lifetime.
- **Funding is pre-launch and visible.** The funding chain is on-chain —
  anyone can trace creator → hops → buyer *if they know which wallets to
  look at*. The protection is that hops are fresh, single-use, zeroed out,
  and (with spread) temporally separated from the launch; there is no
  on-chain marker linking a hop chain to a later token launch.
- **No privacy primitive.** This is heuristic unlinkability, not
  cryptographic privacy. A determined analyst with the buyer list can still
  connect the dots — the goal is defeating automated tracker labeling,
  not a forensic accountant.
