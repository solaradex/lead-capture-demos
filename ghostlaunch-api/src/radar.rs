//! Post-launch radar: monitor a Pump.fun token's bonding curve for trades,
//! detect snipers and whales, and summarize holder distribution.
//!
//! Read-only: fetches signatures + transactions via JSON-RPC and decodes
//! Pump TradeEvents from program logs. Never submits transactions, never
//! needs private keys.
//!
//! Detection heuristics (all configurable):
//! - **Sniper**: a buy landing within `sniper_slot_window` slots of the
//!   token's creation slot (default 2). Bots that buy in block 0-2 are
//!   overwhelmingly automated snipers, not humans.
//! - **Whale**: any single buy of at least `whale_threshold_lamports`
//!   (default 5 SOL).
//! - **Holders**: net token balance per wallet from the decoded trade tape
//!   (buys add, sells subtract). Top-10 share is computed over tracked
//!   positive balances.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use solana_pubkey::Pubkey;
use solana_sniper::decode;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// Anchor emits events as `Program data: <base64>` log lines.
const PROGRAM_DATA_PREFIX: &str = "Program data: ";

/// Tunable radar parameters.
#[derive(Debug, Clone)]
pub struct RadarConfig {
    pub rpc_url: String,
    /// Buys within create_slot + this many slots count as snipers.
    pub sniper_slot_window: u64,
    /// Single buys of at least this many lamports count as whales.
    pub whale_threshold_lamports: u64,
    /// Max signatures pulled from getSignaturesForAddress per snapshot.
    pub max_signatures: usize,
}

impl Default for RadarConfig {
    fn default() -> Self {
        Self {
            rpc_url: String::new(),
            sniper_slot_window: 2,
            whale_threshold_lamports: 5 * LAMPORTS_PER_SOL,
            max_signatures: 1000,
        }
    }
}

impl RadarConfig {
    pub fn validate(&self) -> Result<()> {
        if self.rpc_url.is_empty() {
            anyhow::bail!("radar rpc_url must not be empty");
        }
        if self.max_signatures == 0 || self.max_signatures > 1000 {
            anyhow::bail!("max_signatures must be 1..=1000 (RPC limit)");
        }
        Ok(())
    }
}

/// One decoded trade on the bonding curve.
#[derive(Debug, Clone)]
pub struct TradeRecord {
    pub signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    pub wallet: Pubkey,
    pub is_buy: bool,
    pub sol_lamports: u64,
    pub token_amount: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SniperHit {
    pub wallet: String,
    pub slot: u64,
    pub slots_after_create: u64,
    pub sol_lamports: u64,
    pub token_amount: u64,
    pub signature: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WhaleHit {
    pub wallet: String,
    pub slot: u64,
    pub is_buy: bool,
    pub sol_lamports: u64,
    pub token_amount: u64,
    pub signature: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HolderInfo {
    pub wallet: String,
    pub net_tokens: u64,
    pub buys: u32,
    pub sells: u32,
    /// Share of tracked positive supply, basis points.
    pub share_bps: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GrowthPoint {
    pub block_time: i64,
    pub holders: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RadarReport {
    pub mint: String,
    pub bonding_curve: String,
    pub create_slot: Option<u64>,
    pub trade_count: usize,
    pub buy_count: usize,
    pub sell_count: usize,
    /// Total SOL spent on buys (lamports).
    pub volume_sol_lamports: u64,
    pub snipers: Vec<SniperHit>,
    pub whales: Vec<WhaleHit>,
    pub holder_count: usize,
    pub top_holders: Vec<HolderInfo>,
    pub holder_growth: Vec<GrowthPoint>,
    /// Wallets the caller marked as known (creator / initial buyers).
    pub known_wallets: Vec<String>,
}

// ---------------------------------------------------------------------------
// RPC
// ---------------------------------------------------------------------------

async fn rpc_call(
    rpc_url: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": method, "params": params,
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("{method} request"))?
        .error_for_status()
        .with_context(|| format!("{method} http error"))?
        .json()
        .await
        .with_context(|| format!("parsing {method} response"))?;
    if let Some(err) = resp.get("error") {
        return Err(anyhow::anyhow!("{method} rpc error: {err}"));
    }
    Ok(resp)
}

/// Decode every Pump TradeEvent for `mint` out of one transaction's logs.
fn trades_from_logs(
    logs: &[String],
    mint: &Pubkey,
    signature: &str,
    slot: u64,
    block_time: Option<i64>,
) -> Vec<TradeRecord> {
    let mut out = Vec::new();
    for log in logs {
        let b64 = match log.strip_prefix(PROGRAM_DATA_PREFIX) {
            Some(b) => b.trim(),
            None => continue,
        };
        let payload = match B64.decode(b64) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let ev = match decode::parse_trade_event_log(&payload) {
            Ok(Some(ev)) => ev,
            _ => continue,
        };
        if ev.mint != *mint {
            continue;
        }
        out.push(TradeRecord {
            signature: signature.to_string(),
            slot,
            block_time,
            wallet: ev.user,
            is_buy: ev.is_buy,
            sol_lamports: ev.sol_amount_lamports,
            token_amount: ev.token_amount,
        });
    }
    out
}

/// Fetch every trade on the mint's bonding curve (newest signatures first
/// from RPC; returned oldest-first). Also returns the creation slot — the
/// earliest slot seen, which is the create transaction for a fresh curve.
pub async fn fetch_trades(
    cfg: &RadarConfig,
    mint: &Pubkey,
) -> Result<(Vec<TradeRecord>, Option<u64>)> {
    cfg.validate()?;
    let curve = decode::bonding_curve_pda(mint);

    let sigs = rpc_call(
        &cfg.rpc_url,
        "getSignaturesForAddress",
        serde_json::json!([
            curve.to_string(),
            {"limit": cfg.max_signatures, "commitment": "confirmed"}
        ]),
    )
    .await?;
    let entries = sigs
        .pointer("/result")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // The bonding curve account is born in the create transaction, so the
    // OLDEST signature touching it is the true creation slot. (The create
    // itself emits no TradeEvent, so the trade tape alone can't tell us.)
    // Signatures come newest-first; the last entry is the oldest. If the
    // result was truncated by `limit`, this is a lower bound.
    let address_create_slot = entries
        .last()
        .and_then(|e| e.get("slot"))
        .and_then(|s| s.as_u64());

    let mut trades = Vec::new();
    for entry in entries {
        // Skip failed transactions — their events never happened.
        if entry.get("err").is_some_and(|e| !e.is_null()) {
            continue;
        }
        let signature = match entry.get("signature").and_then(|s| s.as_str()) {
            Some(s) => s,
            None => continue,
        };
        let tx = rpc_call(
            &cfg.rpc_url,
            "getTransaction",
            serde_json::json!([
                signature,
                {"encoding": "json", "commitment": "confirmed", "maxSupportedTransactionVersion": 0}
            ]),
        )
        .await?;
        if tx.pointer("/result").is_none_or(|v| v.is_null()) {
            continue;
        }
        let slot = tx.pointer("/result/slot").and_then(|s| s.as_u64()).unwrap_or(0);
        if tx
            .pointer("/result/meta/err")
            .is_some_and(|e| !e.is_null())
        {
            continue;
        }
        let block_time = tx
            .pointer("/result/blockTime")
            .and_then(|t| t.as_i64());
        let logs: Vec<String> = tx
            .pointer("/result/meta/logMessages")
            .and_then(|l| l.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        trades.extend(trades_from_logs(&logs, mint, signature, slot, block_time));
        // Be gentle on the RPC: devnet rate limits are real.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }

    // Oldest first so "slot 0" semantics and growth curves read naturally.
    trades.sort_by_key(|t| (t.slot, t.signature.clone()));
    let trade_min_slot = trades.iter().map(|t| t.slot).min();
    // Prefer the true creation slot from the address history; fall back to
    // the earliest trade when the address lookup came back empty.
    let create_slot = match (address_create_slot, trade_min_slot) {
        (Some(a), Some(t)) => Some(a.min(t)),
        (Some(a), None) => Some(a),
        (None, t) => t,
    };
    Ok((trades, create_slot))
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

/// Build a RadarReport from a trade tape. `known_wallets` (creator, initial
/// buyers) are excluded from sniper flags and echoed in the report.
pub fn analyze_trades(
    mint: &Pubkey,
    trades: &[TradeRecord],
    create_slot: Option<u64>,
    cfg: &RadarConfig,
    known_wallets: &HashSet<Pubkey>,
) -> RadarReport {
    let curve = decode::bonding_curve_pda(mint);
    let mut snipers = Vec::new();
    let mut whales = Vec::new();

    for t in trades {
        if t.is_buy {
            if let Some(cs) = create_slot {
                if t.slot <= cs.saturating_add(cfg.sniper_slot_window)
                    && !known_wallets.contains(&t.wallet)
                {
                    snipers.push(SniperHit {
                        wallet: t.wallet.to_string(),
                        slot: t.slot,
                        slots_after_create: t.slot.saturating_sub(cs),
                        sol_lamports: t.sol_lamports,
                        token_amount: t.token_amount,
                        signature: t.signature.clone(),
                    });
                }
            }
            if t.sol_lamports >= cfg.whale_threshold_lamports {
                whales.push(WhaleHit {
                    wallet: t.wallet.to_string(),
                    slot: t.slot,
                    is_buy: true,
                    sol_lamports: t.sol_lamports,
                    token_amount: t.token_amount,
                    signature: t.signature.clone(),
                });
            }
        }
    }
    // A whale-sized SELL is also worth flagging (dump warning).
    for t in trades.iter().filter(|t| !t.is_buy) {
        if t.sol_lamports >= cfg.whale_threshold_lamports {
            whales.push(WhaleHit {
                wallet: t.wallet.to_string(),
                slot: t.slot,
                is_buy: false,
                sol_lamports: t.sol_lamports,
                token_amount: t.token_amount,
                signature: t.signature.clone(),
            });
        }
    }

    // Holder tape: net token balance per wallet.
    #[derive(Default)]
    struct Acc {
        net: i128,
        buys: u32,
        sells: u32,
    }
    let mut acc: HashMap<Pubkey, Acc> = HashMap::new();
    let mut first_seen: HashMap<Pubkey, i64> = HashMap::new();
    for t in trades {
        let a = acc.entry(t.wallet).or_default();
        if t.is_buy {
            a.net += t.token_amount as i128;
            a.buys += 1;
        } else {
            a.net -= t.token_amount as i128;
            a.sells += 1;
        }
        if let Some(bt) = t.block_time {
            first_seen
                .entry(t.wallet)
                .and_modify(|e| *e = (*e).min(bt))
                .or_insert(bt);
        }
    }
    let positive_supply: u128 = acc
        .values()
        .map(|a| a.net.max(0) as u128)
        .sum();
    let mut holders: Vec<HolderInfo> = acc
        .iter()
        .filter(|(_, a)| a.net > 0)
        .map(|(w, a)| HolderInfo {
            wallet: w.to_string(),
            net_tokens: a.net as u64,
            buys: a.buys,
            sells: a.sells,
            share_bps: if positive_supply > 0 {
                (a.net as u128 * 10_000 / positive_supply) as u64
            } else {
                0
            },
        })
        .collect();
    holders.sort_by(|a, b| b.net_tokens.cmp(&a.net_tokens));
    holders.truncate(10);

    // Holder growth: cumulative unique wallets by first-seen time.
    let mut seen_order: Vec<(i64, Pubkey)> = first_seen.into_iter().map(|(w, t)| (t, w)).collect();
    seen_order.sort();
    let mut growth = Vec::new();
    let mut seen = HashSet::new();
    for (bt, w) in seen_order {
        seen.insert(w);
        // One point per new wallet; callers can downsample.
        growth.push(GrowthPoint {
            block_time: bt,
            holders: seen.len(),
        });
    }

    RadarReport {
        mint: mint.to_string(),
        bonding_curve: curve.to_string(),
        create_slot,
        trade_count: trades.len(),
        buy_count: trades.iter().filter(|t| t.is_buy).count(),
        sell_count: trades.iter().filter(|t| !t.is_buy).count(),
        volume_sol_lamports: trades
            .iter()
            .filter(|t| t.is_buy)
            .map(|t| t.sol_lamports as u128)
            .sum::<u128>()
            .min(u64::MAX as u128) as u64,
        snipers,
        whales,
        holder_count: acc.values().filter(|a| a.net > 0).count(),
        top_holders: holders,
        holder_growth: growth,
        known_wallets: {
            let mut k: Vec<String> = known_wallets.iter().map(|w| w.to_string()).collect();
            k.sort();
            k
        },
    }
}

/// Convenience: fetch + analyze in one call.
pub async fn snapshot(
    cfg: &RadarConfig,
    mint: &Pubkey,
    known_wallets: &HashSet<Pubkey>,
) -> Result<RadarReport> {
    let (trades, create_slot) = fetch_trades(cfg, mint).await?;
    Ok(analyze_trades(mint, &trades, create_slot, cfg, known_wallets))
}

/// Parse a comma-separated wallet list (API `known` param).
pub fn parse_wallet_list(s: &str) -> Result<HashSet<Pubkey>> {
    s.split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| {
            Pubkey::from_str(p).with_context(|| format!("bad wallet pubkey: {p}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(wallet: &str, slot: u64, is_buy: bool, sol: u64, tokens: u64) -> TradeRecord {
        TradeRecord {
            signature: format!("sig{slot}{}", &wallet[..4]),
            slot,
            block_time: Some(1_700_000_000 + slot as i64 * 2),
            wallet: Pubkey::from_str(wallet).unwrap(),
            is_buy,
            sol_lamports: sol,
            token_amount: tokens,
        }
    }

    fn wallets() -> (Pubkey, Pubkey, Pubkey, Pubkey) {
        (
            Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap(),
            Pubkey::from_str("2gzurBnwbG8CamD5o7Pj2fYUCTMoaDDUdoUw7HJGS4LM").unwrap(),
            Pubkey::from_str("9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7").unwrap(),
            Pubkey::from_str("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD").unwrap(),
        )
    }

    fn test_mint() -> Pubkey {
        Pubkey::from_str("Bp1ErGRE9pQAMit1eayzXHyfN1vvujgXuPkVeheD6Hqi").unwrap()
    }

    #[test]
    fn sniper_window_flags_early_buys_only() {
        let (creator, bot, organic, _) = wallets();
        let cfg = RadarConfig {
            sniper_slot_window: 2,
            ..Default::default()
        };
        let known: HashSet<Pubkey> = [creator].into_iter().collect();
        let trades = vec![
            trade(&creator.to_string(), 100, true, 50_000_000, 1_000), // known: excluded
            trade(&bot.to_string(), 101, true, 30_000_000, 600),      // sniper
            trade(&organic.to_string(), 120, true, 10_000_000, 200),   // too late
        ];
        let r = analyze_trades(&test_mint(), &trades, Some(100), &cfg, &known);
        assert_eq!(r.snipers.len(), 1);
        assert_eq!(r.snipers[0].wallet, bot.to_string());
        assert_eq!(r.snipers[0].slots_after_create, 1);
    }

    #[test]
    fn whale_threshold_flags_big_buys_and_big_sells() {
        let (creator, _, organic, whale) = wallets();
        let cfg = RadarConfig {
            whale_threshold_lamports: 100_000_000, // 0.1 SOL
            ..Default::default()
        };
        let known: HashSet<Pubkey> = [creator].into_iter().collect();
        let trades = vec![
            trade(&organic.to_string(), 110, true, 50_000_000, 500),
            trade(&whale.to_string(), 111, true, 500_000_000, 5_000),
            trade(&whale.to_string(), 130, false, 200_000_000, 2_000),
        ];
        let r = analyze_trades(&test_mint(), &trades, Some(100), &cfg, &known);
        assert_eq!(r.whales.len(), 2);
        assert!(r.whales.iter().any(|w| w.is_buy));
        assert!(r.whales.iter().any(|w| !w.is_buy));
    }

    #[test]
    fn holder_math_nets_buys_minus_sells() {
        let (creator, _, organic, _) = wallets();
        let cfg = RadarConfig::default();
        let known: HashSet<Pubkey> = [creator].into_iter().collect();
        let trades = vec![
            trade(&organic.to_string(), 110, true, 50_000_000, 1_000),
            trade(&organic.to_string(), 120, true, 50_000_000, 1_000),
            trade(&organic.to_string(), 130, false, 25_000_000, 400),
        ];
        let r = analyze_trades(&test_mint(), &trades, Some(100), &cfg, &known);
        assert_eq!(r.holder_count, 1);
        assert_eq!(r.top_holders.len(), 1);
        assert_eq!(r.top_holders[0].net_tokens, 1_600);
        assert_eq!(r.top_holders[0].share_bps, 10_000);
        assert_eq!(r.buy_count, 2);
        assert_eq!(r.sell_count, 1);
        assert_eq!(r.volume_sol_lamports, 100_000_000);
    }

    #[test]
    fn holder_growth_is_cumulative() {
        let (creator, bot, organic, whale) = wallets();
        let cfg = RadarConfig::default();
        let known: HashSet<Pubkey> = [creator].into_iter().collect();
        let trades = vec![
            trade(&bot.to_string(), 101, true, 1, 10),
            trade(&organic.to_string(), 110, true, 1, 10),
            trade(&whale.to_string(), 120, true, 1, 10),
        ];
        let r = analyze_trades(&test_mint(), &trades, Some(100), &cfg, &known);
        let counts: Vec<usize> = r.holder_growth.iter().map(|g| g.holders).collect();
        assert_eq!(counts, vec![1, 2, 3]);
    }

    #[test]
    fn config_validation_rejects_bad_input() {
        let mut cfg = RadarConfig::default();
        assert!(cfg.validate().is_err()); // empty rpc_url
        cfg.rpc_url = "https://example.com".into();
        assert!(cfg.validate().is_ok());
        cfg.max_signatures = 0;
        assert!(cfg.validate().is_err());
        cfg.max_signatures = 1001;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn wallet_list_parsing() {
        let set = parse_wallet_list("So11111111111111111111111111111111111111112, 2gzurBnwbG8CamD5o7Pj2fYUCTMoaDDUdoUw7HJGS4LM").unwrap();
        assert_eq!(set.len(), 2);
        assert!(parse_wallet_list("notakey").is_err());
        assert!(parse_wallet_list("").unwrap().is_empty());
    }

    #[test]
    fn trade_event_log_decoding_roundtrip() {
        // Build a synthetic TradeEvent payload and confirm the tape parser
        // extracts it: discriminator + mint + sol + tokens + is_buy + user +
        // timestamp (minimal, no extended fields).
        let mint = test_mint();
        let user = Pubkey::from_str("2gzurBnwbG8CamD5o7Pj2fYUCTMoaDDUdoUw7HJGS4LM").unwrap();
        let mut payload = decode::DISC_TRADE_EVENT.to_vec();
        payload.extend_from_slice(mint.as_ref());
        payload.extend_from_slice(&42_000_000u64.to_le_bytes());
        payload.extend_from_slice(&9_000u64.to_le_bytes());
        payload.push(1u8); // is_buy = true
        payload.extend_from_slice(user.as_ref());
        payload.extend_from_slice(&1_700_000_001i64.to_le_bytes());
        let b64 = B64.encode(&payload);
        let logs = vec![format!("Program data: {b64}")];
        let recs = trades_from_logs(&logs, &mint, "sig1", 500, Some(1_700_000_001));
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].wallet, user);
        assert!(recs[0].is_buy);
        assert_eq!(recs[0].sol_lamports, 42_000_000);
        assert_eq!(recs[0].token_amount, 9_000);
        // Wrong mint => filtered out.
        let other = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
        assert!(trades_from_logs(&logs, &other, "sig1", 500, None).is_empty());
        // Non-event data => ignored.
        let junk = vec!["Program data: aGVsbG8=".to_string()];
        assert!(trades_from_logs(&junk, &mint, "sig1", 500, None).is_empty());
    }
}
