//! launch-bundle: build (and optionally execute) a Pump.fun launch bundle.
//!
//! A launch bundle = one `create_v2` transaction + the plan's initial buys,
//! all sharing a recent blockhash, submitted as a single Jito bundle so the
//! token creation and the dev's initial buys land back-to-back with no gap
//! for snipers.
//!
//! SAFETY:
//! - Default mode is `--dry-run`: builds and simulates everything, submits
//!   nothing. Safe on any RPC.
//! - `--execute` actually submits. It REFUSES mainnet RPCs unless the
//!   `I_UNDERSTAND_MAINNET=1` environment variable is set.
//! - Private keys are loaded from files into memory only; they are never
//!   logged, printed, or written.

mod bundle;
mod create;
mod local;
mod plan;
mod stealth;

use anyhow::{anyhow, Context, Result};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_sniper::{execution::ExecutionRelay, jito::JitoRelay, protection, tx_builder};
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

const DEVNET_RPC: &str = "https://api.devnet.solana.com";

#[derive(Debug, Default)]
struct Args {
    plan: String,
    keypair: String,
    buyer_keys: Vec<String>,
    tip_account: Option<String>,
    rpc_url: String,
    jito_url: String,
    execute: bool,
    cu_limit: u32,
    cu_price: u64,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        rpc_url: DEVNET_RPC.to_string(),
        cu_limit: 300_000,
        cu_price: 50_000,
        ..Default::default()
    };
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--plan" => {
                i += 1;
                a.plan = raw.get(i).cloned().ok_or_else(|| anyhow!("--plan needs a value"))?;
            }
            "--keypair" => {
                i += 1;
                a.keypair = raw.get(i).cloned().ok_or_else(|| anyhow!("--keypair needs a value"))?;
            }
            "--buyer-key" => {
                i += 1;
                a.buyer_keys.push(
                    raw.get(i).cloned().ok_or_else(|| anyhow!("--buyer-key needs a value"))?,
                );
            }
            "--tip-account" => {
                i += 1;
                a.tip_account = Some(
                    raw.get(i).cloned().ok_or_else(|| anyhow!("--tip-account needs a value"))?,
                );
            }
            "--rpc-url" => {
                i += 1;
                a.rpc_url = raw.get(i).cloned().ok_or_else(|| anyhow!("--rpc-url needs a value"))?;
            }
            "--jito-url" => {
                i += 1;
                a.jito_url = raw.get(i).cloned().ok_or_else(|| anyhow!("--jito-url needs a value"))?;
            }
            "--execute" => a.execute = true,
            "--dry-run" => a.execute = false,
            "--cu-limit" => {
                i += 1;
                a.cu_limit = raw
                    .get(i)
                    .ok_or_else(|| anyhow!("--cu-limit needs a value"))?
                    .parse()
                    .context("bad --cu-limit")?;
            }
            "--cu-price" => {
                i += 1;
                a.cu_price = raw
                    .get(i)
                    .ok_or_else(|| anyhow!("--cu-price needs a value"))?
                    .parse()
                    .context("bad --cu-price")?;
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other} (see --help)"),
        }
        i += 1;
    }
    if a.plan.is_empty() {
        anyhow::bail!("--plan <plan.json> is required");
    }
    if a.keypair.is_empty() {
        anyhow::bail!("--keypair <path> is required");
    }
    Ok(a)
}

fn print_help() {
    println!(
        "\
launch-bundle: Pump.fun anti-snipe launch bundler

USAGE:
    launch-bundle --plan plan.json --keypair <path> [OPTIONS]

OPTIONS:
    --plan <path>         Launch plan JSON (required)
    --keypair <path>      Creator keypair file, Solana CLI JSON format (required)
    --buyer-key <path>    Extra buyer keypair file (repeatable; one per buyer
                          wallet in the plan that isn't the creator)
    --tip-account <addr>  Jito tip destination (required if plan.tip_lamports > 0)
    --rpc-url <url>       Solana RPC (default: devnet)
    --jito-url <url>      Jito block engine base URL (required for --execute)
    --dry-run             Build + simulate only (default; submits nothing)
    --execute             Actually submit the bundle via Jito
    --cu-limit <n>        Compute unit limit (default 300000)
    --cu-price <n>        Compute unit price, microlamports (default 50000)

SAFETY:
    --execute refuses mainnet RPCs unless I_UNDERSTAND_MAINNET=1 is set."
    );
}

/// Hard mainnet gate: refuse to submit against mainnet unless the operator
/// explicitly opts in via the environment.
fn mainnet_gate(rpc_url: &str, execute: bool) -> Result<()> {    if !execute {
        return Ok(());
    }
    let looks_mainnet = rpc_url.contains("mainnet");
    if looks_mainnet && std::env::var("I_UNDERSTAND_MAINNET").as_deref() != Ok("1") {
        anyhow::bail!(
            "REFUSING to execute against mainnet without I_UNDERSTAND_MAINNET=1 in the environment. \
             This spends real SOL. Re-run with the variable set if you truly mean it."
        );
    }
    Ok(())
}

/// Stealth launch flow (plan.stealth is set).
///
/// 1. Build + simulate the multi-hop funding transactions (own blockhash).
/// 2. `--execute`: submit funding sequentially via direct RPC, confirmed.
/// 3. Build + simulate the launch transactions with a fresh blockhash:
///    the Jito bundle holds [create] (+ tip), or [create, buys…, tip] when
///    spread_slots == 0; spread buys come back separately.
/// 4. `--execute`: Jito-submit the bundle, then submit each spread buy via
///    direct RPC once its target slot is reached.
async fn run_stealth(
    args: &Args,
    plan: &plan::LaunchPlan,
    cfg: &stealth::StealthConfig,
    creator: &solana_keypair::Keypair,
    buyer_keys: &HashMap<Pubkey, solana_keypair::Keypair>,
    tip_account: Option<Pubkey>,
) -> Result<()> {
    let splan = stealth::plan_stealth(plan, cfg)?;
    let creator_pk = creator.pubkey();
    println!(
        "stealth: {} hop(s)/buyer, spread {} slot(s), jitter ±{} bps, buffer {:.3} SOL",
        cfg.funding_hops, cfg.spread_slots, cfg.amount_jitter_bps, cfg.funding_buffer_sol
    );
    for b in &splan.buyers {
        println!(
            "  buy {}: {} spends {:.6} SOL (plan {:.6}, jitter {:+} bps) -> slot +{}",
            b.buy_index,
            b.buyer,
            b.jittered_amount_lamports as f64 / 1e9,
            b.planned_amount_lamports as f64 / 1e9,
            b.jitter_bps_applied,
            b.target_slot_offset,
        );
    }
    let fund_total = splan.total_creator_funding_lamports(&creator_pk);
    println!(
        "  funding: {} transfer(s) across {} buyer chain(s); creator sends {:.6} SOL total",
        splan.funding_transfers.len(),
        splan.buyers.len(),
        fund_total as f64 / 1e9,
    );

    // --- 1. Funding transactions (their own blockhash; they confirm first). ---
    let fund_blockhash = tx_builder::fetch_latest_blockhash(&args.rpc_url).await?;
    let funding_txs =
        bundle::build_funding_transactions(&splan, creator, fund_blockhash)?;
    println!("simulating/building {} funding transaction(s)...", funding_txs.len());
    if args.execute {
        // Interleaved per hop: a hop wallet only exists (and is funded)
        // after the previous transfer confirms, so each step is simulated
        // right before it is submitted.
        println!("submitting funding chain (simulate -> submit -> confirm per hop)...");
        for (i, tx) in funding_txs.iter().enumerate() {
            let sim = protection::simulate_transaction(&args.rpc_url, tx).await?;
            if !sim.success {
                anyhow::bail!("funding tx {i} simulation FAILED: {:?}", sim.error);
            }
            let sig = local::send_and_confirm_tx(&args.rpc_url, tx).await?;
            println!("  funding tx {i}: simulated ok, confirmed {sig}");
        }
    } else {
        // Dry-run: only the creator-signed legs have a funded sender right
        // now; hop legs simulate fine only after the prior hop confirms
        // (which is exactly what --execute does, step by step).
        for (i, (tx, t)) in funding_txs.iter().zip(splan.funding_transfers.iter()).enumerate() {
            if t.from == creator_pk.to_string() {
                let sim = protection::simulate_transaction(&args.rpc_url, tx).await?;
                if !sim.success {
                    anyhow::bail!("funding tx {i} simulation FAILED: {:?}", sim.error);
                }
                println!("  funding tx {i}: simulation ok");
            } else {
                println!("  funding tx {i}: build ok (sender funded when prior hop confirms)");
            }
        }
    }

    // --- 2. Launch transactions (fresh blockhash after funding confirmed). ---
    let sb = bundle::build_stealth_bundle_txs(
        &args.rpc_url,
        plan,
        creator,
        buyer_keys,
        tip_account,
        args.cu_limit,
        args.cu_price,
        &splan,
    )
    .await?;
    println!("new token mint: {}", sb.mint);
    println!(
        "built {} bundle tx(s) + {} spread buy(s)",
        sb.bundle_transactions.len(),
        sb.spread_buys.len(),
    );
    let mut all_txs: Vec<(&str, &String)> = sb
        .bundle_transactions
        .iter()
        .map(|t| ("bundle", t))
        .collect();
    for (bi, tx) in &sb.spread_buys {
        all_txs.push(("spread", tx));
        let _ = bi;
    }
    println!("simulating {} transaction(s)...", all_txs.len());
    for (i, (kind, tx)) in all_txs.iter().enumerate() {
        let sim = protection::simulate_transaction(&args.rpc_url, tx).await?;
        if !sim.success {
            anyhow::bail!("{kind} tx {i} simulation FAILED: {:?}", sim.error);
        }
        println!("  {kind} tx {i}: simulation ok");
    }

    if !args.execute {
        println!("\n--dry-run: funding + launch transactions built and simulated OK. Nothing submitted.");
        println!("new mint would be: {}", sb.mint);
        return Ok(());
    }

    // --- 3. Submit: Jito bundle first, then spread buys at their slots. ---
    if args.jito_url.is_empty() {
        anyhow::bail!("--execute requires --jito-url <block-engine base URL>");
    }
    let jito = JitoRelay::new(&args.jito_url);
    println!("submitting bundle to Jito...");
    let submission = jito.submit_signed_transactions(&sb.bundle_transactions).await?;
    println!("bundle id: {}", submission.bundle_id);
    let status = jito.wait_for_landing(&submission.bundle_id, 20, Duration::from_millis(500)).await?;
    println!("landing status: {status:?}");
    if status.err.is_some() {
        anyhow::bail!("bundle failed on-chain: {:?}", status.err);
    }

    if !sb.spread_buys.is_empty() {
        let anchor_slot = local::fetch_current_slot(&args.rpc_url).await?;
        println!("create landed ~slot {anchor_slot}; submitting spread buys...");
        for (buy_index, tx) in &sb.spread_buys {
            let offset = splan.buyers[*buy_index].target_slot_offset;
            let target = anchor_slot + offset;
            loop {
                let slot = local::fetch_current_slot(&args.rpc_url).await?;
                if slot >= target {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            let sig = local::send_and_confirm_tx(&args.rpc_url, tx).await?;
            println!("  spread buy {buy_index} (+{offset} slots): {sig}");
        }
    }

    println!("\nLAUNCHED (stealth). Token mint: {}", sb.mint);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    mainnet_gate(&args.rpc_url, args.execute)?;

    // Load the plan (validates amounts, wallets, bundle size).
    let plan = plan::LaunchPlan::load(&args.plan)?;
    println!(
        "plan: {} ({}) + {} initial buy(s), {:.3} SOL total",
        plan.token_name,
        plan.token_symbol,
        plan.initial_buys.len(),
        plan.total_buy_sol()
    );

    // Load keypairs from files only. Never printed or logged.
    let creator = local::load_keypair_file(&args.keypair)?;
    println!("creator: {}", creator.pubkey());
    let mut buyer_keys: HashMap<Pubkey, solana_keypair::Keypair> = HashMap::new();
    for path in &args.buyer_keys {
        let kp = local::load_keypair_file(path)?;
        println!("loaded buyer key: {}", kp.pubkey());
        buyer_keys.insert(kp.pubkey(), kp);
    }

    let tip_account = match &args.tip_account {
        Some(s) => Some(Pubkey::from_str(s).context("bad --tip-account")?),
        None => None,
    };

    // Stealth path: multi-hop funding + jittered amounts + slot-spread buys.
    if let Some(cfg) = plan.stealth.clone() {
        cfg.validate()?;
        return run_stealth(&args, &plan, &cfg, &creator, &buyer_keys, tip_account).await;
    }

    // Build every transaction sharing one recent blockhash.
    let bundle = bundle::build_launch_bundle(
        &args.rpc_url,
        &plan,
        &creator,
        &buyer_keys,
        tip_account,
        args.cu_limit,
        args.cu_price,
    )
    .await?;
    println!("new token mint: {}", bundle.mint);
    println!(
        "built {} transaction(s): [create, {} buy(s){}]",
        bundle.transactions.len(),
        plan.initial_buys.len(),
        if plan.tip_lamports > 0 { ", tip" } else { "" }
    );

    // Simulate every transaction (dry-run AND execute: never submit garbage).
    println!("simulating {} transaction(s)...", bundle.transactions.len());
    for (i, tx) in bundle.transactions.iter().enumerate() {
        let sim = protection::simulate_transaction(&args.rpc_url, tx).await?;
        if !sim.success {
            eprintln!("simulation error: {:?}", sim.error);
            for line in &sim.logs {
                eprintln!("  log: {line}");
            }
            anyhow::bail!(
                "transaction {i} simulation FAILED; refusing to submit. \
                 (If the error is AccountNotFound, the payer wallet likely has no SOL \
                 yet — fund it first, then re-run.)"
            );
        }
        println!("  tx {i}: simulation ok");
    }

    if !args.execute {
        println!("\n--dry-run: all transactions built and simulated OK. Nothing submitted.");
        println!("new mint would be: {}", bundle.mint);
        return Ok(());
    }

    // --- LIVE SUBMISSION (only reachable with --execute + mainnet gate) ---
    if args.jito_url.is_empty() {
        anyhow::bail!("--execute requires --jito-url <block-engine base URL>");
    }
    let jito = JitoRelay::new(&args.jito_url);
    println!("submitting bundle to Jito...");
    let submission = jito.submit_signed_transactions(&bundle.transactions).await?;
    println!("bundle id: {}", submission.bundle_id);
    let status = jito
        .wait_for_landing(
            &submission.bundle_id,
            20,
            std::time::Duration::from_millis(500),
        )
        .await?;
    println!("landing status: {status:?}");
    if status.err.is_some() {
        anyhow::bail!("bundle failed on-chain: {:?}", status.err);
    }
    println!("\nLAUNCHED. Token mint: {}", bundle.mint);
    Ok(())
}
