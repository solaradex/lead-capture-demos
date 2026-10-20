//! Devnet end-to-end proof for the stealth funding chain.
//!
//! This is TEST tooling, not product code. It proves on devnet:
//!   1. The multi-hop funding plan builds (creator -> hops -> buyer).
//!   2. Every funding transaction simulates AND executes on-chain,
//!      submitted sequentially in plan order.
//!   3. Buyers end with exactly jittered_buy + buffer; every hop ends at 0.
//!   4. A seeded rebuild produces identical jittered amounts (the two-call
//!      API pattern: fund first, build the bundle later with the same seed).
//!
//! Usage:
//!   DEVNET_RPC="https://devnet.helius-rpc.com/?api-key=<key>" \
//!     stealth-proof /path/to/funder-keypair.json
//!
//! Costs a few cents of devSOL (2 buyers x 2 hops here). Never point at mainnet.

#[path = "../bundle.rs"]
mod bundle;
#[path = "../create.rs"]
mod create;
#[path = "../local.rs"]
mod local;
#[path = "../plan.rs"]
mod plan;
#[path = "../stealth.rs"]
mod stealth;

use anyhow::{anyhow, Context, Result};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_sniper::{protection, tx_builder};
use std::str::FromStr;

use plan::{InitialBuy, LaunchPlan};
use stealth::StealthConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let rpc_url = std::env::var("DEVNET_RPC").context("DEVNET_RPC env var is required")?;
    if rpc_url.contains("mainnet") {
        return Err(anyhow!("REFUSING: this tool is devnet-only"));
    }
    let keypair_path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: stealth-proof <funder-keypair.json>"))?;
    let funder = local::load_keypair_file(&keypair_path)?;
    println!("funder: {}", funder.pubkey());
    let start = local::fetch_balance_lamports(&rpc_url, &funder.pubkey()).await?;
    println!("funder balance: {:.6} SOL", start as f64 / 1e9);

    // Two buyers, fresh wallets, 0.05 and 0.08 SOL planned.
    let buyer_a = Keypair::new();
    let buyer_b = Keypair::new();
    let launch_plan = LaunchPlan {
        token_name: "Stealth Proof".into(),
        token_symbol: "STLTH".into(),
        token_uri: "https://example.com/stealth.json".into(),
        creator: funder.pubkey().to_string(),
        initial_buys: vec![
            InitialBuy {
                wallet: buyer_a.pubkey().to_string(),
                sol_amount: 0.05,
            },
            InitialBuy {
                wallet: buyer_b.pubkey().to_string(),
                sol_amount: 0.08,
            },
        ],
        tip_lamports: 0,
        slippage_bps: 500,
        stealth: None,
    };
    launch_plan.validate()?;

    let cfg = StealthConfig {
        funding_hops: 2,
        spread_slots: 3,
        amount_jitter_bps: 500,
        funding_buffer_sol: 0.015,
        amount_seed: Some(42),
    };
    cfg.validate()?;
    let splan = stealth::plan_stealth(&launch_plan, &cfg)?;
    println!(
        "stealth plan: {} funding transfers, {} hop wallets",
        splan.funding_transfers.len(),
        splan.hop_secrets_bs58().len(),
    );
    for b in &splan.buyers {
        println!(
            "  buyer {}: plan {:.6} -> jittered {:.6} SOL ( {:+} bps ), slot +{}, needs {:.6} SOL",
            b.buy_index,
            b.planned_amount_lamports as f64 / 1e9,
            b.jittered_amount_lamports as f64 / 1e9,
            b.jitter_bps_applied,
            b.target_slot_offset,
            b.funding_needed_lamports as f64 / 1e9,
        );
    }
    let expect_fund = splan.total_creator_funding_lamports(&funder.pubkey());
    println!(
        "  creator must send {:.6} SOL total",
        expect_fund as f64 / 1e9
    );

    // Determinism check: rebuild with the same seed => identical amounts.
    let splan2 = stealth::plan_stealth(&launch_plan, &cfg)?;
    for (a, b) in splan.buyers.iter().zip(splan2.buyers.iter()) {
        if a.jittered_amount_lamports != b.jittered_amount_lamports {
            return Err(anyhow!("seeded rebuild mismatch: two-call pattern broken"));
        }
    }
    println!("  determinism check: same seed => identical jittered amounts ✓");

    // Build the signed funding transactions (own blockhash).
    let blockhash = tx_builder::fetch_latest_blockhash(&rpc_url).await?;
    let funding_txs = bundle::build_funding_transactions(&splan, &funder, blockhash)?;
    assert_eq!(funding_txs.len(), splan.funding_transfers.len());

    // Simulate + submit each transfer in sequence: a hop only exists (and is
    // funded) after the previous transfer confirms, so the chain cannot be
    // simulated up-front — each step is simulated right before submission.
    let mut sigs = Vec::new();
    for (i, tx) in funding_txs.iter().enumerate() {
        let t = &splan.funding_transfers[i];
        println!(
            "  [{}] {} -> {} : {:.6} SOL",
            i,
            &t.from[..8],
            &t.to[..8],
            t.lamports as f64 / 1e9
        );
        let sim = protection::simulate_transaction(&rpc_url, tx).await?;
        if !sim.success {
            return Err(anyhow!(
                "funding tx {i} simulation FAILED: {:?}",
                sim.error
            ));
        }
        let sig = local::send_and_confirm_tx(&rpc_url, tx).await?;
        println!("       simulated ok, confirmed: {sig}");
        sigs.push(sig);
    }

    // Verify end state: buyers hold exactly need; hops hold exactly 0.
    for b in &splan.buyers {
        let buyer = Pubkey::from_str(&b.buyer).unwrap();
        let bal = local::fetch_balance_lamports(&rpc_url, &buyer).await?;
        println!(
            "  buyer {} balance: {:.6} SOL (expected {:.6})",
            b.buy_index,
            bal as f64 / 1e9,
            b.funding_needed_lamports as f64 / 1e9
        );
        if bal != b.funding_needed_lamports {
            return Err(anyhow!(
                "buyer {} balance mismatch: got {bal}, want {}",
                b.buy_index,
                b.funding_needed_lamports
            ));
        }
        for hw in &b.hop_wallets {
            let hop = Pubkey::from_str(hw).unwrap();
            let hbal = local::fetch_balance_lamports(&rpc_url, &hop).await?;
            if hbal != 0 {
                return Err(anyhow!("hop {hw} did not end at zero: {hbal}"));
            }
        }
    }
    println!("all hop wallets ended at 0 ✓");

    println!("\nSTEALTH FUNDING PROOF COMPLETE");
    println!("funding signatures:");
    for s in &sigs {
        println!("  {s}");
    }
    Ok(())
}
