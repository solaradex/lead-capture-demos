//! Launch plan: the declarative input for a bundled Pump.fun launch.
//!
//! A plan describes the token metadata and the initial buys that should land
//! in the same Jito bundle as the creation transaction. Loaded from JSON so
//! Brad (or a future web UI) can prepare launches without touching code.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;
use std::collections::HashSet;
use std::str::FromStr;

/// One initial buy to include in the launch bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitialBuy {
    /// Buyer wallet address (base58). Its keypair must be supplied via CLI.
    pub wallet: String,
    /// SOL to spend on this buy.
    pub sol_amount: f64,
}

/// Everything needed to execute one bundled launch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchPlan {
    pub token_name: String,
    pub token_symbol: String,
    /// Metadata URI (IPFS/Arweave JSON with image, description, links).
    pub token_uri: String,
    /// Creator wallet address (base58). Pays for creation, signs the bundle.
    pub creator: String,
    /// Initial buys, executed in order right after creation.
    /// Jito bundles hold at most 5 transactions: 1 create + up to 4 buys.
    pub initial_buys: Vec<InitialBuy>,
    /// Optional Jito tip in lamports (default 0 = no tip transaction).
    #[serde(default)]
    pub tip_lamports: u64,
    /// Slippage tolerance for the initial buys, in basis points (default 500 = 5%).
    #[serde(default = "default_slippage_bps")]
    pub slippage_bps: u16,
    /// Optional stealth distribution: multi-hop funding, jittered amounts,
    /// slot-spread buys. Absent = classic single-bundle behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stealth: Option<crate::stealth::StealthConfig>,
}

fn default_slippage_bps() -> u16 {
    500
}

/// Hard cap: 1 create tx + this many buy txs per Jito bundle.
pub const MAX_INITIAL_BUYS: usize = 4;

impl LaunchPlan {
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading plan file {path}"))?;
        let plan: LaunchPlan =
            serde_json::from_str(&raw).context("parsing plan JSON")?;
        plan.validate()?;
        Ok(plan)
    }

    pub fn validate(&self) -> Result<()> {
        if self.token_name.trim().is_empty() {
            return Err(anyhow!("token_name must not be empty"));
        }
        if self.token_name.len() > 32 {
            return Err(anyhow!("token_name must be <= 32 chars (pump.fun limit)"));
        }
        if self.token_symbol.trim().is_empty() {
            return Err(anyhow!("token_symbol must not be empty"));
        }
        if self.token_symbol.len() > 10 {
            return Err(anyhow!("token_symbol must be <= 10 chars (pump.fun limit)"));
        }
        if self.token_uri.trim().is_empty() {
            return Err(anyhow!("token_uri must not be empty"));
        }
        Pubkey::from_str(&self.creator)
            .map_err(|_| anyhow!("creator is not a valid pubkey"))?;
        if self.initial_buys.is_empty() {
            return Err(anyhow!("initial_buys must contain at least one buy"));
        }
        if self.initial_buys.len() > MAX_INITIAL_BUYS {
            return Err(anyhow!(
                "at most {MAX_INITIAL_BUYS} initial buys per bundle (Jito limit is 5 txs: 1 create + buys)"
            ));
        }
        if self.slippage_bps > 10_000 {
            return Err(anyhow!("slippage_bps must be <= 10000"));
        }
        if let Some(s) = &self.stealth {
            s.validate()?;
        }
        let mut seen = HashSet::new();
        for (i, buy) in self.initial_buys.iter().enumerate() {
            Pubkey::from_str(&buy.wallet)
                .map_err(|_| anyhow!("initial_buys[{i}].wallet is not a valid pubkey"))?;
            if !buy.sol_amount.is_finite() || buy.sol_amount <= 0.0 {
                return Err(anyhow!("initial_buys[{i}].sol_amount must be > 0"));
            }
            if buy.sol_amount > 1_000.0 {
                return Err(anyhow!(
                    "initial_buys[{i}].sol_amount looks wrong (>1000 SOL); refusing"
                ));
            }
            if !seen.insert(&buy.wallet) {
                return Err(anyhow!(
                    "initial_buys[{i}].wallet is duplicated; merge into one entry"
                ));
            }
        }
        Ok(())
    }

    /// Total SOL the plan intends to spend on initial buys.
    pub fn total_buy_sol(&self) -> f64 {
        self.initial_buys.iter().map(|b| b.sol_amount).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_plan() -> LaunchPlan {
        LaunchPlan {
            token_name: "Test Token".into(),
            token_symbol: "TEST".into(),
            token_uri: "https://example.com/meta.json".into(),
            creator: "So11111111111111111111111111111111111111112".into(),
            initial_buys: vec![InitialBuy {
                wallet: "So11111111111111111111111111111111111111112".into(),
                sol_amount: 1.0,
            }],
            tip_lamports: 0,
            slippage_bps: 500,
            stealth: None,
        }
    }

    #[test]
    fn accepts_good_plan() {
        assert!(good_plan().validate().is_ok());
    }

    #[test]
    fn rejects_empty_name() {
        let mut p = good_plan();
        p.token_name = "  ".into();
        assert!(p.validate().is_err());
    }

    #[test]
    fn rejects_too_many_buys() {
        let mut p = good_plan();
        p.initial_buys = (0..5)
            .map(|_| InitialBuy {
                wallet: "So11111111111111111111111111111111111111112".into(),
                sol_amount: 0.1,
            })
            .collect();
        assert!(p.validate().is_err());
    }

    #[test]
    fn rejects_bad_wallet() {
        let mut p = good_plan();
        p.initial_buys[0].wallet = "not-a-pubkey".into();
        assert!(p.validate().is_err());
    }

    #[test]
    fn rejects_zero_amount() {
        let mut p = good_plan();
        p.initial_buys[0].sol_amount = 0.0;
        assert!(p.validate().is_err());
    }
}
