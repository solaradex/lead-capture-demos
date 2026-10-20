//! Rug-pull / honeypot protection.
//!
//! Pump.fun bonding curves enforce sells on-chain, so classic honeypots
//! (unsellable tokens) are rare here. The real risks this module covers:
//! - **Dev dumps**: the creator selling into their own curve while we hold.
//!   The live loop must treat [`dev_dump_detected`] as an emergency exit.
//! - **Dead curves**: buying after the curve completed/migrated.
//! - **Authority games**: freeze authority set (funds freezable) or mint
//!   authority live (supply inflatable).
//! - **Malformed instructions**: every buy is simulated before submission.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use solana_pubkey::Pubkey;
use tracing::warn;

use crate::decode;
use crate::types::{ChainEvent, EventKind};

/// Policy knobs for pre-buy safety checks.
#[derive(Debug, Clone)]
pub struct SafetyPolicy {
    /// Reject buys when the token has a freeze authority (funds freezable).
    pub require_no_freeze_authority: bool,
    /// Reject buys when mint authority is still live (supply inflatable).
    /// Default false: authority states vary across pump.fun flows; always logged.
    pub require_no_mint_authority: bool,
    /// Allow buying tokens whose bonding curve has completed/migrated.
    /// Default false. Only enable for migration strategies (e.g. pumpswap_migration)
    /// whose entire premise is buying post-migration PumpSwap pools.
    pub allow_migrated_curve: bool,
}

impl Default for SafetyPolicy {
    fn default() -> Self {
        Self {
            require_no_freeze_authority: true,
            require_no_mint_authority: false,
            allow_migrated_curve: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PreBuyReport {
    pub mint: String,
    pub curve_complete: bool,
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
    pub passed: bool,
    pub reasons: Vec<String>,
}

/// Run pre-buy safety checks for a mint. Fails closed on RPC/account errors:
/// if we cannot verify, we do not buy.
pub async fn pre_buy_checks(
    rpc_url: &str,
    mint: &Pubkey,
    policy: &SafetyPolicy,
) -> Result<PreBuyReport> {
    let mut reasons = Vec::new();

    // 1. Curve complete? Never buy a migrated curve — unless the active
    // strategy's entire premise is post-migration pools (pumpswap_migration).
    let curve = decode::bonding_curve_pda(mint);
    let curve_data = fetch_account_data(rpc_url, &curve)
        .await?
        .ok_or_else(|| anyhow!("bonding curve account missing for {mint}"))?;
    let complete = decode::bonding_curve_complete(&curve_data)?;
    if complete && !policy.allow_migrated_curve {
        reasons.push("bonding curve complete (migrated)".to_string());
    }

    // 2. Mint / freeze authorities.
    let mint_data = fetch_account_data(rpc_url, mint)
        .await?
        .ok_or_else(|| anyhow!("mint account missing for {mint}"))?;
    let auth = decode::parse_mint_authorities(&mint_data)?;
    if policy.require_no_freeze_authority && auth.freeze_authority.is_some() {
        reasons.push("freeze authority is set".to_string());
    }
    if policy.require_no_mint_authority && auth.mint_authority.is_some() {
        reasons.push("mint authority is live".to_string());
    }

    let passed = reasons.is_empty();
    if !passed {
        warn!(mint = %mint, ?reasons, "pre-buy safety checks failed");
    }
    Ok(PreBuyReport {
        mint: mint.to_string(),
        curve_complete: complete,
        mint_authority: auth.mint_authority.map(|p| p.to_string()),
        freeze_authority: auth.freeze_authority.map(|p| p.to_string()),
        passed,
        reasons,
    })
}

/// The dev-dump tripwire: true when the creator sells into their own curve.
/// The live loop should emergency-exit any open position in `event.mint`.
pub fn dev_dump_detected(event: &ChainEvent) -> bool {
    event.kind == EventKind::CreatorSell
}

/// Fraction of bonding-curve supply still available (1.0 = fresh launch,
/// 0.0 = fully consumed). Derived from real_token_reserves / token_total_supply.
pub fn curve_remaining_fraction(account_data: &[u8]) -> Result<f64> {
    if account_data.len() < 48 {
        return Err(anyhow!(
            "bonding curve account too short: {}",
            account_data.len()
        ));
    }
    let real_token_reserves =
        u64::from_le_bytes(account_data[24..32].try_into().unwrap());
    let token_total_supply =
        u64::from_le_bytes(account_data[40..48].try_into().unwrap());
    if token_total_supply == 0 {
        return Ok(0.0);
    }
    Ok(real_token_reserves as f64 / token_total_supply as f64)
}

/// True when the curve is nearly consumed (>= `consumed_threshold_pct`).
/// The live loop should exit open positions BEFORE migration: after the
/// token graduates, bonding-curve sells stop working and a separate
/// PumpSwap path is required.
pub fn graduation_imminent(
    account_data: &[u8],
    consumed_threshold_pct: f64,
) -> Result<bool> {
    let remaining = curve_remaining_fraction(account_data)?;
    Ok((1.0 - remaining) * 100.0 >= consumed_threshold_pct)
}

/// Outcome of a pre-submit simulation.
#[derive(Debug, Clone)]
pub struct SimulationOutcome {
    pub success: bool,
    pub logs: Vec<String>,
    pub error: Option<String>,
}

/// Simulate a base64 transaction without submitting it. Catches malformed
/// instructions, missing accounts, and curve-state rejections before any
/// SOL moves. `sigVerify=false`: we simulate our own locally-signed tx.
pub async fn simulate_transaction(
    rpc_url: &str,
    base64_tx: &str,
) -> Result<SimulationOutcome> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "simulateTransaction",
        "params": [base64_tx, {"encoding": "base64", "sigVerify": false, "commitment": "confirmed"}],
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context("simulateTransaction request")?
        .error_for_status()
        .context("simulateTransaction http error")?
        .json()
        .await
        .context("parsing simulateTransaction response")?;
    if let Some(err) = resp.get("error") {
        return Err(anyhow!("simulateTransaction rpc error: {err}"));
    }
    let sim_err = resp.pointer("/result/value/err");
    let logs: Vec<String> = resp
        .pointer("/result/value/logs")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let success = sim_err.map(|v| v.is_null()).unwrap_or(false);
    Ok(SimulationOutcome {
        success,
        logs,
        error: sim_err.and_then(|v| {
            if v.is_null() {
                None
            } else {
                Some(v.to_string())
            }
        }),
    })
}

/// Public wrapper: fetch raw account data (base64) for curve monitoring.
pub async fn fetch_curve_data(rpc_url: &str, pubkey: &Pubkey) -> Result<Option<Vec<u8>>> {
    fetch_account_data(rpc_url, pubkey).await
}

async fn fetch_account_data(rpc_url: &str, pubkey: &Pubkey) -> Result<Option<Vec<u8>>> {    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getAccountInfo",
        "params": [pubkey.to_string(), {"encoding": "base64", "commitment": "confirmed"}],
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context("getAccountInfo request")?
        .error_for_status()
        .context("getAccountInfo http error")?
        .json()
        .await
        .context("parsing getAccountInfo response")?;
    match resp.pointer("/result/value/data/0").and_then(|v| v.as_str()) {
        Some(b64) => Ok(Some(
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .context("decoding account data")?,
        )),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tripwire_fires_on_creator_sell_only() {
        let sell = ChainEvent {
            event_id: "e".into(),
            slot: 1,
            signature: "s".into(),
            program_id: "p".into(),
            kind: EventKind::CreatorSell,
            mint: Some("m".into()),
            creator: Some("c".into()),
            user: Some("u".into()),
            creator_buy_sol: Some(0.5),
            trade_price_sol_per_token: Some(0.000_001),
            virtual_sol_reserves: None,
            virtual_token_reserves: None,
            fee_basis_points: None,
            creator_fee_basis_points: None,
            observed_at_ms: 0,
        };
        assert!(dev_dump_detected(&sell));
        let buy = ChainEvent { kind: EventKind::CreatorBuy, ..sell.clone() };
        assert!(!dev_dump_detected(&buy));
    }

    #[test]
    fn default_policy_blocks_freeze_authority() {
        let p = SafetyPolicy::default();
        assert!(p.require_no_freeze_authority);
    }
}
