//! Stealth distribution: make a launch's initial buys unlinkable on-chain.
//!
//! Pure planning module — no RPC, no signing, no submission. Given a
//! validated [`LaunchPlan`] and a [`StealthConfig`] it derives:
//!
//!   1. **Jittered buy amounts** — each buy is perturbed by ±`amount_jitter_bps`
//!      so on-chain amounts are not round numbers and not identical.
//!   2. **Multi-hop funding** — every buyer wallet is funded through
//!      `funding_hops` fresh ephemeral wallets
//!      (creator → hop₁ → … → hopₙ → buyer) using plain SOL transfers.
//!      Hop keypairs are generated fresh per launch and never reused.
//!      Each hop keeps exactly one transfer fee back, so hops end at **zero**
//!      — no dust left behind to cluster on.
//!   3. **Slot spread** — each buy gets a `target_slot_offset`; with
//!      `spread_slots > 0` the buys are returned as *separate* transactions
//!      (never in the Jito bundle with `create`) for the platform to submit
//!      at staggered slots.
//!
//! ## What this defeats
//!
//! Trackers flag bundled launches with three heuristics:
//!   (a) one funder directly paying several buyer wallets,
//!   (b) identical or round-number buy amounts,
//!   (c) buys landing in the same slot/bundle as creation.
//! This module breaks all three: funding fans out through throwaway
//! intermediaries, amounts carry random noise, and buys land across slots
//! like organic flow.
//!
//! ## Non-custodial note
//!
//! Hop keypairs are ephemeral and launch-scoped. The API generates them and
//! returns the secrets **one time** (same pattern as the mint secret) — the
//! server never holds creator or buyer keys.

use anyhow::{anyhow, Result};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::str::FromStr;

use crate::plan::LaunchPlan;

/// Lamports a plain single-signature system transfer costs. Each hop keeps
/// exactly this much back when forwarding so it ends at zero.
pub const TRANSFER_FEE_LAMPORTS: u64 = 5_000;
/// Hard cap on hops per buyer: more hops = more fees + more pre-launch
/// confirmation latency, with diminishing unlinkability returns.
pub const MAX_FUNDING_HOPS: u8 = 3;
/// A recent blockhash is valid ~150 slots; the spread is capped so one
/// blockhash covers funding + bundle in a single build call.
pub const MAX_SPREAD_SLOTS: u64 = 120;
/// Sanity cap on amount jitter (±20%). Recommended operating range ±3–8%.
pub const MAX_JITTER_BPS: u16 = 2_000;

fn default_funding_hops() -> u8 {
    2
}
fn default_spread_slots() -> u64 {
    3
}
fn default_jitter_bps() -> u16 {
    500
}
fn default_buffer_sol() -> f64 {
    0.015
}

/// Stealth options. Everything is optional with sane defaults; the section
/// itself is optional in the launch plan (absent = current bundled behavior).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StealthConfig {
    /// Funding hops per buyer, 1..=3 (default 2).
    #[serde(default = "default_funding_hops")]
    pub funding_hops: u8,
    /// Spread buys across [0, spread_slots] slots after creation.
    /// 0 = buys stay in the Jito bundle (funding + jitter still apply).
    #[serde(default = "default_spread_slots")]
    pub spread_slots: u64,
    /// Buy-amount randomization, ±basis points (default 500 = ±5%).
    #[serde(default = "default_jitter_bps")]
    pub amount_jitter_bps: u16,
    /// Extra SOL per buyer on top of the (jittered) buy, covering ATA rent
    /// and transaction fees (default 0.015).
    #[serde(default = "default_buffer_sol")]
    pub funding_buffer_sol: f64,
    /// Optional deterministic seed for the amount jitter. A platform that
    /// funds first and builds the launch bundle later (two API calls) passes
    /// the same seed in both so the jittered amounts — and therefore the
    /// funding math — come out identical.
    #[serde(default)]
    pub amount_seed: Option<u64>,
}

impl StealthConfig {
    pub fn validate(&self) -> Result<()> {
        if self.funding_hops == 0 || self.funding_hops > MAX_FUNDING_HOPS {
            return Err(anyhow!(
                "funding_hops must be 1..={MAX_FUNDING_HOPS}"
            ));
        }
        if self.spread_slots > MAX_SPREAD_SLOTS {
            return Err(anyhow!(
                "spread_slots must be <= {MAX_SPREAD_SLOTS} (blockhash lifetime)"
            ));
        }
        if self.amount_jitter_bps > MAX_JITTER_BPS {
            return Err(anyhow!(
                "amount_jitter_bps must be <= {MAX_JITTER_BPS}"
            ));
        }
        if !self.funding_buffer_sol.is_finite()
            || self.funding_buffer_sol < 0.0
            || self.funding_buffer_sol > 1.0
        {
            return Err(anyhow!("funding_buffer_sol must be between 0 and 1"));
        }
        Ok(())
    }
}

/// One SOL funding transfer, in global submit order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FundingTransfer {
    /// Submit order across all buyers' chains (0-based).
    pub sequence: usize,
    /// Base58 sender.
    pub from: String,
    /// Base58 recipient.
    pub to: String,
    pub lamports: u64,
    /// Base58 buyer wallet this chain ultimately funds.
    pub buyer: String,
}

/// Everything the stealth layer decided for one buyer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuyerStealth {
    pub buy_index: usize,
    pub buyer: String,
    pub planned_amount_lamports: u64,
    pub jittered_amount_lamports: u64,
    /// Signed basis points actually applied (e.g. -342 = -3.42%).
    pub jitter_bps_applied: i32,
    /// Fresh ephemeral hop wallets, base58, in chain order.
    pub hop_wallets: Vec<String>,
    /// What the buyer must end up holding: jittered buy + buffer.
    pub funding_needed_lamports: u64,
    /// Submit this buy at/after create_slot + this (0 = with the bundle).
    pub target_slot_offset: u64,
}

/// The full stealth plan: pure data, no RPC touched.
pub struct StealthPlan {
    pub config: StealthConfig,
    pub buyers: Vec<BuyerStealth>,
    pub funding_transfers: Vec<FundingTransfer>,
    /// Fresh ephemeral hop keypairs, indexed [buy_index][hop_index].
    /// Held only to sign funding transactions (CLI) or returned one-time
    /// as base58 secrets (API). Never reused across launches.
    pub hop_keypairs: Vec<Vec<Keypair>>,
}

impl StealthPlan {
    /// Total lamports the creator must send across all first-hop transfers.
    pub fn total_creator_funding_lamports(&self, creator: &Pubkey) -> u64 {
        let c = creator.to_string();
        self.funding_transfers
            .iter()
            .filter(|t| t.from == c)
            .map(|t| t.lamports)
            .sum()
    }

    /// Keypair that must sign as `pubkey`: the creator, or one of the
    /// ephemeral hop wallets.
    pub fn signer_keypair<'a>(
        &'a self,
        creator: &'a Keypair,
        pubkey: &Pubkey,
    ) -> Option<&'a Keypair> {
        if creator.pubkey() == *pubkey {
            return Some(creator);
        }
        let want = pubkey.to_string();
        for (bi, buyer) in self.buyers.iter().enumerate() {
            for (hi, hw) in buyer.hop_wallets.iter().enumerate() {
                if *hw == want {
                    return Some(&self.hop_keypairs[bi][hi]);
                }
            }
        }
        None
    }

    /// One-time export of hop secrets: (pubkey, base58 64-byte secret).
    /// The API returns these once; the CLI never exports them.
    pub fn hop_secrets_bs58(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for chain in &self.hop_keypairs {
            for kp in chain {
                out.push((
                    kp.pubkey().to_string(),
                    bs58::encode(kp.to_bytes()).into_string(),
                ));
            }
        }
        out
    }
}

/// Build the stealth plan with a caller-supplied RNG (tests seed this for
/// determinism; production uses a seeded or OS RNG — see [`plan_stealth`]).
fn plan_stealth_with_rng<R: Rng>(
    plan: &LaunchPlan,
    config: &StealthConfig,
    rng: &mut R,
) -> Result<StealthPlan> {
    config.validate()?;
    let creator = Pubkey::from_str(&plan.creator).map_err(|_| anyhow!("bad creator pubkey"))?;
    let n = plan.initial_buys.len();
    let buffer_lamports = (config.funding_buffer_sol * 1_000_000_000.0).round() as u64;
    let hops = config.funding_hops as usize;

    let mut buyers = Vec::with_capacity(n);
    let mut hop_keypairs: Vec<Vec<Keypair>> = Vec::with_capacity(n);
    let mut funding_transfers: Vec<FundingTransfer> = Vec::new();

    for (i, buy) in plan.initial_buys.iter().enumerate() {
        let buyer =
            Pubkey::from_str(&buy.wallet).map_err(|_| anyhow!("bad buyer wallet"))?;
        let planned = (buy.sol_amount * 1_000_000_000.0).round() as u64;

        // --- Amount jitter: uniform in ±amount_jitter_bps. ---
        let half = config.amount_jitter_bps as f64 / 10_000.0;
        let r: f64 = if half > 0.0 {
            rng.random_range(-half..=half)
        } else {
            0.0
        };
        let jittered = (((planned as f64) * (1.0 + r)).round() as u64).max(1);
        let jitter_bps_applied = (r * 10_000.0).round() as i32;
        let need = jittered + buffer_lamports;

        // --- Fresh ephemeral hop wallets for this buyer. ---
        let chain: Vec<Keypair> = (0..hops).map(|_| Keypair::new()).collect();
        let hop_pks: Vec<Pubkey> = chain.iter().map(|k| k.pubkey()).collect();

        // --- Funding chain: creator → h₀ → … → hₙ₋₁ → buyer.
        // Each sender forwards `need` plus one fee per hop still downstream,
        // keeping exactly TRANSFER_FEE_LAMPORTS for its own fee — so every
        // hop ends at zero and only the buyer retains a balance. ---
        let mut seq = funding_transfers.len();
        let mut push = |from: &Pubkey, to: &Pubkey, lamports: u64| {
            funding_transfers.push(FundingTransfer {
                sequence: seq,
                from: from.to_string(),
                to: to.to_string(),
                lamports,
                buyer: buyer.to_string(),
            });
            seq += 1;
        };
        push(&creator, &hop_pks[0], need + (hops as u64) * TRANSFER_FEE_LAMPORTS);
        for j in 0..hops {
            let to = if j + 1 < hops { hop_pks[j + 1] } else { buyer };
            push(&hop_pks[j], &to, need + ((hops - 1 - j) as u64) * TRANSFER_FEE_LAMPORTS);
        }

        // --- Slot schedule: even coverage of [0, spread_slots] ± 1 slot. ---
        let target_slot_offset = if config.spread_slots == 0 || n == 1 {
            0
        } else {
            let base = (i as u64 * config.spread_slots) / (n as u64 - 1);
            let j: i64 = rng.random_range(-1..=1);
            (base as i64 + j).clamp(0, config.spread_slots as i64) as u64
        };

        buyers.push(BuyerStealth {
            buy_index: i,
            buyer: buyer.to_string(),
            planned_amount_lamports: planned,
            jittered_amount_lamports: jittered,
            jitter_bps_applied,
            hop_wallets: hop_pks.iter().map(|p| p.to_string()).collect(),
            funding_needed_lamports: need,
            target_slot_offset,
        });
        hop_keypairs.push(chain);
    }

    Ok(StealthPlan {
        config: config.clone(),
        buyers,
        funding_transfers,
        hop_keypairs,
    })
}

/// Build the stealth plan. With `amount_seed` the jitter is deterministic
/// (same seed + same plan = same amounts — the two-call API pattern);
/// otherwise it draws from the OS RNG.
pub fn plan_stealth(plan: &LaunchPlan, config: &StealthConfig) -> Result<StealthPlan> {
    match config.amount_seed {
        Some(seed) => plan_stealth_with_rng(plan, config, &mut StdRng::seed_from_u64(seed)),
        None => plan_stealth_with_rng(plan, config, &mut rand::rng()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{InitialBuy, LaunchPlan};

    fn wallets(n: usize) -> Vec<String> {
        (0..n).map(|_| Keypair::new().pubkey().to_string()).collect()
    }

    fn plan_with_buys(amounts: &[f64]) -> LaunchPlan {
        let ws = wallets(amounts.len() + 1);
        LaunchPlan {
            token_name: "T".into(),
            token_symbol: "T".into(),
            token_uri: "https://example.com/m.json".into(),
            creator: ws[0].clone(),
            initial_buys: amounts
                .iter()
                .enumerate()
                .map(|(i, a)| InitialBuy {
                    wallet: ws[i + 1].clone(),
                    sol_amount: *a,
                })
                .collect(),
            tip_lamports: 0,
            slippage_bps: 500,
            stealth: None,
        }
    }

    fn cfg() -> StealthConfig {
        StealthConfig {
            funding_hops: 2,
            spread_slots: 3,
            amount_jitter_bps: 500,
            funding_buffer_sol: 0.015,
            amount_seed: Some(42),
        }
    }

    fn seeded_plan(amounts: &[f64]) -> StealthPlan {
        let p = plan_with_buys(amounts);
        plan_stealth_with_rng(&p, &cfg(), &mut StdRng::seed_from_u64(42)).unwrap()
    }

    fn seeded_plan_and_plan(amounts: &[f64]) -> (LaunchPlan, StealthPlan) {
        let p = plan_with_buys(amounts);
        let s =
            plan_stealth_with_rng(&p, &cfg(), &mut StdRng::seed_from_u64(42)).unwrap();
        (p, s)
    }

    #[test]
    fn rejects_bad_configs() {
        let mut c = cfg();
        c.funding_hops = 0;
        assert!(c.validate().is_err());
        c.funding_hops = 4;
        assert!(c.validate().is_err());
        c.funding_hops = 2;
        c.spread_slots = 121;
        assert!(c.validate().is_err());
        c.spread_slots = 3;
        c.amount_jitter_bps = 2001;
        assert!(c.validate().is_err());
        c.amount_jitter_bps = 500;
        c.funding_buffer_sol = -0.1;
        assert!(c.validate().is_err());
        c.funding_buffer_sol = 0.015;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let s = seeded_plan(&[1.0, 2.5, 0.1]);
        for b in &s.buyers {
            assert!(b.jitter_bps_applied.abs() <= 500);
            let lo = (b.planned_amount_lamports as f64 * 0.95) as u64;
            let hi = (b.planned_amount_lamports as f64 * 1.05) as u64 + 1;
            assert!(
                b.jittered_amount_lamports >= lo && b.jittered_amount_lamports <= hi,
                "buy {} out of bounds",
                b.buy_index
            );
        }
        // Jitter actually moves amounts (not all zero).
        assert!(s.buyers.iter().any(|b| b.jitter_bps_applied != 0));
    }

    #[test]
    fn funding_chain_math_is_exact() {
        // Simulate the chain: every hop must end at exactly 0, the buyer at `need`.
        let s = seeded_plan(&[1.0]);
        let b = &s.buyers[0];
        let h = cfg().funding_hops as usize;
        assert_eq!(s.funding_transfers.len(), h + 1);

        let mut balances: std::collections::HashMap<String, i64> = Default::default();
        for t in &s.funding_transfers {
            assert_eq!(t.sequence, s.funding_transfers.iter().position(|x| x.sequence == t.sequence).unwrap());
            *balances.entry(t.from.clone()).or_insert(0) -= t.lamports as i64;
            *balances.entry(t.to.clone()).or_insert(0) += t.lamports as i64;
            // Each sender pays its own fee on top of what it forwards.
            *balances.entry(t.from.clone()).or_insert(0) -= TRANSFER_FEE_LAMPORTS as i64;
        }
        // Hops end at zero…
        for hw in &b.hop_wallets {
            assert_eq!(balances[hw], 0, "hop {hw} did not end at zero");
        }
        // …buyer ends at need…
        assert_eq!(balances[&b.buyer], b.funding_needed_lamports as i64);
        assert_eq!(
            b.funding_needed_lamports,
            b.jittered_amount_lamports + (0.015f64 * 1e9).round() as u64
        );
        // …and transfer amounts shrink by exactly one fee per hop.
        let mut amts: Vec<u64> = s.funding_transfers.iter().map(|t| t.lamports).collect();
        amts.sort_unstable_by(|a, b| b.cmp(a)); // descending: creator-first order
        for w in amts.windows(2) {
            assert_eq!(w[0] - w[1], TRANSFER_FEE_LAMPORTS);
        }
    }

    #[test]
    fn schedule_offsets_within_spread() {
        let s = seeded_plan(&[0.5, 0.5, 0.5, 0.5]);
        for b in &s.buyers {
            assert!(b.target_slot_offset <= 3, "offset out of spread");
        }
        // Zero spread => all offsets zero.
        let mut c = cfg();
        c.spread_slots = 0;
        let p = plan_with_buys(&[0.5, 0.5]);
        let s2 = plan_stealth_with_rng(&p, &c, &mut StdRng::seed_from_u64(1)).unwrap();
        assert!(s2.buyers.iter().all(|b| b.target_slot_offset == 0));
    }

    #[test]
    fn seeded_jitter_is_deterministic() {
        // The two-call API pattern depends on this: same seed + same plan
        // => same jittered amounts, so funding built in call 1 matches the
        // bundle built in call 2.
        let a = seeded_plan(&[1.0, 2.0]);
        let b_ = seeded_plan(&[1.0, 2.0]);
        let za: Vec<u64> = a.buyers.iter().map(|x| x.jittered_amount_lamports).collect();
        let zb: Vec<u64> = b_.buyers.iter().map(|x| x.jittered_amount_lamports).collect();
        assert_eq!(za, zb);
        // Different seed => (almost surely) different amounts.
        let mut c = cfg();
        c.amount_seed = Some(43);
        let p = plan_with_buys(&[1.0, 2.0]);
        let d = plan_stealth_with_rng(&p, &c, &mut StdRng::seed_from_u64(43)).unwrap();
        let zd: Vec<u64> = d.buyers.iter().map(|x| x.jittered_amount_lamports).collect();
        assert_ne!(za, zd);
    }

    #[test]
    fn hop_wallets_are_fresh_and_unique() {
        let s = seeded_plan(&[1.0, 1.0]);
        let mut all = Vec::new();
        for b in &s.buyers {
            assert_eq!(b.hop_wallets.len(), 2);
            all.extend(b.hop_wallets.iter().cloned());
        }
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 4, "hop wallets must be unique per buyer");
        // None equals a buyer or the plan creator.
        let p = plan_with_buys(&[1.0, 1.0]);
        for hw in &all {
            assert_ne!(hw, &p.creator);
            for buy in &p.initial_buys {
                assert_ne!(hw, &buy.wallet);
            }
        }
        // signer_keypair resolves creator + every hop.
        let creator_kp = Keypair::new();
        let s2 = {
            let mut p2 = plan_with_buys(&[1.0]);
            p2.creator = creator_kp.pubkey().to_string();
            plan_stealth_with_rng(&p2, &cfg(), &mut StdRng::seed_from_u64(7)).unwrap()
        };
        assert!(s2.signer_keypair(&creator_kp, &creator_kp.pubkey()).is_some());
        let hop0 = Pubkey::from_str(&s2.buyers[0].hop_wallets[0]).unwrap();
        let got = s2.signer_keypair(&creator_kp, &hop0).unwrap();
        assert_eq!(got.pubkey(), hop0);
        assert!(s2
            .signer_keypair(&creator_kp, &Keypair::new().pubkey())
            .is_none());
        // Secrets export round-trips: first 32 bytes are the seed.
        for (pk, sk) in s2.hop_secrets_bs58() {
            let bytes = bs58::decode(&sk).into_vec().unwrap();
            assert_eq!(bytes.len(), 64);
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&bytes[..32]);
            let kp = Keypair::new_from_array(seed);
            assert_eq!(kp.pubkey().to_string(), pk);
        }
    }

    #[test]
    fn total_creator_funding_sums_first_hops() {
        let (p, s) = seeded_plan_and_plan(&[1.0, 2.0]);
        let creator = Pubkey::from_str(&p.creator).unwrap();
        let total = s.total_creator_funding_lamports(&creator);
        let expect: u64 = s
            .buyers
            .iter()
            .map(|b| {
                b.jittered_amount_lamports
                    + (0.015f64 * 1e9).round() as u64
                    + 2 * TRANSFER_FEE_LAMPORTS
            })
            .sum();
        assert_eq!(total, expect);
    }
}
