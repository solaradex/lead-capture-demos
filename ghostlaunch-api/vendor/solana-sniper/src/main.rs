use anyhow::Result;
use solana_sniper::{
    accounting::SeenEvents, config::AppConfig, ingest::{EventSource, ReplaySource},
    live, observability::init_logging, risk::RiskEngine, strategy::PumpCreatorBuyStrategy,
};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cfg = AppConfig::from_env()?;
    init_logging(&cfg.log_level);
    info!(mode = %cfg.run_mode, provider = %cfg.ingest_provider,
          strategy = %cfg.strategy,
          execution_enabled = cfg.execution_enabled, "Solana Sniper starting");

    match cfg.run_mode.as_str() {
        // Live trading loop (paper = no submission, live = real execution).
        "live" | "paper" => live::run(&cfg).await,
        // Original observe-first replay path.
        _ => run_observe(&cfg).await,
    }
}

async fn run_observe(cfg: &AppConfig) -> Result<()> {
    let mut source = ReplaySource::from_path(&cfg.replay_file)?;
    let strategy = PumpCreatorBuyStrategy::new(cfg.min_dev_buy_sol);
    let risk = RiskEngine::new(cfg.risk.clone());
    let mut seen = SeenEvents::default();
    let mut accepted = 0_u64;

    while let Some(event) = source.next_event().await? {
        if !seen.insert(event.event_id.clone()) {
            info!(event_id = %event.event_id, "duplicate event skipped");
            continue;
        }
        if let Some(signal) = strategy.evaluate(&event) {
            match risk.evaluate(&signal, 0, 0.0) {
                Ok(()) => {
                    accepted += 1;
                    info!(signal_id = %signal.signal_id, mint = %signal.mint,
                          creator_buy_sol = signal.creator_buy_sol,
                          "OBSERVE: candidate accepted; no transaction submitted");
                }
                Err(e) => info!(signal_id = %signal.signal_id, reason = %e, "candidate rejected"),
            }
        } else {
            info!(event_id = %event.event_id, kind = ?event.kind, "event did not match strategy");
        }
    }
    info!(accepted, "replay finished");
    Ok(())
}
