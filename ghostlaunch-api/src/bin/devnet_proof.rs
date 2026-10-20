//! Devnet end-to-end proof for the launch-bundle toolkit.
//!
//! This is TEST tooling, not product code. It proves on devnet:
//!   1. `create_v2` executes on-chain (new token mint is created).
//!   2. The buy transaction (with ATA creation) simulates AND executes
//!      against the real, now-existing mint.
//!
//! Usage:
//!   DEVNET_RPC="https://devnet.helius-rpc.com/?api-key=<key>" \
//!     devnet_proof /path/to/creator-keypair.json
//!
//! Costs a few cents of devSOL. Never point at mainnet.

#[path = "../create.rs"]
mod create;
#[path = "../local.rs"]
mod local;

use anyhow::{anyhow, Context, Result};
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_sniper::{decode, protection, tx_builder};
use std::time::Duration;

async fn rpc_call(
    rpc_url: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": method, "params": params,
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context(format!("{method} request"))?
        .error_for_status()
        .context(format!("{method} http error"))?
        .json()
        .await
        .context(format!("parsing {method} response"))?;
    if let Some(err) = resp.get("error") {
        return Err(anyhow!("{method} rpc error: {err}"));
    }
    Ok(resp)
}

async fn send_and_confirm(rpc_url: &str, base64_tx: &str) -> Result<String> {
    let resp = rpc_call(
        rpc_url,
        "sendTransaction",
        serde_json::json!([base64_tx, {"encoding": "base64", "preflightCommitment": "confirmed"}]),
    )
    .await?;
    let sig = resp
        .pointer("/result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("no signature in sendTransaction response"))?
        .to_string();
    println!("  submitted: {sig}");

    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let st = rpc_call(
            rpc_url,
            "getSignatureStatuses",
            serde_json::json!([[sig], {"searchTransactionHistory": true}]),
        )
        .await?;
        let val = st.pointer("/result/value/0");
        if let Some(v) = val {
            if v.is_null() {
                continue;
            }
            if let Some(err) = v.get("err") {
                if !err.is_null() {
                    return Err(anyhow!("transaction failed on-chain: {err}"));
                }
            }
            let status = v
                .get("confirmationStatus")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown");
            if status == "confirmed" || status == "finalized" {
                println!("  confirmed ({status})");
                return Ok(sig);
            }
        }
    }
    Err(anyhow!("timed out waiting for confirmation of {sig}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let rpc_url =
        std::env::var("DEVNET_RPC").context("DEVNET_RPC env var is required")?;
    if rpc_url.contains("mainnet") {
        return Err(anyhow!("REFUSING: this tool is devnet-only"));
    }
    let keypair_path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: devnet_proof <creator-keypair.json>"))?;
    let creator = local::load_keypair_file(&keypair_path)?;
    println!("creator: {}", creator.pubkey());

    // Fee recipient from the Pump Global account on devnet.
    let global_data = local::fetch_account_data(&rpc_url, &tx_builder::global_pda())
        .await?
        .ok_or_else(|| anyhow!("Pump Global account not found on devnet"))?;
    let fee_recipient = decode::global_fee_recipient(&global_data)
        .context("parsing Global fee recipient")?;
    println!("fee recipient: {fee_recipient}");

    // ---- 1. CREATE ----
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    println!("\n[1/4] building create_v2 for new mint {mint}");
    let blockhash = tx_builder::fetch_latest_blockhash(&rpc_url).await?;
    let create_ix = create::create_instruction(
        &mint,
        &creator.pubkey(),
        &creator.pubkey(),
        "Devnet Proof Token",
        "DVP",
        "https://example.com/dvp.json",
    );
    let create_tx = local::assemble_signed_base64_multi(
        &[&creator, &mint_kp],
        vec![create_ix],
        blockhash,
        300_000,
        50_000,
    )?;
    let sim = protection::simulate_transaction(&rpc_url, &create_tx).await?;
    if !sim.success {
        return Err(anyhow!("create simulation failed: {:?}", sim.error));
    }
    println!("  create simulation: OK");
    let create_sig = send_and_confirm(&rpc_url, &create_tx).await?;
    println!("[2/4] create LANDED. mint: {mint}  tx: {create_sig}");

    // ---- 2. BUY (against the real, now-existing mint) ----
    println!("\n[3/4] building buy against the live mint");
    let blockhash = tx_builder::fetch_latest_blockhash(&rpc_url).await?;
    let order = tx_builder::BuyOrder {
        accts: tx_builder::PumpAccounts {
            fee_recipient,
            mint,
            creator: creator.pubkey(),
            user: creator.pubkey(),
            token_program: create::token_2022_program(),
            buyback_fee_recipient: tx_builder::buyback_wallet(),
        },
        sol_to_spend_lamports: 50_000_000, // 0.05 devSOL
        expected_tokens_out: 0,
        slippage_bps: 500,
        create_user_ata: true,
        compute_unit_limit: 300_000,
        compute_unit_price_microlamports: 50_000,
    };
    let buy_tx =
        tx_builder::build_buy_transaction(&creator, &order, blockhash).await?;
    let sim = protection::simulate_transaction(&rpc_url, &buy_tx).await?;
    if !sim.success {
        eprintln!("  buy simulation error: {:?}", sim.error);
        for line in &sim.logs {
            eprintln!("  log: {line}");
        }
        return Err(anyhow!("buy simulation FAILED against live mint"));
    }
    println!("  buy simulation: OK");
    let buy_sig = send_and_confirm(&rpc_url, &buy_tx).await?;
    println!("[4/4] buy LANDED. tx: {buy_sig}");

    println!("\nDEVNET PROOF COMPLETE");
    println!("  mint: {mint}");
    println!("  create: {create_sig}");
    println!("  buy:    {buy_sig}");
    Ok(())
}
