//! Live trading loop: Yellowstone -> strategy -> risk -> protection -> Jito.
//!
//! Modes:
//! - `RUN_MODE=paper`: full pipeline against the live feed, nothing submitted.
//! - `RUN_MODE=live`: real execution; requires `EXECUTION_ENABLED=true`.
//!
//! Every buy is gated, in order: strategy signal -> risk limits ->
//! pre-buy safety checks -> transaction simulation -> Jito bundle ->
//! landing verification. A submission ack is never treated as a fill.

use std::collections::HashMap;
use std::env;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use solana_pubkey::Pubkey;
use tokio::time::interval;
use tracing::{error, info, warn};

use crate::{
    config::AppConfig,
    decode,
    execution::ExecutionRelay,
    ingest::{
        polling::PollingSource,
        yellowstone::{fetch_account_data, YellowstoneSource},
        EventSource,
    },
    jito::JitoRelay,
    paper_pnl::PaperPnl,
    persist,
    protection::{self, SafetyPolicy},
    pumpswap_tx,
    risk::{ExitReason, RiskEngine},
    signer::WalletSigner,
    strategy::{LaunchSnipeStrategy, PumpCreatorBuyStrategy, PumpSwapMigrationStrategy, RaydiumMigrationStrategy},
    telegram::TelegramNotifier,
    tx_builder,
    types::{ChainEvent, TradeSignal},
};

/// Read the protocol fee recipient from the on-chain Pump Global account.
/// Fails when the account is missing or unparsable (caller decides fallback).
async fn resolve_fee_recipient(rpc_url: &str) -> Result<Pubkey> {
    let pda = decode::global_pda();
    let data = fetch_account_data(rpc_url, &pda)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Pump Global account not found: {pda}"))?;
    decode::global_fee_recipient(&data)
}

/// An open position being managed by the loop.
#[derive(Clone)]
struct Position {
    mint: Pubkey,
    creator: Pubkey,
    token_program: Pubkey,
    fee_recipient: Pubkey,
    entry_price: f64, // SOL per token
    peak_price: f64,  // SOL per token
    token_amount: u64,
    sol_invested: f64,
    /// Take-profit tier reached: 0 = none, 1 = sold fraction at 2x,
    /// 2 = sold fraction again at 4x. Remainder keeps riding.
    tp_tier: u8,
    /// Fraction of the original position still held (1.0 = whole).
    remaining_fraction: f64,
    /// Set when a live exit failed: the tokens are likely still in the wallet
    /// and the position needs a recovery retry, not silent abandonment.
    exit_failed: bool,
    /// The exit reason from the failed attempt, reused on retry.
    failed_exit_reason: Option<ExitReason>,
    /// Unix seconds of the last trade print seen for this mint. Drives the
    /// stale-position sweep: a token that goes quiet can never trigger
    /// stop/trailing exits, so without this it would lock exposure forever.
    last_event_unix: u64,
    /// True when entered via a migration strategy (raydium_migration or
    /// pumpswap_migration — already migrated): skips the pre-migration
    /// graduation exit check. Kept under the original field name so existing
    /// persisted state still deserializes.
    via_raydium: bool,
}

impl Position {
    fn to_persisted(&self, mint_str: &str) -> persist::PersistedPosition {
        persist::PersistedPosition {
            mint: mint_str.to_string(),
            creator: self.creator.to_string(),
            token_program: self.token_program.to_string(),
            fee_recipient: self.fee_recipient.to_string(),
            entry_price: self.entry_price,
            peak_price: self.peak_price,
            token_amount: self.token_amount,
            sol_invested: self.sol_invested,
            tp_tier: self.tp_tier,
            remaining_fraction: self.remaining_fraction,
            exit_failed: self.exit_failed,
            failed_exit_reason: self.failed_exit_reason.map(|r| format!("{r:?}")),
            last_event_unix: self.last_event_unix,
            via_raydium: self.via_raydium,
        }
    }

    fn from_persisted(pp: &persist::PersistedPosition) -> Result<Self> {
        // failed_exit_reason is informational only; a restored live position
        // re-derives recovery state from on-chain checks, not from the flag.
        Ok(Self {
            mint: Pubkey::from_str(&pp.mint).context("persisted mint")?,
            creator: Pubkey::from_str(&pp.creator).context("persisted creator")?,
            token_program: Pubkey::from_str(&pp.token_program)
                .context("persisted token_program")?,
            fee_recipient: Pubkey::from_str(&pp.fee_recipient)
                .context("persisted fee_recipient")?,
            entry_price: pp.entry_price,
            peak_price: pp.peak_price,
            token_amount: pp.token_amount,
            sol_invested: pp.sol_invested,
            tp_tier: pp.tp_tier,
            remaining_fraction: pp.remaining_fraction,
            exit_failed: false,
            failed_exit_reason: None,
            last_event_unix: pp.last_event_unix,
            via_raydium: pp.via_raydium,
        })
    }
}

/// Current unix time in seconds.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A position is stale when no trade print has arrived for longer than the
/// configured timeout. Pure function for testability.
fn is_stale(last_event_unix: u64, now: u64, timeout_secs: u64) -> bool {
    timeout_secs > 0 && now.saturating_sub(last_event_unix) > timeout_secs
}

struct Ctx {
    cfg: AppConfig,
    rpc_url: String,
    live: bool,
    signer: Option<WalletSigner>,
    jito: Option<JitoRelay>,
    tip_account: Pubkey,
    fee_recipient: Pubkey,
    risk: RiskEngine,
    safety: SafetyPolicy,
    creator_strategy: PumpCreatorBuyStrategy,
    launch_strategy: LaunchSnipeStrategy,
    migration_strategy: RaydiumMigrationStrategy,
    pumpswap_migration_strategy: PumpSwapMigrationStrategy,
    positions: HashMap<String, Position>,
    token_program_cache: HashMap<Pubkey, Pubkey>,
    /// Optional Telegram alerts for the public signals channel. `None` when
    /// TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID are not set.
    telegram: Option<TelegramNotifier>,
    /// Honest paper-trading P&L (slippage + fees modeled). Only used in paper mode.
    pnl: PaperPnl,
}

pub async fn run(cfg: &AppConfig) -> Result<()> {
    let live = cfg.run_mode == "live";
    if live && !cfg.execution_enabled {
        anyhow::bail!("RUN_MODE=live requires EXECUTION_ENABLED=true (refusing to trade live)");
    }
    // Launch sniping needs the full trade firehose for first-trade detection.
    if cfg.strategy == "launch_snipe" {
        std::env::set_var("EMIT_ALL_TRADES", "1");
    }

    let rpc_url = env::var("SOLANA_RPC_URL").context("SOLANA_RPC_URL is not set")?;
    // Fee recipient: prefer the on-chain Pump Global account (it can change);
    // fall back to the FEE_RECIPIENT env var with a loud warning. Only needed
    // when building real transactions (live mode).
    let fee_recipient = if live {
        match resolve_fee_recipient(&rpc_url).await {
            Ok(pk) => {
                info!(fee_recipient = %pk, "using on-chain Pump Global fee recipient");
                pk
            }
            Err(e) => {
                warn!(error = %e, "on-chain fee recipient fetch failed; falling back to FEE_RECIPIENT env");
                Pubkey::from_str(&cfg.fee_recipient).context(
                    "FEE_RECIPIENT is not set or invalid (and on-chain fetch failed)",
                )?
            }
        }
    } else {
        Pubkey::from_str(&cfg.fee_recipient).unwrap_or_default()
    };

    let signer = if live {
        Some(WalletSigner::from_env()?)
    } else {
        None
    };
    if live {
        if cfg.jito_block_engine_url.is_empty() {
            anyhow::bail!("JITO_BLOCK_ENGINE_URL is not set");
        }
        if cfg.jito_tip_account.is_empty() {
            anyhow::bail!("JITO_TIP_ACCOUNT is not set");
        }
    }
    let jito = if live {
        Some(JitoRelay::new(cfg.jito_block_engine_url.clone()))
    } else {
        None
    };
    let tip_account = if live {
        Pubkey::from_str(&cfg.jito_tip_account).context("bad JITO_TIP_ACCOUNT")?
    } else {
        Pubkey::default()
    };

    if live {
        if let Some(s) = &signer {
            info!(wallet = %s.pubkey(), "live execution enabled");
        }
        warn!("LIVE MODE: real SOL will be traded. Risk limits: max {} positions, {:.3} SOL each",
            cfg.risk.max_open_positions, cfg.risk.max_position_sol);
    } else {
        info!("paper mode: full pipeline, nothing will be submitted");
    }

    // The pumpswap_migration strategy only fires on post-migration PumpSwap pools,
    // so the safety gate must allow migrated curves for that strategy. Every other
    // strategy keeps the default (reject migrated curves).
    let mut safety = SafetyPolicy::default();
    safety.allow_migrated_curve = cfg.strategy == "pumpswap_migration";

    let mut ctx = Ctx {
        cfg: cfg.clone(),
        rpc_url,
        live,
        signer,
        jito,
        tip_account,
        fee_recipient,
        risk: RiskEngine::new(cfg.risk.clone()),
        safety,
        creator_strategy: PumpCreatorBuyStrategy::new(cfg.min_dev_buy_sol),
        launch_strategy: LaunchSnipeStrategy::new(),
        migration_strategy: RaydiumMigrationStrategy::new(),
        pumpswap_migration_strategy: PumpSwapMigrationStrategy::new(),
        positions: HashMap::new(),
        token_program_cache: HashMap::new(),
        telegram: TelegramNotifier::from_env(),
        pnl: PaperPnl::new(cfg.paper_slippage_pct),
    };

    // Resume any positions + paper P&L orphaned by a previous run's restart.
    restore_state(&mut ctx);

    // Reconnect loop: Yellowstone streams drop routinely (server-side,
    // proxy, blips). A dropped stream used to exit the process silently;
    // now we reconnect with backoff. Open positions, the P&L tracker and
    // caches survive in `ctx` across reconnects.
    let mut attempt: u64 = 0;
    let mut loop_started = std::time::Instant::now();
    loop {
        match run_stream_loop(&mut ctx).await {
            Ok(()) => warn!("event stream ended; reconnecting"),
            Err(e) => warn!(error = %e, "stream loop errored; reconnecting"),
        }
        // A connection that stayed healthy for 5+ minutes resets the backoff:
        // long-lived flakiness should not escalate the retry delay.
        if loop_started.elapsed() > Duration::from_secs(300) {
            attempt = 0;
        }
        loop_started = std::time::Instant::now();
        let delay_secs = 2_u64.saturating_pow(attempt.min(6) as u32).min(60);
        attempt = attempt.saturating_add(1);
        warn!(delay_secs, attempt, "reconnecting Yellowstone stream");
        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
    }
}

/// One connected stream lifetime: subscribe, then drive the event loop
/// until the stream ends or errors.
async fn run_stream_loop(ctx: &mut Ctx) -> Result<()> {
    // Dispatch on the configured ingest provider. Both implement
    // `EventSource`, so the rest of the loop is provider-agnostic.
    let mut source: Box<dyn EventSource> = match ctx.cfg.ingest_provider.as_str() {
        "polling" => Box::new(PollingSource::connect().await?),
        _ => Box::new(YellowstoneSource::connect().await?),
    };
    info!("live loop running");
    let mut grad_tick = interval(Duration::from_secs(30));
    let mut tick_count: u64 = 0;

    loop {
        tokio::select! {
            _ = grad_tick.tick() => {
                tick_count += 1;
                check_graduations(&mut *ctx).await;
                retry_failed_exits(&mut *ctx).await;
                sweep_stale_positions(&mut *ctx).await;
                // Honest scoreboard every ~5 minutes (paper mode).
                if !ctx.live && ctx.pnl.closed_count() > 0 && tick_count % 10 == 0 {
                    info!("{}", ctx.pnl.summary());
                }
            }
            next = source.next_event() => {
                let Some(event) = next? else {
                    // Stream dropped: return so the reconnect loop
                    // re-subscribes instead of exiting the process.
                    return Ok(());
                };
                if let Err(e) = handle_event(&mut *ctx, &event).await {
                    error!(event_id = %event.event_id, error = %e, "event handling failed");
                }
            }
        }
    }
}

/// Route one normalized event through the pipeline.
async fn handle_event(ctx: &mut Ctx, event: &ChainEvent) -> Result<()> {
    let mint_str = match &event.mint {
        Some(m) => m.clone(),
        None => return Ok(()),
    };

    // 1. Dev-dump tripwire: emergency exit any open position in this mint.
    if protection::dev_dump_detected(event) && ctx.positions.contains_key(&mint_str) {
        warn!(mint = %mint_str, "dev dump detected: emergency exit");
        exit_position(ctx, &mint_str, ExitReason::DevDump, 1.0, event.trade_price_sol_per_token).await;
        return Ok(());
    }

    // 2. Price updates for open positions -> stop/trailing/take-profit.
    // The seeded entry price is recorded into the P&L tracker after the
    // position borrow ends.
    let mut seeded_price: Option<f64> = None;
    if let (Some(pos), Some(price)) = (
        ctx.positions.get_mut(&mint_str),
        event.trade_price_sol_per_token,
    ) {
        // Paper positions start without a fill price; seed from the first print.
        if pos.entry_price <= 0.0 && price > 0.0 {
            pos.entry_price = price;
            pos.peak_price = price;
            seeded_price = Some(price);
        } else if price > pos.peak_price {
            pos.peak_price = price;
        }
        // Any trade print proves the token is alive; drives the stale sweep.
        pos.last_event_unix = now_unix();
        if let Some(reason) =
            ctx.risk.exit_signal(pos.entry_price, pos.peak_price, price, pos.tp_tier)
        {
            // Tiered take-profits are partial: sell the configured fraction
            // and keep the rest riding. Everything else is a full exit.
            let fraction = exit_fraction(ctx, &reason);
            info!(mint = %mint_str, ?reason, fraction, "exit signal");
            exit_position(ctx, &mint_str, reason, fraction, Some(price)).await;
            return Ok(());
        }
    }
    if let Some(p) = seeded_price {
        ctx.pnl.record_entry_price(&mint_str, p);
    }

    // 3. Entry: strategy signal -> risk -> protection -> build -> submit.
    let signal = match ctx.cfg.strategy.as_str() {
        "launch_snipe" => ctx.launch_strategy.evaluate(event),
        "raydium_migration" => ctx.migration_strategy.evaluate(event),
        "pumpswap_migration" => ctx.pumpswap_migration_strategy.evaluate(event),
        _ => ctx.creator_strategy.evaluate(event),
    };
    let Some(signal) = signal else { return Ok(()) };

    // Public signals channel: every qualifying dev buy is alert-worthy,
    // whether or not we open a paper position on it. Migration strategies
    // don't produce dev buys (their creator_buy_sol is 0.0), so posting
    // them here would spam the channel with "DEV BUY — 0.00 SOL".
    if ctx.cfg.strategy == "pump_creator_buy" {
        if let Some(tg) = &ctx.telegram {
            tg.send_dev_buy_alert(&signal.mint, &signal.creator, signal.creator_buy_sol)
                .await;
        }
    }

    if let Err(e) = ctx.risk.evaluate(
        &signal,
        ctx.positions.len(),
        total_exposure_sol(ctx),
    ) {
        info!(signal_id = %signal.signal_id, reason = %e, "signal rejected by risk");
        return Ok(());
    }

    enter_position(ctx, &signal).await
}

/// Full gated entry pipeline for one signal.
async fn enter_position(ctx: &mut Ctx, signal: &TradeSignal) -> Result<()> {
    let mint = Pubkey::from_str(&signal.mint).context("bad mint in signal")?;
    let creator = Pubkey::from_str(&signal.creator).context("bad creator in signal")?;
    let user = ctx
        .signer
        .as_ref()
        .map(|s| s.pubkey())
        .unwrap_or_default();

    // Gate 1: pre-buy safety checks (fail closed).
    let report = protection::pre_buy_checks(&ctx.rpc_url, &mint, &ctx.safety).await?;
    if !report.passed {
        info!(mint = %mint, reasons = ?report.reasons, "buy blocked by safety checks");
        return Ok(());
    }

    // Gate 2: resolve the mint's token program (legacy vs Token-2022).
    let token_program = match ctx.token_program_cache.get(&mint) {
        Some(p) => *p,
        None => {
            let p = tx_builder::fetch_mint_token_program(&ctx.rpc_url, &mint).await?;
            ctx.token_program_cache.insert(mint, p);
            p
        }
    };

    let buy_sol = ctx.cfg.risk.max_position_sol;
    let buy_lamports = (buy_sol * 1_000_000_000.0) as u64;
    info!(mint = %mint, buy_sol, "buy signal accepted; building transaction");

    if !ctx.live {
        info!(mint = %mint, buy_sol, "PAPER BUY (not submitted)");
        ctx.pnl.record_entry(&signal.mint, buy_sol);
        if let Some(tg) = &ctx.telegram {
            tg.send_paper_buy(&signal.mint, buy_sol, &ctx.cfg.strategy).await;
        }
        // Paper position at the latest observed trade price, if any.
        ctx.positions.insert(
            signal.mint.clone(),
            Position {
                mint,
                creator,
                token_program,
                fee_recipient: ctx.fee_recipient,
                entry_price: 0.0, // unknown until first trade print; updated on events
                peak_price: 0.0,
                token_amount: 0,
                sol_invested: buy_sol,
                tp_tier: 0,
                remaining_fraction: 1.0,
                exit_failed: false,
                failed_exit_reason: None,
                last_event_unix: now_unix(),
                via_raydium: matches!(ctx.cfg.strategy.as_str(), "raydium_migration" | "pumpswap_migration"),
            },
        );
        persist_state(ctx);
        return Ok(());
    }

    // PumpSwap migration buy: the token has migrated, so we buy on the
    // PumpSwap pool (not the dead bonding curve). Discovers the pool,
    // wraps SOL to wSOL, and builds the buy_v2 transaction.
    let signer = ctx.signer.as_ref().expect("signer in live mode");
    let keypair = signer.keypair();
    // One blockhash for the whole bundle: buy + tip must share it.
    let blockhash = tx_builder::fetch_latest_blockhash(&ctx.rpc_url).await?;

    // Quote expected tokens via constant product from signal reserves.
    let min_tokens_out = match (signal.virtual_sol_reserves, signal.virtual_token_reserves) {
        (Some(vsr), Some(vtr)) if vsr > 0 && vtr > 0 => {
            let sol_in = buy_lamports as u128;
            let tokens = sol_in * vtr as u128 / (vsr as u128 + sol_in);
            let slip = ctx.cfg.risk.max_slippage_bps.min(10_000) as u128;
            (tokens * (10_000 - slip) / 10_000).max(1) as u64
        }
        _ => {
            warn!(mint = %mint, "signal carries no reserve state; min_tokens_out floor set to 1");
            1
        }
    };

    let buy_tx = pumpswap_tx::build_pumpswap_buy_with_wrap(
        keypair,
        &ctx.rpc_url,
        &mint,
        &token_program,
        buy_lamports,
        min_tokens_out,
        ctx.cfg.compute_unit_limit,
        ctx.cfg.priority_fee_microlamports,
        blockhash.clone(),
    )
    .await?;

    // Gate 3: simulate before submitting.
    let sim = protection::simulate_transaction(&ctx.rpc_url, &buy_tx).await?;
    if !sim.success {
        warn!(mint = %mint, error = ?sim.error, "buy simulation failed; not submitting");
        return Ok(());
    }

    // Gate 4: submit as a Jito bundle (buy + tip) and verify landing.
    let tip_tx = tx_builder::build_tip_transaction(
        keypair,
        &ctx.tip_account,
        ctx.cfg.jito_tip_lamports,
        ctx.cfg.priority_fee_microlamports,
        blockhash,
    )
    .await?;
    let jito = ctx.jito.as_ref().expect("jito in live mode");
    let submission = jito.submit_signed_transactions(&[buy_tx, tip_tx]).await?;
    info!(bundle_id = %submission.bundle_id, mint = %mint, "bundle submitted; verifying landing");
    let status = jito
        .wait_for_landing(&submission.bundle_id, 10, Duration::from_millis(500))
        .await?;
    if !status.landed() {
        warn!(bundle_id = %submission.bundle_id, status = ?status, "bundle did not land; no position registered");
        return Ok(());
    }

    // Fill confirmed: size the position from the actual token balance.
    let ata = tx_builder::ata_pda(&user, &token_program, &mint);
    let token_amount = tx_builder::fetch_token_balance(&ctx.rpc_url, &ata).await?;
    let entry_price = if token_amount > 0 {
        buy_lamports as f64 / token_amount as f64 / 1_000_000_000.0
    } else {
        0.0
    };
    info!(mint = %mint, token_amount, entry_price, "position opened");
    ctx.positions.insert(
        signal.mint.clone(),
        Position {
            mint,
            creator,
            token_program,
            fee_recipient: ctx.fee_recipient,
            entry_price,
            peak_price: entry_price,
            token_amount,
            sol_invested: buy_sol,
            tp_tier: 0,
            remaining_fraction: 1.0,
            exit_failed: false,
                failed_exit_reason: None,
                last_event_unix: now_unix(),
                via_raydium: matches!(ctx.cfg.strategy.as_str(), "raydium_migration" | "pumpswap_migration"),
        },
    );
    persist_state(ctx);
    Ok(())
}

/// Exit a position: build, simulate, submit, and verify a sell.
/// `fraction` is how much of the remaining position to sell: 1.0 = full exit,
/// less = partial (tiered take-profit). A partial exit keeps the position
/// tracked with a reduced size and an advanced take-profit tier.
/// `exit_price` is the observed fill price (paper mode); None falls back to
/// the position's last known price inside the P&L tracker.
async fn exit_position(
    ctx: &mut Ctx,
    mint_str: &str,
    reason: ExitReason,
    fraction: f64,
    exit_price: Option<f64>,
) {
    let fraction = fraction.clamp(0.0, 1.0);
    if !ctx.live {
        // Paper mode: exits cannot fail; record honest P&L (slippage + fees).
        let closed = ctx
            .pnl
            .record_exit(mint_str, exit_price, fraction, reason);
        if let Some(trade) = closed {
            ctx.positions.remove(mint_str);
            info!(mint = %mint_str, ?reason, net_sol = format!("{:+.4}", trade.net_sol), multiple = format!("{:.2}x", trade.multiple), "PAPER SELL (not submitted)");
            if let Some(tg) = &ctx.telegram {
                tg.send_paper_sell(mint_str, &format!("{reason:?}"), trade.net_sol, trade.multiple).await;
            }
        } else if let Some(pos) = ctx.positions.get_mut(mint_str) {
            pos.remaining_fraction *= 1.0 - fraction;
            if let ExitReason::TakeProfit(tier) = reason {
                pos.tp_tier = tier.max(pos.tp_tier);
            }
            info!(mint = %mint_str, ?reason, fraction, remaining = pos.remaining_fraction, "PAPER SELL partial (not submitted)");
        }
        let n = ctx.pnl.closed_count();
        if n > 0 && n % 5 == 0 {
            let summary = ctx.pnl.summary();
            info!("{}", summary);
            if let Some(tg) = &ctx.telegram {
                tg.send_pnl_summary(&summary).await;
            }
        }
        persist_state(ctx);
        return;
    }
    // Live mode: NEVER drop the position before the sell confirms. A failed
    // exit leaves real tokens in the wallet; the position stays tracked with
    // exit_failed set so the next event retries the exit (recovery required).
    let pos = match ctx.positions.get(mint_str).cloned() {
        Some(p) => p,
        None => return,
    };
    info!(mint = %mint_str, ?reason, fraction, "exiting position");
    match exit_position_live(ctx, &pos, reason, fraction).await {
        Ok(()) => {
            if fraction >= 1.0 {
                ctx.positions.remove(mint_str);
            } else if let Some(p) = ctx.positions.get_mut(mint_str) {
                // Partial take-profit confirmed: shrink and keep riding.
                p.remaining_fraction *= 1.0 - fraction;
                if let ExitReason::TakeProfit(tier) = reason {
                    p.tp_tier = tier.max(p.tp_tier);
                }
                p.exit_failed = false;
                p.failed_exit_reason = None;
                info!(mint = %mint_str, remaining = p.remaining_fraction, "partial exit confirmed; position kept");
            }
        }
        Err(e) => {
            error!(mint = %mint_str, error = %e, "EXIT FAILED - position retained, recovery required; will retry on next event");
            if let Some(p) = ctx.positions.get_mut(mint_str) {
                p.exit_failed = true;
                p.failed_exit_reason = Some(reason);
            }
        }
    }
    persist_state(ctx);
}

async fn exit_position_live(ctx: &Ctx, pos: &Position, reason: ExitReason, fraction: f64) -> Result<()> {
    let signer = ctx.signer.as_ref().expect("signer in live mode");
    let user = signer.pubkey();
    let ata = tx_builder::ata_pda(&user, &pos.token_program, &pos.mint);
    let balance = tx_builder::fetch_token_balance(&ctx.rpc_url, &ata).await?;
    if balance == 0 {
        warn!(mint = %pos.mint, "nothing to sell; closing position");
        return Ok(());
    }
    // Partial exit: sell only the requested fraction of the balance.
    let sell_amount = ((balance as f64 * fraction).round() as u64).max(1).min(balance);
    // Estimate output from the best price we have; slippage guard does the rest.
    let px = if pos.peak_price > 0.0 {
        pos.peak_price
    } else {
        pos.entry_price
    };
    let expected_sol = (px * sell_amount as f64 * 1_000_000_000.0) as u64;

    // PumpSwap migration sell: sell on the PumpSwap pool via sell_v2.
    let wsol = solana_pubkey::Pubkey::from_str("So11111111111111111111111111111111111111112")?;
    let spl_token = solana_pubkey::Pubkey::from_str(tx_builder::TOKEN_PROGRAM_STR)?;
    let (pool, pool_base_ta, pool_quote_ta) =
        pumpswap_tx::find_pumpswap_pool(&ctx.rpc_url, &pos.mint).await?;
    let user_quote_ata = pumpswap_tx::pumpswap_ata(&user, &spl_token, &wsol);
    let buyback_ata = pumpswap_tx::pumpswap_ata(&pumpswap_tx::pumpswap_buyback_wallet(), &spl_token, &wsol);
    let swap_accts = pumpswap_tx::PumpSwapAccounts {
        pool,
        user,
        base_mint: pos.mint,
        quote_mint: wsol,
        base_token_program: pos.token_program,
        quote_token_program: spl_token,
        buyback_fee_recipient: buyback_ata,
        fee_config: None,
        pool_base_token_account: Some(pool_base_ta),
        pool_quote_token_account: Some(pool_quote_ta),
    };
    // Create wSOL ATA if missing (for receiving SOL)
    let create_quote_ata = !pumpswap_tx::account_exists(&ctx.rpc_url, &user_quote_ata).await?;
    let slip = ctx.cfg.risk.max_slippage_bps.min(10_000) as u64;
    let min_sol_out = expected_sol * (10_000 - slip) / 10_000;
    let order = pumpswap_tx::PumpSwapSellOrder {
        accts: swap_accts,
        token_amount: sell_amount,
        min_quote_amount_out_lamports: min_sol_out.max(1),
        create_user_quote_ata: create_quote_ata,
        compute_unit_limit: ctx.cfg.compute_unit_limit,
        compute_unit_price_microlamports: ctx.cfg.priority_fee_microlamports,
    };
    // One blockhash for the whole bundle: sell + tip must share it.
    let blockhash = tx_builder::fetch_latest_blockhash(&ctx.rpc_url).await?;
    let sell_tx = pumpswap_tx::build_pumpswap_sell_transaction(signer.keypair(), &order, blockhash.clone()).await?;
    let sim = protection::simulate_transaction(&ctx.rpc_url, &sell_tx).await?;
    if !sim.success {
        anyhow::bail!("sell simulation failed: {:?}", sim.error);
    }
    let tip_tx = tx_builder::build_tip_transaction(
        signer.keypair(),
        &ctx.tip_account,
        ctx.cfg.jito_tip_lamports,
        ctx.cfg.priority_fee_microlamports,
        blockhash,
    )
    .await?;
    let jito = ctx.jito.as_ref().expect("jito in live mode");
    let submission = jito.submit_signed_transactions(&[sell_tx, tip_tx]).await?;
    let status = jito
        .wait_for_landing(&submission.bundle_id, 10, Duration::from_millis(500))
        .await?;
    if !status.landed() {
        anyhow::bail!("sell bundle did not land: {:?}", status);
    }
    info!(mint = %pos.mint, ?reason, bundle_id = %submission.bundle_id, "position closed");
    Ok(())
}

/// Retry exits that previously failed (tokens likely still in wallet).
/// Runs on the periodic tick so a dead token with no new events still gets
/// recovery attempts instead of sitting stranded.
async fn retry_failed_exits(ctx: &mut Ctx) {
    let failed: Vec<(String, ExitReason)> = ctx
        .positions
        .iter()
        .filter(|(_, p)| p.exit_failed)
        .map(|(k, p)| (k.clone(), p.failed_exit_reason.unwrap_or(ExitReason::HardStopLoss)))
        .collect();
    for (mint_str, reason) in failed {
        warn!(mint = %mint_str, ?reason, "retrying previously failed exit");
        exit_position(ctx, &mint_str, reason, exit_fraction(ctx, &reason), None).await;
    }
}

/// How much of the position an exit reason sells: take-profit tiers are
/// partial, everything else is a full exit.
fn exit_fraction(ctx: &Ctx, reason: &ExitReason) -> f64 {
    match reason {
        ExitReason::TakeProfit(_) => ctx.cfg.risk.take_profit_fraction,
        _ => 1.0,
    }
}

/// Best-effort snapshot of positions + paper P&L to disk. Called after every
/// mutation so a restart (cron, crash, deploy) resumes instead of orphaning.
fn persist_state(ctx: &Ctx) {
    let state = persist::BotState {
        positions: ctx
            .positions
            .iter()
            .map(|(k, p)| p.to_persisted(k))
            .collect(),
        pnl: ctx.pnl.snapshot(),
    };
    if let Err(e) = persist::save_state(&ctx.cfg.position_state_path, &state) {
        warn!(error = %e, "failed to persist position state");
    }
}

/// Restore positions + paper P&L saved by [`persist_state`]. Runs once at
/// startup, before the reconnect loop.
fn restore_state(ctx: &mut Ctx) {
    let state = match persist::load_state(&ctx.cfg.position_state_path) {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            warn!(error = %e, "position state file corrupt; starting fresh");
            return;
        }
    };
    let mut restored = 0;
    for pp in &state.positions {
        match Position::from_persisted(pp) {
            Ok(pos) => {
                ctx.positions.insert(pp.mint.clone(), pos);
                restored += 1;
            }
            Err(e) => warn!(mint = %pp.mint, error = %e, "skipping unrestorable position"),
        }
    }
    ctx.pnl.restore(state.pnl);
    if restored > 0 {
        warn!(
            restored,
            closed_trades = ctx.pnl.closed_count(),
            "restored position state from previous run; stale positions will exit on the next sweep"
        );
    }
}

/// Exit positions that have seen no trade prints for longer than the
/// configured stale timeout. Without this, a dead token parks in the book
/// forever and the exposure cap blocks every new signal (observed in the
/// Oct 11 paper test: 2.5h of rejected signals). Exits at the last known
/// price; paper P&L falls back to entry price when none was ever observed.
async fn sweep_stale_positions(ctx: &mut Ctx) {
    let timeout = ctx.cfg.stale_position_timeout_secs;
    if timeout == 0 || ctx.positions.is_empty() {
        return;
    }
    let now = now_unix();
    let stale: Vec<(String, Option<f64>, u64)> = ctx
        .positions
        .iter()
        .filter(|(_, p)| is_stale(p.last_event_unix, now, timeout))
        .map(|(k, p)| {
            let px = if p.peak_price > 0.0 {
                Some(p.peak_price)
            } else if p.entry_price > 0.0 {
                Some(p.entry_price)
            } else {
                None
            };
            (k.clone(), px, now.saturating_sub(p.last_event_unix))
        })
        .collect();
    for (mint_str, px, idle_secs) in stale {
        warn!(mint = %mint_str, idle_secs, timeout_secs = timeout, "position stale: no price updates; exiting");
        exit_position(ctx, &mint_str, ExitReason::Stale, 1.0, px).await;
    }
}

/// Periodic graduation check: exit positions before their curve migrates.
/// Positions entered via the raydium_migration strategy are already migrated
/// and trade on Raydium — the bonding-curve check would wrongly exit them.
async fn check_graduations(ctx: &mut Ctx) {    if ctx.positions.is_empty() {
        return;
    }
    let mints: Vec<String> = ctx.positions.keys().cloned().collect();
    for mint_str in mints {
        let pos = match ctx.positions.get(&mint_str) {
            Some(p) => p,
            None => continue,
        };
        if pos.via_raydium {
            continue;
        }
        let curve = decode::bonding_curve_pda(&pos.mint);
        let data = match protection::fetch_curve_data(&ctx.rpc_url, &curve).await {
            Ok(Some(d)) => d,
            Ok(None) => continue,
            Err(e) => {
                warn!(mint = %mint_str, error = %e, "graduation check failed");
                continue;
            }
        };
        match protection::graduation_imminent(&data, ctx.cfg.graduation_exit_pct) {
            Ok(true) => {
                warn!(mint = %mint_str, "curve nearly complete: exiting before migration");
                exit_position(ctx, &mint_str, ExitReason::Graduation, 1.0, None).await;
            }
            Ok(false) => {}
            Err(e) => warn!(mint = %mint_str, error = %e, "graduation check failed"),
        }
    }
}

async fn account_exists(rpc_url: &str, pubkey: &Pubkey) -> Result<bool> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getAccountInfo",
        "params": [pubkey.to_string(), {"commitment": "confirmed"}],
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context("getAccountInfo request")?
        .error_for_status()?
        .json()
        .await?;
    Ok(!resp.pointer("/result/value").map(|v| v.is_null()).unwrap_or(true))
}

fn total_exposure_sol(ctx: &Ctx) -> f64 {
    ctx.positions.values().map(|p| p.sol_invested).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_detection_boundaries() {
        // Fresh position: not stale.
        assert!(!is_stale(1000, 1000, 3600));
        assert!(!is_stale(1000, 4600, 3600));
        // Exactly at the timeout: not stale (strictly greater).
        assert!(!is_stale(1000, 4600, 3600));
        // Past the timeout: stale.
        assert!(is_stale(1000, 4601, 3600));
        // Disabled timeout never goes stale.
        assert!(!is_stale(0, u64::MAX, 0));
        // Clock skew (last in the future) is not stale.
        assert!(!is_stale(5000, 1000, 3600));
    }

    #[test]
    fn position_persisted_round_trip() {
        let pos = Position {
            mint: Pubkey::from_str("6qUw38wQ4TF8DGnpmM9jGydEQWXmgquHtmK3op5gpump").unwrap(),
            creator: Pubkey::from_str("Git31c6HwUBYU9qgQakTg96VsLECjsa66AnXsUT9B252").unwrap(),
            token_program: Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap(),
            fee_recipient: Pubkey::default(),
            entry_price: 0.001,
            peak_price: 0.002,
            token_amount: 49000,
            sol_invested: 0.049,
            tp_tier: 1,
            remaining_fraction: 0.5,
            exit_failed: false,
            failed_exit_reason: None,
            last_event_unix: 1_700_000_000,
            via_raydium: false,
        };
        let pp = pos.to_persisted("6qUw38wQ4TF8DGnpmM9jGydEQWXmgquHtmK3op5gpump");
        let back = Position::from_persisted(&pp).unwrap();
        assert_eq!(back.mint, pos.mint);
        assert_eq!(back.creator, pos.creator);
        assert_eq!(back.token_amount, 49000);
        assert!((back.remaining_fraction - 0.5).abs() < 1e-12);
        assert_eq!(back.tp_tier, 1);
        assert_eq!(back.last_event_unix, 1_700_000_000);
        assert!((back.peak_price - 0.002).abs() < 1e-12);
    }
}
