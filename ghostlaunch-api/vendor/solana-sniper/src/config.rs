use anyhow::{anyhow, Result};
use std::env;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub log_level: String,
    pub run_mode: String,
    pub ingest_provider: String,
    pub replay_file: String,
    pub execution_enabled: bool,
    pub min_dev_buy_sol: f64,
    pub strategy: String,
    pub fee_recipient: String,
    pub jito_block_engine_url: String,
    pub jito_tip_account: String,
    pub jito_tip_lamports: u64,
    pub priority_fee_microlamports: u64,
    pub compute_unit_limit: u32,
    pub graduation_exit_pct: f64,
    /// Slippage modeled per side in paper mode (percent). Default 2.0.
    pub paper_slippage_pct: f64,
    /// Seconds without a price print after which an open position is exited
    /// as stale (0 = disabled). Default 3600.
    pub stale_position_timeout_secs: u64,
    /// JSON file used to persist open positions + paper P&L across restarts.
    pub position_state_path: String,
    pub risk: RiskConfig,
}
#[derive(Debug, Clone)]
pub struct RiskConfig {
    pub max_open_positions: usize,
    pub max_position_sol: f64,
    pub max_total_exposure_sol: f64,
    pub max_slippage_bps: u16,
    pub stop_loss_pct: f64,
    pub trailing_stop_pct: f64,
    pub take_profit_pct: f64,
    /// Second take-profit target in percent (tier 2). Default 300 = 4x.
    pub take_profit_pct_2: f64,
    /// Fraction of the remaining position sold at each take-profit tier.
    /// Default 0.5 = sell half at 2x, half of the rest at 4x.
    pub take_profit_fraction: f64,
}
impl AppConfig {
    pub fn from_env() -> Result<Self> {
        let run_mode = get("RUN_MODE", "observe");
        let ingest_provider = get("INGEST_PROVIDER", "replay");
        if !["observe", "paper", "test", "live"].contains(&run_mode.as_str()) {
            return Err(anyhow!("RUN_MODE must be observe, paper, test, or live"));
        }
        if !["replay", "yellowstone", "polling"].contains(&ingest_provider.as_str()) {
            return Err(anyhow!("INGEST_PROVIDER must be replay, yellowstone, or polling"));
        }
        let live_feed = run_mode == "paper" || run_mode == "live";
        if live_feed && !["yellowstone", "polling"].contains(&ingest_provider.as_str()) {
            return Err(anyhow!("RUN_MODE={run_mode} requires INGEST_PROVIDER=yellowstone or polling"));
        }
        let strategy = get("STRATEGY", "pump_creator_buy");
        if !["pump_creator_buy", "launch_snipe", "raydium_migration", "pumpswap_migration"]
            .contains(&strategy.as_str())
        {
            return Err(anyhow!(
                "STRATEGY must be pump_creator_buy, launch_snipe, raydium_migration, or pumpswap_migration"
            ));
        }
        let risk = RiskConfig {
            max_open_positions: parse("MAX_OPEN_POSITIONS", 3_usize)?,
            max_position_sol: parse("MAX_POSITION_SOL", 0.049_f64)?,
            max_total_exposure_sol: parse("MAX_TOTAL_EXPOSURE_SOL", 0.147_f64)?,
            max_slippage_bps: parse("MAX_SLIPPAGE_BPS", 500_u16)?,
            stop_loss_pct: parse("STOP_LOSS_PCT", 25.0_f64)?,
            trailing_stop_pct: parse("TRAILING_STOP_PCT", 15.0_f64)?,
            take_profit_pct: parse("TAKE_PROFIT_PCT", 100.0_f64)?,
            take_profit_pct_2: parse("TAKE_PROFIT_PCT_2", 300.0_f64)?,
            take_profit_fraction: parse("TAKE_PROFIT_FRACTION", 0.5_f64)?,
        };
        risk.validate()?;
        let min_dev_buy_sol = parse("MIN_DEV_BUY_SOL", 5.0_f64)?;
        if !min_dev_buy_sol.is_finite() || min_dev_buy_sol < 0.0 {
            return Err(anyhow!("MIN_DEV_BUY_SOL must be finite and non-negative"));
        }
        let graduation_exit_pct = parse("GRADUATION_EXIT_PCT", 95.0_f64)?;
        if !(0.0..100.0).contains(&graduation_exit_pct) {
            return Err(anyhow!("GRADUATION_EXIT_PCT must be in (0, 100)"));
        }
        Ok(Self { log_level: get("LOG_LEVEL", "info"), run_mode, ingest_provider,
            replay_file: get("REPLAY_FILE", "fixtures/events.jsonl"),
            execution_enabled: parse("EXECUTION_ENABLED", false)?, min_dev_buy_sol,
            strategy, fee_recipient: get("FEE_RECIPIENT", ""),
            jito_block_engine_url: get("JITO_BLOCK_ENGINE_URL", ""),
            jito_tip_account: get("JITO_TIP_ACCOUNT", ""),
            jito_tip_lamports: parse("JITO_TIP_LAMPORTS", 10_000_u64)?,
            priority_fee_microlamports: parse("PRIORITY_FEE_MICROLAMPORTS", 50_000_u64)?,
            compute_unit_limit: parse("COMPUTE_UNIT_LIMIT", 300_000_u32)?,
            graduation_exit_pct,
            paper_slippage_pct: parse("PAPER_SLIPPAGE_PCT", 2.0_f64)?,
            stale_position_timeout_secs: parse("STALE_POSITION_TIMEOUT_SECS", 3600_u64)?,
            position_state_path: get("POSITION_STATE_PATH", "position-state.json"),
            risk })
    }
}
impl RiskConfig {
    fn validate(&self) -> Result<()> {
        if self.max_open_positions == 0 { return Err(anyhow!("MAX_OPEN_POSITIONS must be > 0")); }
        for (name, v) in [("MAX_POSITION_SOL", self.max_position_sol),
            ("MAX_TOTAL_EXPOSURE_SOL", self.max_total_exposure_sol),
            ("STOP_LOSS_PCT", self.stop_loss_pct), ("TRAILING_STOP_PCT", self.trailing_stop_pct),
            ("TAKE_PROFIT_PCT", self.take_profit_pct),
            ("TAKE_PROFIT_PCT_2", self.take_profit_pct_2)] {
            if !v.is_finite() || v < 0.0 { return Err(anyhow!("{name} must be finite and non-negative")); }
        }
        if !self.take_profit_fraction.is_finite()
            || self.take_profit_fraction <= 0.0
            || self.take_profit_fraction > 1.0
        {
            return Err(anyhow!("TAKE_PROFIT_FRACTION must be in (0, 1]"));
        }
        if self.max_position_sol <= 0.0 { return Err(anyhow!("MAX_POSITION_SOL must be > 0")); }
        if self.max_total_exposure_sol < self.max_position_sol {
            return Err(anyhow!("MAX_TOTAL_EXPOSURE_SOL must be >= MAX_POSITION_SOL"));
        }
        if self.max_slippage_bps > 10_000 { return Err(anyhow!("MAX_SLIPPAGE_BPS must be <= 10000")); }
        Ok(())
    }
}
fn get(k: &str, d: &str) -> String { env::var(k).unwrap_or_else(|_| d.to_owned()) }
fn parse<T: std::str::FromStr>(k: &str, d: T) -> Result<T>
where T::Err: std::fmt::Display {
    match env::var(k) { Ok(v) => v.parse().map_err(|e| anyhow!("Invalid {k}: {e}")), Err(_) => Ok(d) }
}
