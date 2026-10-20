//! Durable position + paper-P&L state.
//!
//! Positions used to live only in memory: any restart (cron stop/start,
//! crash, deploy) silently orphaned them and reset the paper P&L scoreboard.
//! This module snapshots open positions and the P&L tracker to a JSON file
//! (atomic write via temp file + rename) and restores them at startup.
//!
//! Note: restored positions resume normal event-driven management. In live
//! mode the operator should still reconcile on-chain balances before trusting
//! a restored `token_amount` (see the TODO in accounting.rs).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::paper_pnl::PnlSnapshot;

/// One position, with pubkeys as base58 strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedPosition {
    pub mint: String,
    pub creator: String,
    pub token_program: String,
    pub fee_recipient: String,
    pub entry_price: f64,
    pub peak_price: f64,
    pub token_amount: u64,
    pub sol_invested: f64,
    pub tp_tier: u8,
    pub remaining_fraction: f64,
    pub exit_failed: bool,
    pub failed_exit_reason: Option<String>,
    pub last_event_unix: u64,
    /// True when entered via the raydium_migration strategy. Defaults false
    /// for state files written before this field existed.
    #[serde(default)]
    pub via_raydium: bool,
}

/// Everything needed to resume the loop after a restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotState {
    pub positions: Vec<PersistedPosition>,
    pub pnl: PnlSnapshot,
}

/// Atomically write the current state. Best-effort: callers log failures.
pub fn save_state(path: &str, state: &BotState) -> Result<()> {
    let tmp = format!("{path}.tmp");
    let json = serde_json::to_string_pretty(state).context("serialize bot state")?;
    std::fs::write(&tmp, json).with_context(|| format!("write {tmp}"))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename {tmp} -> {path}"))?;
    Ok(())
}

/// Load previously saved state. Returns `Ok(None)` when no state file exists
/// yet; a corrupt file is an error so startup can shout about it.
pub fn load_state(path: &str) -> Result<Option<BotState>> {
    if !Path::new(path).exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    let state: BotState =
        serde_json::from_str(&raw).with_context(|| format!("parse {path}"))?;
    Ok(Some(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paper_pnl::PaperPnl;
    use crate::risk::ExitReason;

    fn sample_state() -> BotState {
        let mut pnl = PaperPnl::new(2.0);
        pnl.record_entry("mint1", 0.049);
        pnl.record_entry_price("mint1", 0.001);
        pnl.record_exit("mint1", Some(0.002), 1.0, ExitReason::TakeProfit(1));
        pnl.record_entry("mint2", 0.049); // still open
        BotState {
            positions: vec![PersistedPosition {
                mint: "mint2".to_string(),
                creator: "creator1".to_string(),
                token_program: "prog1".to_string(),
                fee_recipient: "fee1".to_string(),
                entry_price: 0.001,
                peak_price: 0.0015,
                token_amount: 49000,
                sol_invested: 0.049,
                tp_tier: 0,
                remaining_fraction: 1.0,
                exit_failed: false,
                failed_exit_reason: None,
                last_event_unix: 1_700_000_000,
                via_raydium: true,
            }],
            pnl: pnl.snapshot(),
        }
    }

    #[test]
    fn state_round_trips_through_json() {
        let state = sample_state();
        let json = serde_json::to_string(&state).unwrap();
        let back: BotState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.positions.len(), 1);
        assert_eq!(back.positions[0].mint, "mint2");
        assert_eq!(back.positions[0].token_amount, 49000);
        assert!((back.positions[0].peak_price - 0.0015).abs() < 1e-12);
        assert!(back.positions[0].via_raydium);
        assert_eq!(back.pnl.closed.len(), 1);
        assert_eq!(back.pnl.open.len(), 1);
        assert!((back.pnl.slippage_pct - 2.0).abs() < 1e-12);
    }

    #[test]
    fn old_state_without_via_raydium_still_parses() {
        // State files written before via_raydium existed must load with
        // the field defaulting to false.
        let json = r#"{"mint":"m","creator":"c","token_program":"t",
            "fee_recipient":"f","entry_price":0.0,"peak_price":0.0,
            "token_amount":0,"sol_invested":0.1,"tp_tier":0,
            "remaining_fraction":1.0,"exit_failed":false,
            "failed_exit_reason":null,"last_event_unix":1}"#;
        let pp: PersistedPosition = serde_json::from_str(json).unwrap();
        assert!(!pp.via_raydium);
    }

    #[test]
    fn save_and_load_use_the_filesystem_atomically() {
        let dir = std::env::temp_dir().join("remirun-persist-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let path_str = path.to_string_lossy().to_string();

        // No file yet -> None, not an error.
        assert!(load_state(&path_str).unwrap().is_none());

        save_state(&path_str, &sample_state()).unwrap();
        assert!(!Path::new(&format!("{path_str}.tmp")).exists());
        let back = load_state(&path_str).unwrap().unwrap();
        assert_eq!(back.positions[0].mint, "mint2");
        assert_eq!(back.pnl.closed.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_file_is_an_error_not_silent_empty() {
        let dir = std::env::temp_dir().join("remirun-persist-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let path_str = path.to_string_lossy().to_string();
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_state(&path_str).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
