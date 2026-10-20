//! Honest paper-trading P&L tracker.
//!
//! Paper fills are simulated from observed trade prints. To avoid the classic
//! trap of optimistic paper results, every fill models:
//! - entry slippage (we move the price when we buy),
//! - exit slippage (we move the price when we sell, worse in a dump),
//! - a Solana transaction fee per entry and per exit.
//!
//! Token amounts are implied from SOL invested and observed prices, since
//! paper mode never holds real tokens. Partial exits (tiered take-profits)
//! bank realized proceeds; the trade closes when the remainder exits.

use crate::risk::ExitReason;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Lamports per SOL.
const LAMPORTS: f64 = 1_000_000_000.0;
/// Typical Solana transaction fee, in SOL.
const TX_FEE_SOL: f64 = 5_000.0 / LAMPORTS;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OpenPaper {
    sol_invested: f64,
    entry_price: Option<f64>,
    /// Fraction of the original position already sold.
    exited_fraction: f64,
    /// SOL banked from partial exits (after slippage/fees).
    realized_sol: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosedTrade {
    pub mint: String,
    pub sol_in: f64,
    pub sol_out: f64,
    pub net_sol: f64,
    pub multiple: f64,
    pub final_reason: ExitReason,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperPnl {
    /// Slippage applied to each side, e.g. 0.02 = 2%.
    slippage: f64,
    open: HashMap<String, OpenPaper>,
    closed: Vec<ClosedTrade>,
}

/// Serializable snapshot of the P&L tracker, for persistence across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlSnapshot {
    pub slippage_pct: f64,
    pub(crate) open: HashMap<String, OpenPaper>,
    pub(crate) closed: Vec<ClosedTrade>,
}

impl PaperPnl {
    pub fn new(slippage_pct: f64) -> Self {
        Self {
            slippage: (slippage_pct / 100.0).clamp(0.0, 0.5),
            open: HashMap::new(),
            closed: Vec::new(),
        }
    }

    /// Record a paper buy. Cost basis includes one tx fee.
    pub fn record_entry(&mut self, mint: &str, sol_invested: f64) {
        self.open.insert(
            mint.to_string(),
            OpenPaper {
                sol_invested,
                entry_price: None,
                exited_fraction: 0.0,
                realized_sol: 0.0,
            },
        );
    }

    /// Seed the entry price from the first observed trade print.
    pub fn record_entry_price(&mut self, mint: &str, price: f64) {
        if let Some(o) = self.open.get_mut(mint) {
            if o.entry_price.is_none() && price > 0.0 {
                o.entry_price = Some(price);
            }
        }
    }

    /// Implied token balance from SOL invested, after entry slippage.
    fn implied_tokens(o: &OpenPaper, slippage: f64) -> Option<f64> {
        let entry = o.entry_price?;
        if entry <= 0.0 {
            return None;
        }
        Some(o.sol_invested * (1.0 - slippage) / entry)
    }

    /// Record a paper exit of `fraction` of the remaining position at `price`.
    /// Returns the closed trade when the position is fully exited.
    pub fn record_exit(
        &mut self,
        mint: &str,
        price: Option<f64>,
        fraction: f64,
        reason: ExitReason,
    ) -> Option<ClosedTrade> {
        let o = self.open.get_mut(mint)?;
        let entry_price = o.entry_price.unwrap_or(0.0);
        // Fallback when no price print is available: value at entry.
        let px = price.filter(|p| *p > 0.0).unwrap_or(entry_price);
        let remaining = (1.0 - o.exited_fraction).max(0.0);
        let sell_frac = fraction.clamp(0.0, 1.0).min(remaining);
        let proceeds = match Self::implied_tokens(o, self.slippage) {
            Some(tokens) => {
                tokens * sell_frac * px * (1.0 - self.slippage) - TX_FEE_SOL
            }
            None => 0.0,
        }
        .max(0.0);
        o.realized_sol += proceeds;
        o.exited_fraction += sell_frac;

        if o.exited_fraction >= 1.0 - 1e-9 {
            let o = self.open.remove(mint).unwrap();
            let sol_out = o.realized_sol;
            // Entry also paid a tx fee on top of the invested SOL.
            let sol_in = o.sol_invested + TX_FEE_SOL;
            let net = sol_out - sol_in;
            let multiple = if sol_in > 0.0 { sol_out / sol_in } else { 0.0 };
            let trade = ClosedTrade {
                mint: mint.to_string(),
                sol_in,
                sol_out,
                net_sol: net,
                multiple,
                final_reason: reason,
            };
            self.closed.push(trade.clone());
            Some(trade)
        } else {
            None
        }
    }

    pub fn closed_count(&self) -> usize {
        self.closed.len()
    }

    /// Capture the full tracker state for persistence across restarts.
    pub fn snapshot(&self) -> PnlSnapshot {
        PnlSnapshot {
            slippage_pct: self.slippage * 100.0,
            open: self.open.clone(),
            closed: self.closed.clone(),
        }
    }

    /// Restore tracker state saved by [`PaperPnl::snapshot`].
    pub fn restore(&mut self, snap: PnlSnapshot) {
        self.open = snap.open;
        self.closed = snap.closed;
        // Keep the configured slippage; the snapshot's is informational.
    }

    /// One-line human summary for the log.
    pub fn summary(&self) -> String {
        let n = self.closed.len();
        if n == 0 {
            return "PAPER P&L: no closed trades yet".to_string();
        }
        let net: f64 = self.closed.iter().map(|t| t.net_sol).sum();
        let wins: Vec<&ClosedTrade> = self.closed.iter().filter(|t| t.net_sol > 0.0).collect();
        let losses = n - wins.len();
        let win_rate = wins.len() as f64 / n as f64 * 100.0;
        let gross_win: f64 = wins.iter().map(|t| t.net_sol).sum();
        let gross_loss: f64 = self
            .closed
            .iter()
            .filter(|t| t.net_sol <= 0.0)
            .map(|t| -t.net_sol)
            .sum();
        let profit_factor = if gross_loss > 1e-12 {
            gross_win / gross_loss
        } else {
            f64::INFINITY
        };
        let avg_win = if wins.is_empty() {
            0.0
        } else {
            gross_win / wins.len() as f64
        };
        let avg_loss = if losses == 0 {
            0.0
        } else {
            gross_loss / losses as f64
        };
        let best = self
            .closed
            .iter()
            .max_by(|a, b| a.net_sol.partial_cmp(&b.net_sol).unwrap());
        let mut reasons: HashMap<String, usize> = HashMap::new();
        for t in &self.closed {
            *reasons.entry(format!("{:?}", t.final_reason)).or_insert(0) += 1;
        }
        let mut reason_str: Vec<String> =
            reasons.iter().map(|(k, v)| format!("{k} {v}")).collect();
        reason_str.sort();
        format!(
            "PAPER P&L ({} closed, {:.1}% slip/side): net {:+.4} SOL | win rate {:.0}% ({}/{}) | avg win {:+.4} | avg loss {:+.4} | profit factor {:.2} | best {:+.4} ({}) | exits: {}",
            n,
            self.slippage * 100.0,
            net,
            win_rate,
            wins.len(),
            n,
            avg_win,
            -avg_loss,
            profit_factor,
            best.map(|t| t.net_sol).unwrap_or(0.0),
            best.map(|t| t.mint.chars().take(8).collect::<String>()).unwrap_or_default(),
            reason_str.join(", "),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_win_at_2x() {
        let mut p = PaperPnl::new(0.0); // no slippage for exact math
        p.record_entry("m1", 1.0);
        p.record_entry_price("m1", 0.001);
        let t = p.record_exit("m1", Some(0.002), 1.0, ExitReason::TakeProfit(1)).unwrap();
        // 1000 tokens * 0.002 = 2.0 out, minus tx fees.
        assert!((t.sol_out - (2.0 - TX_FEE_SOL)).abs() < 1e-9);
        assert!(t.net_sol > 0.9);
        assert_eq!(p.closed_count(), 1);
    }

    #[test]
    fn partial_tp_then_trailing_stop() {
        let mut p = PaperPnl::new(0.0);
        p.record_entry("m2", 1.0);
        p.record_entry_price("m2", 0.001);
        // TP1: sell half at 2x.
        assert!(p.record_exit("m2", Some(0.002), 0.5, ExitReason::TakeProfit(1)).is_none());
        assert_eq!(p.closed_count(), 0);
        // Remainder exits at 3x via trailing stop.
        let t = p.record_exit("m2", Some(0.003), 1.0, ExitReason::TrailingStop).unwrap();
        // 500*0.002 + 500*0.003 = 2.5 out (minus 2 tx fees on exits + entry fee).
        assert!((t.sol_out - (2.5 - 2.0 * TX_FEE_SOL)).abs() < 1e-9);
        assert!(t.net_sol > 1.4);
    }

    #[test]
    fn rug_before_tp_is_a_loss() {
        let mut p = PaperPnl::new(2.0);
        p.record_entry("m3", 0.049);
        p.record_entry_price("m3", 0.00001);
        let t = p.record_exit("m3", Some(0.000005), 1.0, ExitReason::DevDump).unwrap();
        assert!(t.net_sol < 0.0);
    }

    #[test]
    fn summary_counts_reasons() {
        let mut p = PaperPnl::new(0.0);
        p.record_entry("a", 1.0);
        p.record_entry_price("a", 1.0);
        p.record_exit("a", Some(2.0), 1.0, ExitReason::TakeProfit(1));
        p.record_entry("b", 1.0);
        p.record_entry_price("b", 1.0);
        p.record_exit("b", Some(0.5), 1.0, ExitReason::HardStopLoss);
        let s = p.summary();
        assert!(s.contains("2 closed"));
        assert!(s.contains("win rate 50%"));
        assert!(s.contains("TakeProfit(1) 1"));
    }
}
