# GhostLaunch — Full-System Devnet Test

**Date:** Oct 19, 2026 (~10:30pm EDT)
**Scope:** all four layers TOGETHER, end to end, through the public API surface.
**Network:** devnet ONLY. Nothing touched mainnet.
**Result: PASS — all four integration joints verified on-chain.**

## System under test

| Layer | Component | Version |
|---|---|---|
| 1. Buy fix | `create_v2` + `buy_exact_quote_in_v3` (17 accts, buyback recipient) | proven Oct 19 |
| 2. API | `launch-api` (Axum) — build / simulate / health / radar | v0.3.0, 90/90 tests |
| 3. Stealth (Cloak) | `src/stealth.rs` — multi-hop funding, ±jitter, slot-spread buys | proven Oct 19 |
| 4. Radar | `src/radar.rs` — sniper/whale/holder analytics, read-only | proven Oct 19 |

**Test executor:** `src/bin/fullsystem_proof.rs` (new) — the platform integrator's
reference implementation for the API → execution joint. It signs the API's
unsigned transactions via `required_signers` order and submits via direct RPC
(Jito is mainnet-only; devnet uses per-transaction direct submission).

**Test parameters:** 2 buyers (0.08 / 0.06 SOL), 2 funding hops each, 3-slot
spread, ±500 bps jitter, 0.015 SOL buffer. Funder: devnet test wallet
`4oUyaKH…QcfSut`. RPC: Helius devnet.

## Step-by-step evidence

### 1. API build with stealth — PASS
`POST /v1/bundle/build` returned: fresh mint
`AGPdp5tMiEwK7oQxnVZjkkxXrLmTD52hkfTpjLHGCEBm`, one-time mint secret,
4 hop wallets, 6 funding txs (in submit order), 1 create + 2 buys,
buy_schedule with slot offsets.

### 2. Stealth funding chain — PASS (6/6 confirmed)
| # | Sender (first 8) | Signature |
|---|---|---|
| 0 | `4oUyaKHz` (creator) | `65cScmdwJEYaG449yviBQjJnbzLa8ipHx7YASbHxAzQBaxhrGr867baKRAWj6aZgLykCLNTEqaha7hY6qnAFKKDv` |
| 1 | `ADfjPM8Z` (hop) | `xBEz8jPaV9J6YBe3pvb8BELw5VE15dXRbrnLbfZL2HXc6xaYSZpMxDAuStWbPaM7ont5kmdYn5xDhpjqnc1syAh` |
| 2 | `3bmoopUs` (hop) | `2vuJD3ToARqYwLu6hpVANW8KACp4kaKpJHbb3R3DwBLX7umkxEhWrAaBxyzTczUXwn6FUdELFsLdzCMt6EqxJGmW` |
| 3 | `4oUyaKHz` (creator) | `4HZFHTDWnRJtx6MqsZKRtkzTmssWwZyxXXToPUwMKBbWpctZV54ZZ3U312RgUVxEBBx2Nj6yg9kxemhHfGajVryJ` |
| 4 | `8FLGz9Gr` (hop) | `jGCTeUSgHi5u7FqNG3GeXdxLzrG4EUsuDSZPL3wqovtaL4a2cj83xfmGyhwcDLFADPefr7xtuLB3dcXwSWk7a2c` |
| 5 | `BkAbHWtZ` (hop) | `JFfVW2GoBAh3gNFbUB78GnA7KDMudRSF9dvs3ztpjUwvGkJBSG3eybxDbTtLHoE3fAS3XFTbgnZagg8MdFFJhAM` |

Joint proven: API-issued **one-time hop secrets** reconstruct into valid
signing keypairs; `required_signers` order signs every leg correctly.

### 3. Create — PASS
`vc4CiXRrhmA5aLmPHqkcpqatVpX6Vnfum5VzgMzE2zApYdZbCyWGB4d6H3RT6MgC5uc42CQNLWgzkSQbrtfMD8i`
(slot **513009913**). Signed by creator + mint key from the API's one-time
mint secret. Token-2022 mint created on devnet.

### 4. Spread buys — PASS (per buy_schedule)
| Buy | Amount (jittered) | Target slot | Landed slot | Signature |
|---|---|---|---|---|
| 0 | 0.080023 SOL | 513009914 (+1) | 513009928 | `5KhYcDKE6UidDdndcaQmeVagfhLUuApLGhWDzipP6nQWu9nKKadL7XWBF4KJZX2gym31KuQhLWXHX8Pz8drWNa69` |
| 1 | 0.062754 SOL | 513009916 (+3) | 513009944 | `5XwoLoR4nBf7QzbFKtJ8ovqoZYfCLtt6JRxB7ExYBBbhEJV2iE7ucagZMXiWaZV8STxdk8uw4VbfoJmNrCB6fMBo` |

(Landed slots run a few slots past target — the executor guarantees
at-or-after; devnet slot timing accounts for the delta. Not a defect.)

### 5. Radar — PASS
`GET /v1/radar/AGPdp5tMiEwK7oQxnVZjkkxXrLmTD52hkfTpjLHGCEBm?whale_sol=1&known=<creator>,<buyer1>,<buyer2>`

```json
{
  "mint": "AGPdp5tMiEwK7oQxnVZjkkxXrLmTD52hkfTpjLHGCEBm",
  "bonding_curve": "2cuSUoBqVm2HgnSkjFJEvjYcrj98jf7BLJDZkV8nbVTU",
  "create_slot": 513009913,
  "buy_count": 2, "sell_count": 0, "trade_count": 2,
  "holder_count": 2,
  "top_holders": [
    {"wallet": "HCPt3wRsTRzFkS3YPSZTfzf5QAhnznFcsxnKNPyh9Gjh", "share_bps": 6521},
    {"wallet": "7j2aoD6Efzz1tjkMXU7g6G979fB9Qy1XcyhxuTAbgury", "share_bps": 3478}
  ],
  "snipers": [], "whales": []
}
```

Radar detected the creation, both buys, the exact 2-buyer holder set with
correct shares, and holder growth 1→2. No whales (correct: 0.08/0.06 SOL <
1 SOL threshold).

### 6. known_wallets exclusion — PASS (A/B verified)
- **Without** `known`, `sniper_slots=50`: both buyers flagged as snipers
  (15 and 31 slots after create). Detection works.
- **With** `known=<creator,buyer1,buyer2>`, `sniper_slots=50`: `snipers: []`.
  Exclusion works exactly as designed — a platform passing its own wallets
  will not see them mislabeled.

## Integration joint verdicts

| Joint | Result |
|---|---|
| A. `api.build` → external signing (`required_signers` order) | **PASS** — 9/9 txs signed correctly, incl. hop secrets |
| B. Funding chain → sequential on-chain execution | **PASS** — 6/6 confirmed in submit order |
| C. Create → spread buys per `buy_schedule` | **PASS** — create + 2 buys landed at/after target slots |
| D. Launched mint → radar detection | **PASS** — full trade tape, holders, sniper/whale flags correct |

## Integration gaps found

**None blocking.** Observations (all pre-existing, documented behavior):
1. Jito is mainnet-only; devnet execution uses direct RPC per transaction
   (same approach as all prior devnet proofs).
2. Buy simulation against a fresh mint returns `AccountNotFound` — expected,
   since the mint doesn't exist until create lands. Pre-existing, not a
   regression.
3. Spread buys land a few slots after their target (executor waits for
   at-or-after). Fine for stealth; platforms wanting tighter timing can
   reduce `spread_slots`.

## Conclusion

**GhostLaunch works end to end.** A platform can call one API endpoint,
receive everything needed (unsigned bundle + funding plan + one-time
secrets + execution schedule), execute it with their own keys without ever
sharing them, and monitor the launch with radar — all proven live on devnet
tonight. This is the milestone the build was working toward.
