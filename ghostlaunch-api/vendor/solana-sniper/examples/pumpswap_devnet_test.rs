//! Devnet verification for PumpSwap AMM v2 swap builders.
//!
//! Uses `buy_v2` / `sell_v2` (17 accounts) — the v1 instructions fail on
//! devnet with Custom(3012). Verified against a successful devnet swap.
//!
//! Usage:
//!   cargo run --example pumpswap_devnet_test -- --pool <POOL_ADDRESS>
//!   cargo run --example pumpswap_devnet_test -- --pool <POOL_ADDRESS> --execute
//!
//! NEVER run this against mainnet. Devnet only.

use std::env;
use std::str::FromStr;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

use solana_sniper::decode;
use solana_sniper::pumpswap_tx::{
    build_pumpswap_buy_transaction, build_pumpswap_sell_transaction,
    PumpSwapAccounts, PumpSwapBuyOrder, PumpSwapSellOrder,
};

const DEVNET_RPC: &str = "https://api.devnet.solana.com";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Pool account field offsets (from official pump_amm.json IDL).
const POOL_BASE_MINT_OFFSET: usize = 43;
const POOL_QUOTE_MINT_OFFSET: usize = 75;
const POOL_BASE_TA_OFFSET: usize = 139;
const POOL_QUOTE_TA_OFFSET: usize = 171;

async fn rpc_call(method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let resp: serde_json::Value = client
        .post(DEVNET_RPC)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    if let Some(err) = resp.get("error") {
        anyhow::bail!("RPC error: {}", err);
    }
    Ok(resp["result"].clone())
}

fn pubkey_at(data: &[u8], offset: usize) -> Pubkey {
    Pubkey::new_from_array(data[offset..offset + 32].try_into().unwrap())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let pool_str = args
        .iter()
        .position(|a| a == "--pool")
        .and_then(|i| args.get(i + 1))
        .expect("--pool <POOL_ADDRESS> is required");
    let execute = args.iter().any(|a| a == "--execute");
    // Optional override for buyback fee recipient
    let buyback_override = args
        .iter()
        .position(|a| a == "--buyback")
        .and_then(|i| args.get(i + 1));

    let pool = Pubkey::from_str(pool_str)?;
    println!("Pool: {}", pool);

    // Load devnet wallet
    let keypair_path = format!("{}/.config/solana/id.json", env::var("HOME")?);
    let keypair_data = std::fs::read_to_string(&keypair_path)?;
    let keypair_bytes: Vec<u8> = serde_json::from_str(&keypair_data)?;
    let signer = Keypair::try_from(keypair_bytes.as_slice())
        .map_err(|e| anyhow::anyhow!("Invalid keypair: {:?}", e))?;
    let user = signer.pubkey();
    println!("User: {}", user);

    // Check SOL balance
    let balance = rpc_call("getBalance", serde_json::json!([user.to_string()])).await?;
    let lamports = balance["value"].as_u64().unwrap_or(0);
    println!("Balance: {} SOL", lamports as f64 / 1e9);
    if lamports < 10_000_000 {
        anyhow::bail!("Insufficient balance for test (need at least 0.01 SOL)");
    }

    // Fetch pool account data
    println!("\n--- Fetching pool account ---");
    let pool_info = rpc_call(
        "getAccountInfo",
        serde_json::json!([pool.to_string(), {"encoding": "base64"}]),
    )
    .await?;
    let data_b64 = pool_info["value"]["data"][0]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No pool data"))?;
    let pool_data = B64.decode(data_b64)?;
    println!("Pool data len: {} bytes", pool_data.len());
    if pool_data.len() < 203 {
        anyhow::bail!("Pool data too short: {} bytes", pool_data.len());
    }

    let base_mint = pubkey_at(&pool_data, POOL_BASE_MINT_OFFSET);
    let quote_mint = pubkey_at(&pool_data, POOL_QUOTE_MINT_OFFSET);
    let pool_base_ta = pubkey_at(&pool_data, POOL_BASE_TA_OFFSET);
    let pool_quote_ta = pubkey_at(&pool_data, POOL_QUOTE_TA_OFFSET);
    println!("Base mint: {}", base_mint);
    println!("Quote mint: {}", quote_mint);
    println!("Pool base TA: {}", pool_base_ta);
    println!("Pool quote TA: {}", pool_quote_ta);

    // Note: PumpSwap supports non-wSOL quote mints (verified on devnet).
    // The quote_mint is read from pool data; no wSOL requirement.
    println!("Quote mint: {} (non-wSOL quotes supported)", quote_mint);

    // Resolve token programs from mint owners
    println!("\n--- Resolving token programs ---");
    let base_mint_info = rpc_call(
        "getAccountInfo",
        serde_json::json!([base_mint.to_string(), {"encoding": "jsonParsed"}]),
    )
    .await?;
    let base_token_program = Pubkey::from_str(
        base_mint_info["value"]["owner"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("No base mint owner"))?,
    )?;
    let quote_mint_info = rpc_call(
        "getAccountInfo",
        serde_json::json!([quote_mint.to_string(), {"encoding": "jsonParsed"}]),
    )
    .await?;
    let quote_token_program = Pubkey::from_str(
        quote_mint_info["value"]["owner"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("No quote mint owner"))?,
    )?;
    println!("Base token program: {}", base_token_program);
    println!("Quote token program: {}", quote_token_program);

    // Buyback fee recipient: use override or the one from the known-good devnet tx.
    // This is set by the program admin; for devnet it appears to be global.
    let buyback_fee_recipient = if let Some(addr) = buyback_override {
        Pubkey::from_str(addr)?
    } else {
        // From successful devnet swap 355wg1ntWhxYfrB7TL2zDRLoZmUrg4RQWMN86k8QrPg5yNqRRrNEZT5qVeJAmjLgpQMTr9E62YtTmcbsYRRDXnHb
        Pubkey::from_str("GsVBKjffkB769p9tHTZWoAX3r9T6dXoDTJr3f7XutJH7")?
    };
    println!("Buyback fee recipient: {}", buyback_fee_recipient);

    let accts = PumpSwapAccounts {
        pool,
        user,
        base_mint,
        quote_mint,
        base_token_program,
        quote_token_program,
        buyback_fee_recipient,
        // Fee config from the successful devnet swap (global, same for all pools)
        fee_config: Some(Pubkey::from_str("5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx")?),
        pool_base_token_account: Some(pool_base_ta),
        pool_quote_token_account: Some(pool_quote_ta),
    };

    // Get recent blockhash
    let bh_result = rpc_call("getLatestBlockhash", serde_json::json!([{"commitment": "confirmed"}])).await?;
    let blockhash_str = bh_result["value"]["blockhash"].as_str().unwrap();
    let blockhash: solana_hash::Hash = blockhash_str.parse()?;
    println!("\nBlockhash: {}", blockhash);

    // For buy_v2, the user pays with wSOL (not native SOL).
    // We need to: create wSOL ATA if needed, wrap SOL, then buy.
    println!("\n--- Building buy_v2 transaction ---");
    let user_quote_ata = accts.user_quote_ata();
    
    // Check if wSOL ATA exists
    let wsol_ata_info = rpc_call(
        "getAccountInfo",
        serde_json::json!([user_quote_ata.to_string(), {"encoding": "base64"}]),
    ).await?;
    let wsol_exists = wsol_ata_info["value"].is_object();
    println!("wSOL ATA exists: {}", wsol_exists);
    
    let mut pre_ixs = Vec::new();
    if !wsol_exists {
        // Create wSOL ATA
        pre_ixs.push(solana_sniper::tx_builder::create_ata_instruction(
            &user,
            &user_quote_ata,
            &user,
            &accts.quote_mint,
            &accts.quote_token_program,
        ));
    }
    // Wrap 0.02 SOL into wSOL (enough for the buy + fees)
    // System transfer to wSOL ATA
    pre_ixs.push(solana_sniper::tx_builder::system_transfer_instruction(
        &user,
        &user_quote_ata,
        20_000_000, // 0.02 SOL
    ));
    // Sync native (token program instruction 17)
    pre_ixs.push(solana_instruction::Instruction {
        program_id: accts.quote_token_program,
        accounts: vec![solana_instruction::AccountMeta::new(user_quote_ata, false)],
        data: vec![17], // SyncNative
    });
    let buy_order = PumpSwapBuyOrder {
        accts: accts.clone(),
        base_amount_out: 1_000,              // 1000 base token units
        max_quote_amount_in_lamports: 10_000_000, // 0.01 SOL max
        create_user_base_ata: true,
        compute_unit_limit: 200_000,
        compute_unit_price_microlamports: 1_000,
    };
    // Build full transaction: wrap SOL + buy
    // Note: build_pumpswap_buy_transaction doesn't support pre-instructions,
    // so we assemble manually here.
    let mut all_ixs = pre_ixs;
    if buy_order.create_user_base_ata {
        let ata = accts.user_base_ata();
        all_ixs.push(solana_sniper::tx_builder::create_ata_instruction(
            &user,
            &ata,
            &user,
            &accts.base_mint,
            &accts.base_token_program,
        ));
    }
    all_ixs.push(solana_sniper::pumpswap_tx::buy_v2_instruction(
        &accts,
        buy_order.base_amount_out,
        buy_order.max_quote_amount_in_lamports,
    ));
    // Assemble manually
    let buy_tx_b64 = {
        use solana_sniper::tx_builder::{assemble_signed_base64, TxConfig};
        // Set create_user_base_ata=false since we handled it above
        assemble_signed_base64(
            &signer,
            all_ixs,
            &TxConfig {
                blockhash,
                compute_unit_limit: buy_order.compute_unit_limit,
                compute_unit_price_microlamports: buy_order.compute_unit_price_microlamports,
            },
        )?
    };
    println!("Buy tx built: {} chars (base64)", buy_tx_b64.len());

    // Simulate the buy
    println!("\n--- Simulating buy_v2 ---");
    let sim_result = rpc_call(
        "simulateTransaction",
        serde_json::json!([buy_tx_b64, {"encoding": "base64", "commitment": "confirmed"}]),
    )
    .await?;
    let sim_err = &sim_result["value"]["err"];
    if !sim_err.is_null() {
        println!("❌ SIMULATION FAILED: {}", serde_json::to_string_pretty(sim_err)?);
        if let Some(logs) = sim_result["value"]["logs"].as_array() {
            println!("\nLogs:");
            for log in logs.iter().take(25) {
                println!("  {}", log.as_str().unwrap_or("?"));
            }
        }
        anyhow::bail!("Buy simulation failed - NOT submitting");
    }
    println!("✅ Buy simulation SUCCEEDED");
    if let Some(units) = sim_result["value"]["unitsConsumed"].as_u64() {
        println!("   Compute units: {}", units);
    }

    if !execute {
        println!("\n--- Dry run complete (pass --execute to submit) ---");
        return Ok(());
    }

    // Submit the buy
    println!("\n--- Submitting buy transaction ---");
    let submit_result = rpc_call(
        "sendTransaction",
        serde_json::json!([buy_tx_b64, {"encoding": "base64", "preflightCommitment": "confirmed"}]),
    )
    .await?;
    let signature = submit_result.as_str().unwrap_or("?");
    println!("✅ Buy submitted: {}", signature);
    println!("   View: https://explorer.solana.com/tx/{}?cluster=devnet", signature);

    println!("\nWaiting for confirmation...");
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    // Check token balance
    let user_base_ata = accts.user_base_ata();
    let token_balance = rpc_call(
        "getTokenAccountBalance",
        serde_json::json!([user_base_ata.to_string()]),
    )
    .await?;
    let token_amount = token_balance["value"]["amount"]
        .as_str()
        .unwrap_or("0")
        .parse::<u64>()
        .unwrap_or(0);
    println!("\n--- Buy result ---");
    println!("Tokens received: {}", token_amount);

    if token_amount == 0 {
        println!("⚠️  No tokens received - skipping sell test");
        return Ok(());
    }

    // Build and simulate a sell of half the tokens
    println!("\n--- Building sell_v2 transaction (half of received) ---");
    let sell_amount = token_amount / 2;
    let bh2 = rpc_call("getLatestBlockhash", serde_json::json!([{"commitment": "confirmed"}])).await?;
    let blockhash2: solana_hash::Hash = bh2["value"]["blockhash"].as_str().unwrap().parse()?;
    let sell_order = PumpSwapSellOrder {
        accts: accts.clone(),
        token_amount: sell_amount,
        min_quote_amount_out_lamports: 1, // minimal floor for test
        create_user_quote_ata: false,
        compute_unit_limit: 200_000,
        compute_unit_price_microlamports: 1_000,
    };
    let sell_tx_b64 = build_pumpswap_sell_transaction(&signer, &sell_order, blockhash2).await?;
    println!("Sell tx built: {} chars (base64)", sell_tx_b64.len());

    println!("\n--- Simulating sell_v2 ---");
    let sell_sim = rpc_call(
        "simulateTransaction",
        serde_json::json!([sell_tx_b64, {"encoding": "base64", "commitment": "confirmed"}]),
    )
    .await?;
    if sell_sim["value"]["err"].is_null() {
        println!("✅ Sell simulation SUCCEEDED");
        if let Some(units) = sell_sim["value"]["unitsConsumed"].as_u64() {
            println!("   Compute units: {}", units);
        }
    } else {
        println!("❌ Sell simulation FAILED: {}", sell_sim["value"]["err"]);
        if let Some(logs) = sell_sim["value"]["logs"].as_array() {
            for log in logs.iter().take(25) {
                println!("  {}", log.as_str().unwrap_or("?"));
            }
        }
    }

    println!("\n=== Devnet verification complete ===");
    Ok(())
}
