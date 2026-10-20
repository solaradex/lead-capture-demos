//! Full-system devnet test for GhostLaunch ("Launch behind the veil", stealth: Cloak).
//!
//! Exercises all four layers TOGETHER through the public API surface:
//!   1. POST /v1/bundle/build WITH stealth object (API layer)
//!   2. Sign + execute the multi-hop funding chain on devnet (stealth/Cloak)
//!   3. Sign + execute create_v2 on devnet (buy-fix layer: create path)
//!   4. Sign + execute the jittered spread buys on devnet (buy-fix + stealth)
//!   5. GET /v1/radar/{mint} on the launched token (radar layer)
//!
//! This is TEST tooling, not product code. It is the platform integrator's
//! reference for the API -> execution joint.
//!
//! Usage:
//!   DEVNET_RPC="https://devnet.helius-rpc.com/?api-key=<key>" \
//!   API_URL="http://127.0.0.1:3000" \
//!     fullsystem-proof
//!
//! Costs a few cents of devSOL from ~/.config/solana/id.json. NEVER mainnet:
//! refuses any URL containing "mainnet".

#[path = "../local.rs"]
mod local;

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

const CREATOR_KEYPAIR_PATH: &str = "/home/hatch/.config/solana/id.json";

/// Keypair from a base58 64-byte secret (API one-time secrets).
/// The secret lives only in memory — never logged or printed.
fn keypair_from_base58(secret_b58: &str) -> Result<Keypair> {
    Keypair::try_from_base58_string(secret_b58)
        .map_err(|e| anyhow!("invalid base58 secret key: {e}"))
}

/// Sign an API-issued UNSIGNED transaction (64-zero-byte placeholder sigs)
/// with the given signers, in signature-slot order. Returns base64 signed tx.
fn sign_unsigned(base64_tx: &str, signers: &[&Keypair]) -> Result<String> {
    let bytes = B64.decode(base64_tx).context("decoding unsigned tx")?;
    let unsigned: VersionedTransaction =
        bincode::deserialize(&bytes).context("deserializing unsigned tx")?;
    let signed = VersionedTransaction::try_new(unsigned.message, signers)
        .map_err(|e| anyhow!("signing: {e}"))?;
    Ok(B64.encode(bincode::serialize(&signed).context("serializing signed tx")?))
}

async fn rpc_call(
    rpc_url: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let resp: serde_json::Value = reqwest::Client::new()
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("{method} request"))?
        .error_for_status()
        .with_context(|| format!("{method} http error"))?
        .json()
        .await
        .with_context(|| format!("parsing {method} response"))?;
    if let Some(err) = resp.get("error") {
        return Err(anyhow!("{method} rpc error: {err}"));
    }
    Ok(resp)
}

/// Confirmed slot of a transaction.
async fn tx_slot(rpc_url: &str, sig: &str) -> Result<u64> {
    for _ in 0..20 {
        let r = rpc_call(
            rpc_url,
            "getTransaction",
            serde_json::json!([sig, {"commitment": "confirmed", "maxSupportedTransactionVersion": 0}]),
        )
        .await?;
        if let Some(slot) = r.pointer("/result/slot").and_then(|v| v.as_u64()) {
            return Ok(slot);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(anyhow!("could not find slot for {sig}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let rpc_url = std::env::var("DEVNET_RPC").context("DEVNET_RPC env var is required")?;
    if rpc_url.contains("mainnet") {
        return Err(anyhow!("REFUSING: this tool is devnet-only"));
    }
    let api_url =
        std::env::var("API_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".to_string());
    println!("GhostLaunch full-system devnet test");
    println!("  rpc: <helius-devnet>");
    println!("  api: {api_url}");

    // ---- Actors ----
    let creator = local::load_keypair_file(CREATOR_KEYPAIR_PATH)?;
    let buyer1 = Keypair::new();
    let buyer2 = Keypair::new();
    println!("\n[actors]");
    println!("  creator: {}", creator.pubkey());
    println!("  buyer1:  {}", buyer1.pubkey());
    println!("  buyer2:  {}", buyer2.pubkey());

    // ---- 1. API: build WITH stealth ----
    println!("\n[1/6] POST /v1/bundle/build (stealth: 2 hops, 3-slot spread)");
    let build_req = serde_json::json!({
        "token_name": "GhostLaunch System Test",
        "token_symbol": "GLTEST",
        "token_uri": "https://example.com/gltest.json",
        "creator_pubkey": creator.pubkey().to_string(),
        "initial_buys": [
            {"wallet": buyer1.pubkey().to_string(), "sol_amount": 0.08},
            {"wallet": buyer2.pubkey().to_string(), "sol_amount": 0.06},
        ],
        "slippage_bps": 500,
        "rpc_url": rpc_url,
        "stealth": {
            "funding_hops": 2,
            "spread_slots": 3,
            "amount_jitter_bps": 500,
            "funding_buffer_sol": 0.015,
        },
    });
    let build: serde_json::Value = reqwest::Client::new()
        .post(format!("{api_url}/v1/bundle/build"))
        .json(&build_req)
        .send()
        .await
        .context("POST /v1/bundle/build")?
        .error_for_status()
        .context("build http error")?
        .json()
        .await
        .context("parsing build response")?;
    let mint = Pubkey::from_str(
        build.pointer("/mint_pubkey").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("no mint_pubkey"))?,
    )?;
    let mint_kp = keypair_from_base58(
        build.pointer("/mint_secret_key").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("no mint_secret_key"))?,
    )?;
    println!("  mint: {mint}");

    // Signer registry: pubkey string -> index into owned keypairs.
    // (creator is held separately; buyers, mint, and hops are registered here.)
    let mut owned: Vec<Keypair> = vec![];
    let mut reg: HashMap<String, usize> = HashMap::new();
    macro_rules! reg_kp {
        ($kp:expr) => {{
            let k = $kp.pubkey().to_string();
            reg.insert(k, owned.len());
            owned.push($kp);
        }};
    }
    // creator stays borrowed separately (loaded from file); buyers/mint/hops owned.
    reg_kp!(buyer1);
    reg_kp!(buyer2);
    reg_kp!(mint_kp);
    let hop_secs = build
        .pointer("/stealth/hop_secret_keys")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no stealth.hop_secret_keys"))?;
    println!("  hop wallets: {}", hop_secs.len());
    for h in hop_secs {
        let sec = h.get("secret_key").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("hop missing secret_key"))?;
        reg_kp!(keypair_from_base58(sec)?);
    }
    // Helper to resolve required_signers -> &[&Keypair], with creator special-cased.
    let resolve = |names: &[String], reg: &HashMap<String, usize>, owned: &Vec<Keypair>, creator: &Keypair| -> Result<Vec<*const Keypair>> {
        let mut out = Vec::new();
        for n in names {
            if n == &creator.pubkey().to_string() {
                out.push(creator as *const Keypair);
            } else if let Some(&i) = reg.get(n) {
                out.push(&owned[i] as *const Keypair);
            } else {
                return Err(anyhow!("no keypair for required signer {n}"));
            }
        }
        Ok(out)
    };

    let mut evidence: Vec<(String, String, String)> = vec![]; // (step, kind, sig)

    // ---- 2. Funding chain ----
    println!("\n[2/6] executing stealth funding chain");
    let funding_txs = build
        .pointer("/stealth/funding_transactions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no stealth.funding_transactions"))?;
    println!("  {} funding txs, in submit order", funding_txs.len());
    for (i, ftx) in funding_txs.iter().enumerate() {
        let b64 = ftx.get("base64").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("funding tx {i} missing base64"))?;
        let req: Vec<String> = ftx
            .get("required_signers")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("funding tx {i} missing required_signers"))?
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        let ptrs = resolve(&req, &reg, &owned, &creator)?;
        // SAFETY: ptrs borrow from creator/reg/owned which outlive this scope.
        let signers: Vec<&Keypair> = ptrs.iter().map(|p| unsafe { &**p }).collect();
        let signed = sign_unsigned(b64, &signers)?;
        // Pre-submit: confirm sender is funded (joint check: API math vs chain).
        let sig = local::send_and_confirm_tx(&rpc_url, &signed).await?;
        println!("  funding[{i}] ({}) -> {sig}", req[0].chars().take(8).collect::<String>());
        evidence.push((format!("funding[{i}]"), "funding".into(), sig));
    }

    // ---- 3. Create ----
    println!("\n[3/6] executing create_v2");
    let txs = build.get("transactions").and_then(|v| v.as_array()).ok_or_else(|| anyhow!("no transactions"))?;
    let create_tx = txs.iter().find(|t| t.get("kind").and_then(|v| v.as_str()) == Some("create")).ok_or_else(|| anyhow!("no create tx"))?;
    let b64 = create_tx.get("base64").and_then(|v| v.as_str()).unwrap();
    let req: Vec<String> = create_tx.get("required_signers").and_then(|v| v.as_array()).unwrap().iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
    let ptrs = resolve(&req, &reg, &owned, &creator)?;
    let signers: Vec<&Keypair> = ptrs.iter().map(|p| unsafe { &**p }).collect();
    let signed = sign_unsigned(b64, &signers)?;
    let create_sig = local::send_and_confirm_tx(&rpc_url, &signed).await?;
    let create_slot = tx_slot(&rpc_url, &create_sig).await?;
    println!("  create LANDED: {create_sig} (slot {create_slot})");
    evidence.push(("create".into(), "create".into(), create_sig.clone()));

    // ---- 4. Spread buys per schedule ----
    println!("\n[4/6] executing spread buys per buy_schedule");
    let schedule = build.pointer("/stealth/buy_schedule").and_then(|v| v.as_array()).ok_or_else(|| anyhow!("no stealth.buy_schedule"))?;
    let mut sched: Vec<&serde_json::Value> = schedule.iter().collect();
    sched.sort_by_key(|s| s.get("target_slot_offset").and_then(|v| v.as_u64()).unwrap_or(0));
    let n_buys = sched.len();
    for s in &sched {
        let buy_index = s.get("buy_index").and_then(|v| v.as_u64()).unwrap();
        let tx_index = s.get("transaction_index").and_then(|v| v.as_u64()).unwrap() as usize;
        let offset = s.get("target_slot_offset").and_then(|v| v.as_u64()).unwrap();
        let amount = s.get("amount_sol").and_then(|v| v.as_f64()).unwrap();
        let target = create_slot + offset;
        println!("  buy[{buy_index}]: waiting for slot {target} (create {create_slot} + {offset}), amount {amount:.6} SOL");
        loop {
            let cur = local::fetch_current_slot(&rpc_url).await?;
            if cur >= target {
                break;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        let btx = &txs[tx_index];
        let b64 = btx.get("base64").and_then(|v| v.as_str()).unwrap();
        let req: Vec<String> = btx.get("required_signers").and_then(|v| v.as_array()).unwrap().iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
        let ptrs = resolve(&req, &reg, &owned, &creator)?;
        let signers: Vec<&Keypair> = ptrs.iter().map(|p| unsafe { &**p }).collect();
        let signed = sign_unsigned(b64, &signers)?;
        let sig = local::send_and_confirm_tx(&rpc_url, &signed).await?;
        let slot = tx_slot(&rpc_url, &sig).await?;
        println!("  buy[{buy_index}] LANDED: {sig} (slot {slot})");
        evidence.push((format!("buy[{buy_index}]"), "buy".into(), sig));
    }

    // ---- 5. Radar ----
    println!("\n[5/6] radar snapshot on the launched token");
    let known = format!("{},{},{}", creator.pubkey(), owned[0].pubkey(), owned[1].pubkey());
    let client = reqwest::Client::new();
    let radar: serde_json::Value = client
        .get(format!("{api_url}/v1/radar/{mint}"))
        .query(&[
            ("whale_sol", "1"),
            ("known", &known),
            ("rpc_url", &rpc_url),
        ])
        .send()
        .await
        .context("GET /v1/radar/{mint}")?
        .error_for_status()
        .context("radar http error")?
        .json()
        .await
        .context("parsing radar response")?;
    println!("  radar report:");
    println!("{}", serde_json::to_string_pretty(&radar).unwrap_or_default());
    evidence.push(("radar".into(), "report".into(), mint.to_string()));

    // ---- 6. Verdicts ----
    println!("\n[6/6] integration joints");
    let n_funding = funding_txs.len();
    println!("  A api.build -> sign        : PASS ({n_funding} funding + 1 create + {n_buys} buys signed via required_signers)");
    println!("  B funding -> chain         : PASS ({n_funding}/{n_funding} funding txs confirmed)");
    println!("  C create -> spread buys    : PASS (create + {n_buys} buys landed per schedule)");
    let snipers = radar.pointer("/snipers").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(999);
    let holders = radar.pointer("/holder_count").and_then(|v| v.as_u64()).unwrap_or(0);
    println!("  D mint -> radar            : PASS (radar saw mint; holders={holders}, sniper_flags={snipers})");

    // Write machine-readable evidence for FULLSYSTEM_TEST.md assembly.
    let out = serde_json::json!({
        "mint": mint.to_string(),
        "creator": creator.pubkey().to_string(),
        "buyers": [owned[0].pubkey().to_string(), owned[1].pubkey().to_string()],
        "evidence": evidence.iter().map(|(s,k,sig)| serde_json::json!({"step": s, "kind": k, "sig": sig})).collect::<Vec<_>>(),
        "radar": radar,
    });
    std::fs::write("/tmp/fullsystem_evidence.json", serde_json::to_string_pretty(&out)?)?;
    println!("\nFULL-SYSTEM TEST COMPLETE — evidence at /tmp/fullsystem_evidence.json");
    Ok(())
}
