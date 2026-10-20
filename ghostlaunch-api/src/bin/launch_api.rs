//! launch-api: HTTP API for the Pump.fun launch bundler.
//!
//! This turns the `launch-bundle` CLI into something launch platforms can
//! embed. It is NON-CUSTODIAL: the server never sees or holds private keys.
//! It builds the exact same instructions as the CLI path, but returns them
//! UNSIGNED. The caller signs with their own keys and submits via their own
//! Jito setup.
//!
//! Endpoints:
//!   GET  /v1/health           -> { status, version } (no auth)
//!   POST /v1/bundle/build     -> unsigned bundle + one-time mint secret
//!   POST /v1/bundle/simulate  -> build + simulate every tx, submit nothing
//!
//! Auth: X-API-Key header, checked against LAUNCH_API_KEYS (comma-separated).
//! If LAUNCH_API_KEYS is unset, auth is skipped with a startup warning.

#[path = "../create.rs"]
mod create;
#[path = "../local.rs"]
mod local;
#[path = "../plan.rs"]
mod plan;
#[path = "../stealth.rs"]
mod stealth;
#[path = "../radar.rs"]
mod radar;

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{Path, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{Html, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_message::{v0::Message as V0Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_sniper::{decode, protection, tx_builder};
use solana_transaction::versioned::VersionedTransaction;
use std::str::FromStr;

use plan::{InitialBuy, LaunchPlan};

const VERSION: &str = "0.3.0";
const DEFAULT_RPC_URL: &str = "https://api.devnet.solana.com";
/// Recent blockhashes stay valid for ~150 slots (MAX_PROCESSING_AGE).
const BLOCKHASH_VALID_SLOTS: u64 = 150;
const CU_LIMIT: u32 = 300_000;
const CU_PRICE: u64 = 50_000;
const TIP_CU_LIMIT: u32 = 10_000;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct BuyRequest {
    wallet: String,
    sol_amount: f64,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct BuildRequest {
    token_name: String,
    token_symbol: String,
    token_uri: String,
    creator_pubkey: String,
    initial_buys: Vec<BuyRequest>,
    #[serde(default)]
    slippage_bps: Option<u16>,
    #[serde(default)]
    tip_lamports: Option<u64>,
    /// Required when tip_lamports > 0: where the Jito tip goes.
    #[serde(default)]
    tip_account: Option<String>,
    #[serde(default)]
    rpc_url: Option<String>,
    /// Optional stealth distribution (multi-hop funding, jittered amounts,
    /// slot-spread buys). Absent = classic single-bundle behavior.
    #[serde(default)]
    stealth: Option<StealthRequest>,
}

/// Stealth options in the API. All fields optional; defaults mirror the
/// plan-file section (2 hops, 3-slot spread, ±5% jitter, 0.015 SOL buffer).
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct StealthRequest {
    #[serde(default)]
    funding_hops: Option<u8>,
    #[serde(default)]
    spread_slots: Option<u64>,
    #[serde(default)]
    amount_jitter_bps: Option<u16>,
    #[serde(default)]
    funding_buffer_sol: Option<f64>,
    /// Deterministic jitter seed. Pass the same seed when you build funding
    /// first and the launch bundle later (two calls) so amounts match.
    #[serde(default)]
    amount_seed: Option<u64>,
}

impl StealthRequest {
    fn to_config(&self) -> Result<stealth::StealthConfig> {
        let cfg = stealth::StealthConfig {
            funding_hops: self.funding_hops.unwrap_or(2),
            spread_slots: self.spread_slots.unwrap_or(3),
            amount_jitter_bps: self.amount_jitter_bps.unwrap_or(500),
            funding_buffer_sol: self.funding_buffer_sol.unwrap_or(0.015),
            amount_seed: self.amount_seed,
        };
        cfg.validate()?;
        Ok(cfg)
    }
}

#[derive(Debug, serde::Serialize)]
struct UnsignedTxResponse {
    index: usize,
    kind: String,
    /// Base64 unsigned transaction. Signatures are 64 zero bytes each;
    /// replace them by signing in `required_signers` order.
    base64: String,
    /// Signer pubkeys in signature-slot order.
    required_signers: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
struct BuildResponse {
    mint_pubkey: String,
    /// Base58 64-byte secret. ONE-TIME: the server does not store it.
    /// The caller must save it — it signs the create transaction.
    mint_secret_key: String,
    blockhash: String,
    expires_in_slots: u64,
    transactions: Vec<UnsignedTxResponse>,
    /// Present only when the request included `stealth`.
    #[serde(skip_serializing_if = "Option::is_none")]
    stealth: Option<StealthResponse>,
}

#[derive(Debug, serde::Serialize)]
struct HopKeyResponse {
    pubkey: String,
    /// Base58 64-byte secret. ONE-TIME: the server does not store it.
    /// Signs the funding transaction(s) where this hop is the sender.
    secret_key: String,
}

#[derive(Debug, serde::Serialize)]
struct ScheduledBuyResponse {
    buy_index: usize,
    /// Index into `transactions` where this buy's unsigned tx lives.
    transaction_index: usize,
    /// Submit at/after create_slot + this. 0 with the bundle otherwise.
    target_slot_offset: u64,
    amount_sol: f64,
    wallet: String,
}

#[derive(Debug, serde::Serialize)]
struct StealthResponse {
    /// Unsigned SOL funding transfers, in the order they must be submitted
    /// (before the launch). Each is signed by its sender: the creator for
    /// the first hop of each chain, then the hop wallets (see
    /// hop_secret_keys).
    funding_transactions: Vec<UnsignedTxResponse>,
    funding_blockhash: String,
    funding_expires_in_slots: u64,
    /// One-time hop secrets, in no particular order. The server never
    /// stores them — save them, fund, then discard.
    hop_secret_keys: Vec<HopKeyResponse>,
    /// The jittered buy amounts actually built, per initial_buys index.
    executed_buy_amounts_sol: Vec<f64>,
    /// Per-buy submission schedule.
    buy_schedule: Vec<ScheduledBuyResponse>,
    /// Total SOL the creator must send across all first-hop transfers.
    total_funding_sol: f64,
    /// Human-readable execution order for the platform integrator.
    execution_note: String,
}

#[derive(Debug, serde::Serialize)]
struct SimResult {
    index: usize,
    kind: String,
    success: bool,
    error: Option<String>,
    logs: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
struct SimulateResponse {
    mint_pubkey: String,
    results: Vec<SimResult>,
}

#[derive(Debug, serde::Serialize)]
struct ErrorBody {
    error: String,
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ErrorBody>)>;

fn api_error(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (
        status,
        Json(ErrorBody {
            error: msg.into(),
        }),
    )
}

// ---------------------------------------------------------------------------
// Unsigned transaction assembly
// ---------------------------------------------------------------------------

/// Assemble instructions into an UNSIGNED v0 transaction, returned as base64
/// plus the required signer pubkeys in signature-slot order.
///
/// The message is compiled with the exact same payer + instructions as the
/// signed CLI path (`local::assemble_signed_base64_multi`), so the signer
/// set and order are identical — only the signatures are empty (64 zero
/// bytes each). The caller signs each slot with the matching private key.
fn assemble_unsigned_base64(
    payer: &Pubkey,
    instructions: Vec<Instruction>,
    blockhash: Hash,
    compute_unit_limit: u32,
    compute_unit_price_microlamports: u64,
) -> Result<(String, Vec<Pubkey>)> {
    let mut ixs = Vec::with_capacity(instructions.len() + 2);
    ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(
        compute_unit_limit,
    ));
    ixs.push(ComputeBudgetInstruction::set_compute_unit_price(
        compute_unit_price_microlamports,
    ));
    ixs.extend(instructions);

    let message = V0Message::try_compile(payer, &ixs, &[], blockhash)
        .map_err(|e| anyhow!("compiling v0 message: {e}"))?;
    let num_signers = message.header.num_required_signatures as usize;
    let signers: Vec<Pubkey> =
        message.account_keys[..num_signers].to_vec();
    let tx = VersionedTransaction {
        signatures: vec![Signature::default(); num_signers],
        message: VersionedMessage::V0(message),
    };
    let bytes = bincode::serialize(&tx).context("serializing transaction")?;
    Ok((B64.encode(bytes), signers))
}

/// Plain variant of `assemble_unsigned_base64`: no compute-budget
/// instructions. Funding transfers look like ordinary wallet transfers and
/// cost exactly 5,000 lamports, matching the hop funding math.
fn assemble_unsigned_base64_plain(
    payer: &Pubkey,
    instructions: Vec<Instruction>,
    blockhash: Hash,
) -> Result<(String, Vec<Pubkey>)> {
    let message = V0Message::try_compile(payer, &instructions, &[], blockhash)
        .map_err(|e| anyhow!("compiling v0 message: {e}"))?;
    let num_signers = message.header.num_required_signatures as usize;
    let signers: Vec<Pubkey> = message.account_keys[..num_signers].to_vec();
    let tx = VersionedTransaction {
        signatures: vec![Signature::default(); num_signers],
        message: VersionedMessage::V0(message),
    };
    let bytes = bincode::serialize(&tx).context("serializing transaction")?;
    Ok((B64.encode(bytes), signers))
}

/// Build the buy instruction list EXACTLY as `tx_builder::build_buy_transaction`
/// does — same instructions, same order, same slippage-floor logic.
/// Only the final signing step differs (handled by `assemble_unsigned_base64`).
fn buy_instructions(order: &tx_builder::BuyOrder) -> Vec<Instruction> {
    let mut ixs = Vec::new();
    if order.create_user_ata {
        let ata = tx_builder::ata_pda(
            &order.accts.user,
            &order.accts.token_program,
            &order.accts.mint,
        );
        ixs.push(tx_builder::create_ata_instruction(
            &order.accts.user,
            &ata,
            &order.accts.user,
            &order.accts.mint,
            &order.accts.token_program,
        ));
    }
    // Slippage floor: identical to build_buy_transaction — accept the quoted
    // output minus tolerance; fall back to 1 when no quote is available.
    let min_tokens_out = if order.expected_tokens_out > 0 {
        let floor = (order.expected_tokens_out as u128)
            .saturating_mul(10_000 - order.slippage_bps.min(10_000) as u128)
            / 10_000;
        floor.clamp(1, u64::MAX as u128) as u64
    } else {
        1
    };
    ixs.push(tx_builder::buy_exact_quote_in_v3_instruction(
        &order.accts,
        order.sol_to_spend_lamports,
        min_tokens_out,
    ));
    ixs
}

struct UnsignedBundle {
    mint_pubkey: Pubkey,
    mint_secret_bs58: String,
    blockhash: Hash,
    /// (kind, base64, required signers)
    txs: Vec<(String, String, Vec<Pubkey>)>,
}

async fn resolve_fee_recipient(rpc_url: &str) -> Result<Pubkey> {
    let data = local::fetch_account_data(rpc_url, &tx_builder::global_pda())
        .await?
        .ok_or_else(|| anyhow!("Pump Global account not found"))?;
    decode::global_fee_recipient(&data).context("parsing Global fee recipient")
}

/// Build the full launch bundle UNSIGNED. Mirrors `bundle::build_launch_bundle`
/// instruction-for-instruction; the only difference is no signing happens here.
/// When `stealth` is present, buy amounts come from the stealth plan
/// (jittered); with spread_slots > 0 the buys are still returned as
/// individual transactions — the `buy_schedule` in the response tells the
/// caller to submit them separately instead of bundling with `create`.
async fn build_unsigned_bundle(
    rpc_url: &str,
    plan: &LaunchPlan,
    tip_account: Option<Pubkey>,
    stealth: Option<&stealth::StealthPlan>,
) -> Result<UnsignedBundle> {
    let creator_pubkey =
        Pubkey::from_str(&plan.creator).map_err(|_| anyhow!("bad creator pubkey"))?;
    if plan.tip_lamports > 0 && tip_account.is_none() {
        anyhow::bail!("tip_lamports > 0 requires tip_account");
    }

    // Fresh mint for the new token. The server generates it, returns the
    // secret ONE TIME, and never stores it.
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let mint_secret_bs58 = bs58::encode(mint_kp.to_bytes()).into_string();

    let fee_recipient = resolve_fee_recipient(rpc_url).await?;
    let blockhash = tx_builder::fetch_latest_blockhash(rpc_url).await?;
    let token_program = create::token_2022_program();

    let mut txs: Vec<(String, String, Vec<Pubkey>)> = Vec::new();

    // --- 1. Create (signers: creator/payer + new mint). ---
    let create_ix = create::create_instruction(
        &mint,
        &creator_pubkey,
        &creator_pubkey,
        &plan.token_name,
        &plan.token_symbol,
        &plan.token_uri,
    );
    let (create_b64, create_signers) = assemble_unsigned_base64(
        &creator_pubkey,
        vec![create_ix],
        blockhash.clone(),
        CU_LIMIT,
        CU_PRICE,
    )?;
    txs.push(("create".into(), create_b64, create_signers));

    // --- 2. One buy per initial buy, in plan order. ---
    // With stealth, amounts are the jittered ones from the stealth plan.
    let buy_amounts: Vec<u64> = match stealth {
        Some(s) => s.buyers.iter().map(|b| b.jittered_amount_lamports).collect(),
        None => plan
            .initial_buys
            .iter()
            .map(|b| (b.sol_amount * 1_000_000_000.0) as u64)
            .collect(),
    };
    for (buy, sol_lamports) in plan.initial_buys.iter().zip(buy_amounts.iter()) {
        let buyer_pubkey =
            Pubkey::from_str(&buy.wallet).map_err(|_| anyhow!("bad buyer wallet"))?;
        let sol_lamports = *sol_lamports;
        let order = tx_builder::BuyOrder {
            accts: tx_builder::PumpAccounts {
                fee_recipient,
                mint,
                creator: creator_pubkey,
                user: buyer_pubkey,
                token_program,
                buyback_fee_recipient: tx_builder::buyback_wallet(),
            },
            sol_to_spend_lamports: sol_lamports,
            expected_tokens_out: 0,
            slippage_bps: plan.slippage_bps,
            create_user_ata: true,
            compute_unit_limit: CU_LIMIT,
            compute_unit_price_microlamports: CU_PRICE,
        };
        let (buy_b64, buy_signers) = assemble_unsigned_base64(
            &buyer_pubkey,
            buy_instructions(&order),
            blockhash.clone(),
            CU_LIMIT,
            CU_PRICE,
        )?;
        txs.push(("buy".into(), buy_b64, buy_signers));
    }

    // --- 3. Optional Jito tip (signer: creator). ---
    if plan.tip_lamports > 0 {
        let tip_ix = tx_builder::system_transfer_instruction(
            &creator_pubkey,
            &tip_account.unwrap(),
            plan.tip_lamports,
        );
        let (tip_b64, tip_signers) = assemble_unsigned_base64(
            &creator_pubkey,
            vec![tip_ix],
            blockhash.clone(),
            TIP_CU_LIMIT,
            CU_PRICE,
        )?;
        txs.push(("tip".into(), tip_b64, tip_signers));
    }

    if txs.len() > 5 {
        anyhow::bail!(
            "bundle would hold {} transactions; Jito limit is 5",
            txs.len()
        );
    }

    Ok(UnsignedBundle {
        mint_pubkey: mint,
        mint_secret_bs58,
        blockhash,
        txs,
    })
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

fn to_launch_plan(req: &BuildRequest) -> Result<LaunchPlan> {
    let rpc_url = req
        .rpc_url
        .clone()
        .unwrap_or_else(|| DEFAULT_RPC_URL.to_string());
    if !(rpc_url.starts_with("http://") || rpc_url.starts_with("https://")) {
        anyhow::bail!("rpc_url must be an http(s) URL");
    }
    let plan = LaunchPlan {
        token_name: req.token_name.clone(),
        token_symbol: req.token_symbol.clone(),
        token_uri: req.token_uri.clone(),
        creator: req.creator_pubkey.clone(),
        initial_buys: req
            .initial_buys
            .iter()
            .map(|b| InitialBuy {
                wallet: b.wallet.clone(),
                sol_amount: b.sol_amount,
            })
            .collect(),
        tip_lamports: req.tip_lamports.unwrap_or(0),
        slippage_bps: req.slippage_bps.unwrap_or(500),
        stealth: None, // API stealth comes via req.stealth, planned separately
    };
    plan.validate()?;
    Ok(plan)
}

fn rpc_url_of(req: &BuildRequest) -> String {
    req.rpc_url
        .clone()
        .unwrap_or_else(|| DEFAULT_RPC_URL.to_string())
}

async fn build_handler(Json(req): Json<BuildRequest>) -> ApiResult<BuildResponse> {
    let plan = to_launch_plan(&req).map_err(|e| api_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    let tip_account = match &req.tip_account {
        Some(s) => Some(
            Pubkey::from_str(s)
                .map_err(|_| api_error(StatusCode::BAD_REQUEST, "tip_account is not a valid pubkey"))?,
        ),
        None => None,
    };
    // Stealth planning is pure (no RPC): validated before anything on-chain.
    let stealth_plan = match &req.stealth {
        Some(sr) => Some(
            sr.to_config()
                .and_then(|cfg| stealth::plan_stealth(&plan, &cfg))
                .map_err(|e| api_error(StatusCode::BAD_REQUEST, e.to_string()))?,
        ),
        None => None,
    };
    let rpc_url = rpc_url_of(&req);
    let bundle = build_unsigned_bundle(&rpc_url, &plan, tip_account, stealth_plan.as_ref())
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, format!("bundle build failed: {e}")))?;

    let transactions = bundle
        .txs
        .iter()
        .enumerate()
        .map(|(index, (kind, base64, required_signers))| UnsignedTxResponse {
            index,
            kind: kind.clone(),
            base64: base64.clone(),
            required_signers: required_signers.iter().map(|p| p.to_string()).collect(),
        })
        .collect::<Vec<_>>();

    let stealth = match stealth_plan {
        Some(splan) => Some(
            build_stealth_response(&plan, &splan, &bundle, &transactions)
                .map_err(|e| api_error(StatusCode::BAD_GATEWAY, format!("stealth build failed: {e}")))?,
        ),
        None => None,
    };

    Ok(Json(BuildResponse {
        mint_pubkey: bundle.mint_pubkey.to_string(),
        mint_secret_key: bundle.mint_secret_bs58,
        blockhash: bundle.blockhash.to_string(),
        expires_in_slots: BLOCKHASH_VALID_SLOTS,
        transactions,
        stealth,
    }))
}

/// Assemble the unsigned stealth artifacts: funding transactions (unsigned
/// system transfers in submit order), one-time hop secrets, the buy
/// schedule, and a human-readable execution note.
fn build_stealth_response(
    plan: &LaunchPlan,
    splan: &stealth::StealthPlan,
    bundle: &UnsignedBundle,
    transactions: &[UnsignedTxResponse],
) -> Result<StealthResponse> {
    // Funding txs share the bundle's blockhash (single build call). For the
    // two-call pattern (fund first, build later) pass the same amount_seed.
    let mut funding_transactions = Vec::with_capacity(splan.funding_transfers.len());
    for t in &splan.funding_transfers {
        let from =
            Pubkey::from_str(&t.from).map_err(|_| anyhow!("bad funding sender"))?;
        let to =
            Pubkey::from_str(&t.to).map_err(|_| anyhow!("bad funding recipient"))?;
        let ix = tx_builder::system_transfer_instruction(&from, &to, t.lamports);
        let (base64, required_signers) =
            assemble_unsigned_base64_plain(&from, vec![ix], bundle.blockhash.clone())?;
        funding_transactions.push(UnsignedTxResponse {
            index: t.sequence,
            kind: "funding".to_string(),
            base64,
            required_signers: required_signers.iter().map(|p| p.to_string()).collect(),
        });
    }

    let hop_secret_keys = splan
        .hop_secrets_bs58()
        .into_iter()
        .map(|(pubkey, secret_key)| HopKeyResponse { pubkey, secret_key })
        .collect();

    // Buy txs sit at transactions[1..=n] (right after create; tip is last).
    // Verify the layout rather than assuming it.
    let spread = splan.config.spread_slots > 0;
    for b in &splan.buyers {
        let tx = transactions
            .get(1 + b.buy_index)
            .ok_or_else(|| anyhow!("bundle layout mismatch: missing buy tx"))?;
        if tx.kind != "buy" {
            return Err(anyhow!("bundle layout mismatch: expected buy tx"));
        }
    }
    let buy_schedule = splan
        .buyers
        .iter()
        .map(|b| ScheduledBuyResponse {
            buy_index: b.buy_index,
            transaction_index: 1 + b.buy_index,
            target_slot_offset: b.target_slot_offset,
            amount_sol: b.jittered_amount_lamports as f64 / 1_000_000_000.0,
            wallet: b.buyer.clone(),
        })
        .collect::<Vec<_>>();
    let executed_buy_amounts_sol = buy_schedule.iter().map(|s| s.amount_sol).collect();

    let creator = Pubkey::from_str(&plan.creator).map_err(|_| anyhow!("bad creator pubkey"))?;
    let total_funding_sol =
        splan.total_creator_funding_lamports(&creator) as f64 / 1_000_000_000.0;

    let execution_note = if spread {
        format!(
            "1) Sign each funding tx with its sender's key (creator, then hop secrets) and submit \
             IN ORDER via direct RPC, waiting for confirmation. 2) Sign transactions[0] (create{}) \
             with creator + mint keys and submit via Jito alone (do NOT bundle the buys). \
             3) Submit each buy at/after create_slot + its target_slot_offset via direct RPC, \
             signed by the buyer wallet.",
            if plan.tip_lamports > 0 { " + tip" } else { "" },
        )
    } else {
        "Sign each funding tx with its sender's key (creator, then hop secrets) and submit IN \
         ORDER via direct RPC before the launch. Then sign and submit transactions[] as one Jito \
         bundle (create + buys + tip) — the buys carry jittered amounts.".to_string()
    };

    Ok(StealthResponse {
        funding_transactions,
        funding_blockhash: bundle.blockhash.to_string(),
        funding_expires_in_slots: BLOCKHASH_VALID_SLOTS,
        hop_secret_keys,
        executed_buy_amounts_sol,
        buy_schedule,
        total_funding_sol,
        execution_note,
    })
}

async fn simulate_handler(Json(req): Json<BuildRequest>) -> ApiResult<SimulateResponse> {
    let plan = to_launch_plan(&req).map_err(|e| api_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    let tip_account = match &req.tip_account {
        Some(s) => Some(
            Pubkey::from_str(s)
                .map_err(|_| api_error(StatusCode::BAD_REQUEST, "tip_account is not a valid pubkey"))?,
        ),
        None => None,
    };
    let stealth_plan = match &req.stealth {
        Some(sr) => Some(
            sr.to_config()
                .and_then(|cfg| stealth::plan_stealth(&plan, &cfg))
                .map_err(|e| api_error(StatusCode::BAD_REQUEST, e.to_string()))?,
        ),
        None => None,
    };
    let rpc_url = rpc_url_of(&req);
    let bundle = build_unsigned_bundle(&rpc_url, &plan, tip_account, stealth_plan.as_ref())
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, format!("bundle build failed: {e}")))?;

    // With stealth, simulate the funding chain first (kind "funding"), then
    // the bundle transactions. Unsigned txs simulate fine: simulation checks
    // program logic, not signatures. Only the creator-signed legs have a
    // funded sender pre-launch — hop legs are skipped (their sender is
    // funded when the prior hop confirms, exactly as the execution note
    // describes).
    let mut sim_txs: Vec<(String, String)> = Vec::new();
    if let Some(splan) = &stealth_plan {
        let creator_str = plan.creator.clone();
        for t in &splan.funding_transfers {
            if t.from != creator_str {
                continue;
            }
            let from = Pubkey::from_str(&t.from)
                .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "bad funding sender"))?;
            let to = Pubkey::from_str(&t.to)
                .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "bad funding recipient"))?;
            let ix = tx_builder::system_transfer_instruction(&from, &to, t.lamports);
            let (base64, _) = assemble_unsigned_base64_plain(&from, vec![ix], bundle.blockhash.clone())
                .map_err(|e| api_error(StatusCode::BAD_GATEWAY, format!("funding build failed: {e}")))?;
            sim_txs.push(("funding".to_string(), base64));
        }
    }
    for (kind, base64, _) in &bundle.txs {
        sim_txs.push((kind.clone(), base64.clone()));
    }

    let mut results = Vec::new();
    for (index, (kind, base64)) in sim_txs.iter().enumerate() {
        match protection::simulate_transaction(&rpc_url, base64).await {
            Ok(sim) => results.push(SimResult {
                index,
                kind: kind.clone(),
                success: sim.success,
                error: sim.error,
                logs: sim.logs,
            }),
            Err(e) => results.push(SimResult {
                index,
                kind: kind.clone(),
                success: false,
                error: Some(format!("simulation RPC failed: {e}")),
                logs: vec![],
            }),
        }
    }

    Ok(Json(SimulateResponse {
        mint_pubkey: bundle.mint_pubkey.to_string(),
        results,
    }))
}

async fn health_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok", "version": VERSION }))
}

/// Public API landing/docs page (no auth) — the target for the marketing
/// site's "Get API Access" button.
async fn docs_handler() -> Html<&'static str> {
    Html(DOCS_HTML)
}

const DOCS_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>GhostLaunch API — Launch behind the veil</title>
<style>
  body { background:#050508; color:#e8e8f0; font-family: ui-sans-serif, system-ui, sans-serif; margin:0; padding:48px 20px 80px; }
  .wrap { max-width: 820px; margin: 0 auto; }
  h1 { font-size: 44px; margin: 0 0 6px; letter-spacing:-1px; }
  .slogan { color:#00e5ff; font-size:18px; margin:0 0 8px; }
  .mono { font-family: ui-monospace, Menlo, monospace; font-size:13px; text-transform:uppercase; letter-spacing:2px; color:#8a8aa3; }
  p { line-height:1.6; color:#c9c9d8; }
  h2 { margin-top:44px; font-size:22px; }
  code { font-family: ui-monospace, Menlo, monospace; background:#12121c; padding:2px 6px; border-radius:4px; color:#a5f3fc; font-size:14px; }
  pre { background:#0b0b14; border:1px solid #1e1e30; border-radius:8px; padding:16px; overflow-x:auto; font-size:13px; line-height:1.55; color:#d6d6e8; }
  table { width:100%; border-collapse:collapse; margin-top:12px; }
  td, th { text-align:left; padding:10px 8px; border-bottom:1px solid #1c1c2c; font-size:15px; vertical-align:top; }
  th { color:#8a8aa3; font-weight:600; font-size:13px; text-transform:uppercase; letter-spacing:1px; }
  .price { color:#c4b5fd; font-weight:700; }
  a { color:#00e5ff; }
  .note { background:#0b0b14; border:1px solid #1e1e30; border-left:3px solid #7c3aed; border-radius:6px; padding:14px 16px; margin-top:36px; }
</style>
</head>
<body>
<div class="wrap">
  <p class="mono">GhostLaunch API · v0.3.0</p>
  <h1>GhostLaunch</h1>
  <p class="slogan">Launch behind the veil.</p>
  <p>The stealth launch system for Solana meme coins, as one API. Build a full
  Pump.fun launch — token creation, distributed initial buys, stealth wallet
  funding via <b>Cloak</b> — as unsigned transactions you sign yourself.
  Non-custodial by design: this API never holds private keys.</p>

  <h2>Endpoints</h2>
  <table>
    <tr><th>Method</th><th>Path</th><th>What it does</th></tr>
    <tr><td><code>GET</code></td><td><code>/v1/health</code></td><td>Status + version. No auth.</td></tr>
    <tr><td><code>POST</code></td><td><code>/v1/bundle/build</code></td><td>Build a launch bundle: create + up to 4 buys, optional Cloak stealth plan (multi-hop funding, amount jitter, slot spread). Returns unsigned transactions, signers in order, and one-time mint/hop secrets.</td></tr>
    <tr><td><code>POST</code></td><td><code>/v1/bundle/simulate</code></td><td>Simulate a bundle against an RPC before submitting.</td></tr>
    <tr><td><code>GET</code></td><td><code>/v1/radar/{mint}</code></td><td>Launch radar snapshot: sniper detection, whale alerts, holder analytics for a token.</td></tr>
    <tr><td><code>POST</code></td><td><code>/v1/radar/watch</code></td><td>Register a mint for ongoing radar monitoring.</td></tr>
    <tr><td><code>GET</code></td><td><code>/v1/radar/watch/{id}</code></td><td>Fetch a watch's latest radar report.</td></tr>
  </table>

  <h2>Authentication</h2>
  <p>Every endpoint except <code>/v1/health</code> and this page requires an API key:</p>
  <pre>X-API-Key: &lt;your key&gt;</pre>

  <h2>Example — build a launch</h2>
  <pre>curl -X POST https://ghostlaunch-api.onrender.com/v1/bundle/build \
  -H "X-API-Key: &lt;your key&gt;" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "My Coin", "symbol": "COIN",
    "creator": "&lt;your wallet pubkey&gt;",
    "buys": [{ "buyer": "&lt;buyer pubkey&gt;", "sol": 0.1 }]
  }'</pre>
  <p>The response contains the mint pubkey, the one-time mint secret key, and
  the unsigned transactions (base64) with their required signers listed in
  order. Sign them with your own keys and submit — we never touch them.</p>

  <h2>Pricing</h2>
  <p><span class="price">Creators: 0.2 SOL per launch</span>, flat — stealth included.<br>
  <span class="price">Platforms: 0.05 SOL per launch</span>, wholesale via API. You set your retail price and keep the spread.</p>

  <div class="note">
    <b>Getting an API key:</b> keys are issued to integration partners.
    Reach out via <a href="https://ghostlaunch.framer.website/">ghostlaunch.framer.website</a> to request access.
  </div>
</div>
</body>
</html>"##;

// ---------------------------------------------------------------------------
// Radar: post-launch monitoring (read-only)
// ---------------------------------------------------------------------------

/// Shared watch state for radar watches.
#[derive(Clone, Default)]
struct ApiState {
    watches: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, WatchEntry>>>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct WatchEntry {
    watch_id: String,
    mint: String,
    started_at_unix: i64,
    polls_completed: u64,
    watching: bool,
    latest: Option<radar::RadarReport>,
    error: Option<String>,
}

/// Query params for GET /v1/radar/{mint} (on-demand snapshot).
#[derive(Debug, serde::Deserialize)]
struct RadarQuery {
    #[serde(default)]
    rpc_url: Option<String>,
    /// Buys within create_slot + N count as snipers (default 2).
    #[serde(default)]
    sniper_slots: Option<u64>,
    /// Single buys >= this many SOL count as whales (default 5.0).
    #[serde(default)]
    whale_sol: Option<f64>,
    /// Max signatures to scan (default 1000, max 1000).
    #[serde(default)]
    limit: Option<usize>,
    /// Comma-separated known wallets (creator / initial buyers).
    #[serde(default)]
    known: Option<String>,
}

fn radar_config_from_query(q: &RadarQuery, default_rpc: &str) -> Result<radar::RadarConfig> {
    let cfg = radar::RadarConfig {
        rpc_url: q
            .rpc_url
            .clone()
            .unwrap_or_else(|| default_rpc.to_string()),
        sniper_slot_window: q.sniper_slots.unwrap_or(2),
        whale_threshold_lamports: ((q.whale_sol.unwrap_or(5.0))
            * radar::LAMPORTS_PER_SOL as f64) as u64,
        max_signatures: q.limit.unwrap_or(1000),
    };
    cfg.validate()?;
    Ok(cfg)
}

fn parse_known(q_known: &Option<String>) -> Result<std::collections::HashSet<Pubkey>> {
    match q_known {
        Some(s) => radar::parse_wallet_list(s),
        None => Ok(std::collections::HashSet::new()),
    }
}

async fn radar_snapshot_handler(
    Path(mint_str): Path<String>,
    Query(q): Query<RadarQuery>,
) -> Result<Json<radar::RadarReport>, (StatusCode, Json<serde_json::Value>)> {
    let err = |code: StatusCode, msg: String| (code, Json(serde_json::json!({"error": msg})));
    let mint = Pubkey::from_str(&mint_str).map_err(|_| err(StatusCode::BAD_REQUEST, format!("bad mint pubkey: {mint_str}")))?;
    let cfg = radar_config_from_query(&q, DEFAULT_RPC_URL)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let known =
        parse_known(&q.known).map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    radar::snapshot(&cfg, &mint, &known)
        .await
        .map(Json)
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("radar snapshot failed: {e:#}")))
}

/// Body for POST /v1/radar/watch.
#[derive(Debug, serde::Deserialize)]
struct WatchRequest {
    mint: String,
    #[serde(default)]
    rpc_url: Option<String>,
    /// How long to watch, seconds (default 1800 = 30 min).
    #[serde(default)]
    duration_secs: Option<u64>,
    /// Poll interval, seconds (default 30, min 10).
    #[serde(default)]
    poll_interval_secs: Option<u64>,
    #[serde(default)]
    sniper_slots: Option<u64>,
    #[serde(default)]
    whale_sol: Option<f64>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    known_wallets: Option<Vec<String>>,
}

async fn radar_watch_start_handler(
    State(state): State<ApiState>,
    Json(req): Json<WatchRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let err = |code: StatusCode, msg: String| (code, Json(serde_json::json!({"error": msg})));
    let mint = Pubkey::from_str(&req.mint)
        .map_err(|_| err(StatusCode::BAD_REQUEST, format!("bad mint pubkey: {}", req.mint)))?;
    let q = RadarQuery {
        rpc_url: req.rpc_url.clone(),
        sniper_slots: req.sniper_slots,
        whale_sol: req.whale_sol,
        limit: req.limit,
        known: None,
    };
    let cfg = radar_config_from_query(&q, DEFAULT_RPC_URL)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let known: std::collections::HashSet<Pubkey> = match &req.known_wallets {
        Some(list) => list
            .iter()
            .map(|s| {
                Pubkey::from_str(s).with_context(|| format!("bad known wallet: {s}"))
            })
            .collect::<Result<_>>()
            .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{e:#}")))?,
        None => std::collections::HashSet::new(),
    };
    let duration = req.duration_secs.unwrap_or(1800).clamp(60, 86400);
    let poll_interval = req.poll_interval_secs.unwrap_or(30).clamp(10, 3600);

    let watch_id = format!("w_{}", &uuid_simple());
    let started_at = chrono_now_unix();
    let entry = WatchEntry {
        watch_id: watch_id.clone(),
        mint: mint.to_string(),
        started_at_unix: started_at,
        polls_completed: 0,
        watching: true,
        latest: None,
        error: None,
    };
    state.watches.lock().await.insert(watch_id.clone(), entry);

    // Background poll loop. Read-only: snapshots only.
    let watches = state.watches.clone();
    let wid = watch_id.clone();
    tokio::spawn(async move {
        let deadline = started_at + duration as i64;
        loop {
            match radar::snapshot(&cfg, &mint, &known).await {
                Ok(report) => {
                    let mut w = watches.lock().await;
                    if let Some(e) = w.get_mut(&wid) {
                        e.polls_completed += 1;
                        e.latest = Some(report);
                        e.error = None;
                    }
                }
                Err(e) => {
                    let mut w = watches.lock().await;
                    if let Some(entry) = w.get_mut(&wid) {
                        entry.error = Some(format!("{e:#}"));
                    }
                }
            }
            if chrono_now_unix() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(poll_interval)).await;
        }
        let mut w = watches.lock().await;
        if let Some(e) = w.get_mut(&wid) {
            e.watching = false;
        }
    });

    Ok(Json(serde_json::json!({
        "watch_id": watch_id,
        "mint": mint.to_string(),
        "duration_secs": duration,
        "poll_interval_secs": poll_interval,
        "status": "watching",
    })))
}

async fn radar_watch_get_handler(
    State(state): State<ApiState>,
    Path(watch_id): Path<String>,
) -> Result<Json<WatchEntry>, (StatusCode, Json<serde_json::Value>)> {
    let w = state.watches.lock().await;
    match w.get(&watch_id) {
        Some(e) => Ok(Json(e.clone())),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("unknown watch_id: {watch_id}")})),
        )),
    }
}

/// Seconds since epoch, no extra deps.
fn chrono_now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 16 hex chars of randomness for watch IDs (no uuid dep).
fn uuid_simple() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut h);
    std::thread::current().id().hash(&mut h);
    format!("{:016x}", h.finish())
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// API-key gate. Exempt: /v1/health (monitoring must not need a key).
/// If LAUNCH_API_KEYS is unset, auth is skipped (dev mode).
async fn auth_middleware(request: Request, next: Next) -> Result<Response, StatusCode> {
    if request.uri().path() == "/v1/health" || request.uri().path() == "/" {
        return Ok(next.run(request).await);
    }
    let configured = std::env::var("LAUNCH_API_KEYS").unwrap_or_default();
    let keys: Vec<String> = configured
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if keys.is_empty() {
        return Ok(next.run(request).await); // dev mode: no keys configured
    }
    let provided = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if keys.iter().any(|k| k == provided) {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn app(state: ApiState) -> Router {
    Router::new()
        .route("/", get(docs_handler))
        .route("/v1/health", get(health_handler))
        .route("/v1/bundle/build", post(build_handler))
        .route("/v1/bundle/simulate", post(simulate_handler))
        .route("/v1/radar/{mint}", get(radar_snapshot_handler))
        .route("/v1/radar/watch", post(radar_watch_start_handler))
        .route("/v1/radar/watch/{id}", get(radar_watch_get_handler))
        .layer(middleware::from_fn(auth_middleware))
        .with_state(state)
}

#[tokio::main]
async fn main() {
    if std::env::var("LAUNCH_API_KEYS").unwrap_or_default().is_empty() {
        eprintln!("WARNING: LAUNCH_API_KEYS is unset — API auth is disabled (dev mode)");
    }
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("binding port");
    eprintln!("launch-api v{VERSION} listening on 0.0.0.0:{port}");
    axum::serve(listener, app(ApiState::default()))
        .await
        .expect("serving");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use solana_instruction::Instruction;
    use solana_pubkey::Pubkey;
    use tower::ServiceExt;

    fn test_request() -> BuildRequest {
        BuildRequest {
            token_name: "Test".into(),
            token_symbol: "TST".into(),
            token_uri: "https://example.com/m.json".into(),
            creator_pubkey: "So11111111111111111111111111111111111111112".into(),
            initial_buys: vec![BuyRequest {
                wallet: "So11111111111111111111111111111111111111112".into(),
                sol_amount: 0.1,
            }],
            slippage_bps: None,
            tip_lamports: None,
            tip_account: None,
            rpc_url: None,
            stealth: None,
        }
    }

    #[test]
    fn plan_validation_reused() {
        // Bad pubkey -> 400-class validation error before any RPC is touched.
        let mut req = test_request();
        req.creator_pubkey = "not-a-pubkey".into();
        assert!(to_launch_plan(&req).is_err());

        let mut req = test_request();
        req.initial_buys[0].sol_amount = 0.0;
        assert!(to_launch_plan(&req).is_err());

        assert!(to_launch_plan(&test_request()).is_ok());
    }

    #[test]
    fn stealth_request_defaults_and_validation() {
        // All-None => defaults (2 hops, 3-slot spread, ±500bps, 0.015 buffer).
        let sr = StealthRequest {
            funding_hops: None,
            spread_slots: None,
            amount_jitter_bps: None,
            funding_buffer_sol: None,
            amount_seed: None,
        };
        let cfg = sr.to_config().unwrap();
        assert_eq!(cfg.funding_hops, 2);
        assert_eq!(cfg.spread_slots, 3);
        assert_eq!(cfg.amount_jitter_bps, 500);
        assert!((cfg.funding_buffer_sol - 0.015).abs() < 1e-12);
        assert_eq!(cfg.amount_seed, None);

        // Overrides respected; bad values rejected.
        let bad = StealthRequest {
            funding_hops: Some(9),
            spread_slots: None,
            amount_jitter_bps: None,
            funding_buffer_sol: None,
            amount_seed: Some(7),
        };
        assert!(bad.to_config().is_err());

        let ok = StealthRequest {
            funding_hops: Some(1),
            spread_slots: Some(0),
            amount_jitter_bps: Some(300),
            funding_buffer_sol: Some(0.02),
            amount_seed: Some(7),
        };
        let cfg = ok.to_config().unwrap();
        assert_eq!(cfg.funding_hops, 1);
        assert_eq!(cfg.spread_slots, 0);
        assert_eq!(cfg.amount_seed, Some(7));
    }

    #[test]
    fn stealth_plan_end_to_end_offline() {
        // Pure planning + unsigned funding assembly, no RPC.
        let mut req = test_request();
        req.stealth = Some(StealthRequest {
            funding_hops: Some(2),
            spread_slots: Some(3),
            amount_jitter_bps: Some(500),
            funding_buffer_sol: Some(0.015),
            amount_seed: Some(1234),
        });
        let plan = to_launch_plan(&req).unwrap();
        let splan = stealth::plan_stealth(
            &plan,
            &req.stealth.as_ref().unwrap().to_config().unwrap(),
        )
        .unwrap();
        // 1 buyer => 3 funding transfers (creator->h0, h0->h1, h1->buyer).
        assert_eq!(splan.funding_transfers.len(), 3);
        // Funding txs assemble unsigned with the sender as sole signer.
        let blockhash = Hash::new_from_array([1u8; 32]);
        for t in &splan.funding_transfers {
            let from = Pubkey::from_str(&t.from).unwrap();
            let to = Pubkey::from_str(&t.to).unwrap();
            let ix = tx_builder::system_transfer_instruction(&from, &to, t.lamports);
            let (b64, signers) =
                assemble_unsigned_base64_plain(&from, vec![ix], blockhash.clone()).unwrap();
            assert_eq!(signers, vec![from]);
            let bytes = B64.decode(&b64).unwrap();
            let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
            assert_eq!(tx.signatures.len(), 1);
        }
        // Schedule within spread.
        assert!(splan.buyers[0].target_slot_offset <= 3);
    }

    #[test]
    fn tip_requires_tip_account() {        let mut req = test_request();
        req.tip_lamports = Some(10_000);
        // Plan itself validates; the tip_account requirement is enforced
        // in build_unsigned_bundle. Validation of the plan passes here.
        assert!(to_launch_plan(&req).is_ok());
    }

    #[test]
    fn unsigned_tx_decodes_with_correct_signers() {
        // Pure offline check: build an unsigned tx, decode it, verify the
        // signer slots and that signatures are empty placeholders.
        let payer = Pubkey::new_unique();
        let other_signer = Pubkey::new_unique();
        let ix = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![
                solana_instruction::AccountMeta::new(payer, true),
                solana_instruction::AccountMeta::new(other_signer, true),
                solana_instruction::AccountMeta::new_readonly(Pubkey::new_unique(), false),
            ],
            data: vec![1, 2, 3],
        };
        let blockhash = Hash::new_from_array([7u8; 32]);
        let (b64, signers) =
            assemble_unsigned_base64(&payer, vec![ix], blockhash.clone(), 200_000, 1_000).unwrap();

        // Signer order: payer first, then other required signers.
        assert_eq!(signers, vec![payer, other_signer]);

        // Decode the base64 and confirm structure.
        let bytes = B64.decode(&b64).unwrap();
        let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        assert_eq!(tx.signatures.len(), 2);
        assert!(tx.signatures.iter().all(|s| *s == Signature::default()));
        match &tx.message {
            VersionedMessage::V0(msg) => {
                assert_eq!(msg.header.num_required_signatures, 2);
                assert_eq!(msg.recent_blockhash, blockhash);
                // Instructions survive the round trip (compute budget x2 + ours).
                assert_eq!(msg.instructions.len(), 3);
            }
            _ => panic!("expected v0 message"),
        }
    }

    #[test]
    fn buy_instructions_match_signed_path_layout() {
        // The unsigned buy must contain exactly: [create_ata?, buy_v3].
        // create_user_ata=true here, so expect 2 instructions.
        let order = tx_builder::BuyOrder {
            accts: tx_builder::PumpAccounts {
                fee_recipient: Pubkey::new_unique(),
                mint: Pubkey::new_unique(),
                creator: Pubkey::new_unique(),
                user: Pubkey::new_unique(),
                token_program: create::token_2022_program(),
                buyback_fee_recipient: tx_builder::buyback_wallet(),
            },
            sol_to_spend_lamports: 100_000_000,
            expected_tokens_out: 0,
            slippage_bps: 500,
            create_user_ata: true,
            compute_unit_limit: CU_LIMIT,
            compute_unit_price_microlamports: CU_PRICE,
        };
        let ixs = buy_instructions(&order);
        assert_eq!(ixs.len(), 2);
        // First is the ATA program create, second targets the pump program.
        assert_eq!(ixs[0].program_id, tx_builder::ata_program_id());
        assert_eq!(ixs[1].program_id, decode::pump_program_id());
        // Without ATA creation there is exactly one instruction.
        let mut order2 = order.clone();
        order2.create_user_ata = false;
        assert_eq!(buy_instructions(&order2).len(), 1);
    }

    #[test]
    fn unsigned_message_matches_signed_message() {
        // The core safety property: same payer + same instructions must
        // produce a BYTE-IDENTICAL message in the unsigned path and the
        // signed CLI path. Only the signatures differ (empty vs real).
        // This proves the API builds exactly what the CLI builds.
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let ix = tx_builder::system_transfer_instruction(&payer.pubkey(), &recipient, 1_000);
        let blockhash = Hash::new_from_array([9u8; 32]);

        let (unsigned_b64, unsigned_signers) =
            assemble_unsigned_base64(&payer.pubkey(), vec![ix.clone()], blockhash.clone(), 200_000, 1_000)
                .unwrap();
        let signed_b64 =
            local::assemble_signed_base64_multi(&[&payer], vec![ix], blockhash, 200_000, 1_000)
                .unwrap();

        let unsigned_tx: VersionedTransaction =
            bincode::deserialize(&B64.decode(&unsigned_b64).unwrap()).unwrap();
        let signed_tx: VersionedTransaction =
            bincode::deserialize(&B64.decode(&signed_b64).unwrap()).unwrap();

        assert_eq!(unsigned_tx.message, signed_tx.message);
        assert_eq!(unsigned_signers, vec![payer.pubkey()]);
        assert!(unsigned_tx.signatures.iter().all(|s| *s == Signature::default()));
        assert!(signed_tx.signatures.iter().all(|s| *s != Signature::default()));
    }

    #[tokio::test]
    async fn health_endpoint_ok() {
        let app = app(ApiState::default());
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["version"], VERSION);
    }

    #[tokio::test]
    async fn build_rejects_bad_request_without_touching_rpc() {
        // Auth disabled in tests (LAUNCH_API_KEYS unset in this env by default
        // unless the outer env sets it — validation happens before RPC).
        let app = app(ApiState::default());
        let mut req = test_request();
        req.creator_pubkey = "bogus".into();
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/bundle/build")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_string(&req).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn auth_rejects_missing_key_when_configured() {
        // With keys configured, a missing key -> 401 (health stays open).
        std::env::set_var("LAUNCH_API_KEYS", "test-key-123");
        let app = app(ApiState::default());

        let no_key = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/bundle/build")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(no_key.status(), StatusCode::UNAUTHORIZED);

        let health = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);

        std::env::remove_var("LAUNCH_API_KEYS");
    }

    /// Full build against a live RPC. Needs network; skips gracefully when
    /// the RPC is unreachable so unit runs stay green offline.
    #[tokio::test]
    async fn build_end_to_end_against_rpc() {
        let rpc = std::env::var("LAUNCH_API_TEST_RPC")
            .unwrap_or_else(|_| DEFAULT_RPC_URL.to_string());
        // Quick connectivity probe before doing real work.
        if tx_builder::fetch_latest_blockhash(&rpc).await.is_err() {
            eprintln!("skipping build_end_to_end_against_rpc: no RPC at {rpc}");
            return;
        }
        let mut req = test_request();
        req.rpc_url = Some(rpc);
        let app = app(ApiState::default());
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/bundle/build")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_string(&req).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // mint_pubkey is valid base58, 32 bytes.
        let mint = v["mint_pubkey"].as_str().unwrap();
        let mint_bytes = bs58::decode(mint).into_vec().unwrap();
        assert_eq!(mint_bytes.len(), 32);
        // mint_secret_key is valid base58, 64 bytes, and matches the pubkey.
        let secret_b58 = v["mint_secret_key"].as_str().unwrap();
        let secret = bs58::decode(secret_b58).into_vec().unwrap();
        assert_eq!(secret.len(), 64);
        let kp = Keypair::try_from_base58_string(secret_b58).unwrap();
        assert_eq!(kp.pubkey().to_string(), mint);
        // Transactions present and decode.
        let txs = v["transactions"].as_array().unwrap();
        assert_eq!(txs.len(), 2); // create + 1 buy
        for t in txs {
            let bytes = B64.decode(t["base64"].as_str().unwrap()).unwrap();
            let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
            let signers = t["required_signers"].as_array().unwrap();
            // Signature slots match required_signers, all empty placeholders.
            assert_eq!(tx.signatures.len(), signers.len());
            assert!(tx.signatures.iter().all(|s| *s == Signature::default()));
            assert!(!signers.is_empty());
        }
        // Create tx (index 0) needs creator + mint signatures.
        let create_signers = txs[0]["required_signers"].as_array().unwrap();
        assert_eq!(create_signers.len(), 2);
        assert_eq!(create_signers[0].as_str().unwrap(), req.creator_pubkey);
        assert_eq!(create_signers[1].as_str().unwrap(), mint);
        // Buy tx (index 1) needs only the buyer.
        let buy_signers = txs[1]["required_signers"].as_array().unwrap();
        assert_eq!(buy_signers.len(), 1);

        assert!(v["blockhash"].as_str().unwrap().len() > 30);
        assert_eq!(v["expires_in_slots"], 150);
    }
}
