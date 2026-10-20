use anyhow::{anyhow, Result};
use crate::{config::RiskConfig, types::TradeSignal};
use serde::{Deserialize, Serialize};
pub struct RiskEngine { cfg: RiskConfig }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    HardStopLoss,
    TrailingStop,
    /// Tiered take-profit: tier 1 = first target, tier 2 = second target.
    /// Both are partial exits; the remainder keeps riding.
    TakeProfit(u8),
    /// Bonding curve nearly complete: full exit before PumpSwap migration.
    Graduation,
    DevDump,
    /// No price updates for longer than the stale timeout: exit at the last
    /// known price so a dead token cannot lock exposure forever.
    Stale,
}
impl RiskEngine {
    pub fn new(cfg: RiskConfig) -> Self { Self { cfg } }
    pub fn evaluate(&self, s: &TradeSignal, open: usize, exposure: f64) -> Result<()> {
        if open >= self.cfg.max_open_positions { return Err(anyhow!("maximum open positions reached")); }
        if !exposure.is_finite() || exposure < 0.0 { return Err(anyhow!("invalid exposure state")); }
        if exposure + self.cfg.max_position_sol > self.cfg.max_total_exposure_sol {
            return Err(anyhow!("maximum total exposure would be exceeded"));
        }
        if s.mint.trim().is_empty() || s.creator.trim().is_empty() { return Err(anyhow!("missing mint/creator")); }
        if !s.creator_buy_sol.is_finite() || s.creator_buy_sol < 0.0 { return Err(anyhow!("invalid buy amount")); }
        Ok(())
    }
    pub fn exit_signal(&self, entry: f64, peak: f64, current: f64, tp_tier: u8) -> Option<ExitReason> {
        if ![entry, peak, current].iter().all(|x| x.is_finite()) || entry <= 0.0 || peak <= 0.0 || current <= 0.0 { return None; }
        if current <= entry * (1.0 - self.cfg.stop_loss_pct / 100.0) { return Some(ExitReason::HardStopLoss); }
        if current <= peak * (1.0 - self.cfg.trailing_stop_pct / 100.0) { return Some(ExitReason::TrailingStop); }
        // Tiered take-profits: each tier sells a fraction and lets the rest ride.
        if tp_tier == 0 && current >= entry * (1.0 + self.cfg.take_profit_pct / 100.0) { return Some(ExitReason::TakeProfit(1)); }
        if tp_tier == 1 && current >= entry * (1.0 + self.cfg.take_profit_pct_2 / 100.0) { return Some(ExitReason::TakeProfit(2)); }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RiskConfig;

    fn cfg() -> RiskConfig {
        RiskConfig {
            max_open_positions: 3,
            max_position_sol: 0.049,
            max_total_exposure_sol: 0.147,
            max_slippage_bps: 500,
            stop_loss_pct: 25.0,
            trailing_stop_pct: 15.0,
            take_profit_pct: 100.0,
            take_profit_pct_2: 300.0,
            take_profit_fraction: 0.5,
        }
    }

    #[test]
    fn tiered_take_profit_fires_in_order() {
        let r = RiskEngine::new(cfg());
        // Tier 1 at 2x.
        assert_eq!(r.exit_signal(1.0, 2.0, 2.0, 0), Some(ExitReason::TakeProfit(1)));
        // Tier 1 does not refire once taken.
        assert_eq!(r.exit_signal(1.0, 2.5, 2.5, 1), None);
        // Tier 2 at 4x.
        assert_eq!(r.exit_signal(1.0, 4.0, 4.0, 1), Some(ExitReason::TakeProfit(2)));
        // Tier 2 does not refire.
        assert_eq!(r.exit_signal(1.0, 5.0, 5.0, 2), None);
    }

    #[test]
    fn trailing_stop_still_protects_runner() {
        let r = RiskEngine::new(cfg());
        // After tier 2 at 4x peak, a 15% drop exits the remainder.
        assert_eq!(
            r.exit_signal(1.0, 4.0, 3.39, 2),
            Some(ExitReason::TrailingStop)
        );
        // Small dip does not.
        assert_eq!(r.exit_signal(1.0, 4.0, 3.5, 2), None);
    }

    #[test]
    fn stop_loss_unchanged() {
        let r = RiskEngine::new(cfg());
        assert_eq!(
            r.exit_signal(1.0, 1.0, 0.74, 0),
            Some(ExitReason::HardStopLoss)
        );
    }
}
