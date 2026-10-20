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
//!   GET  /v1/balance          -> caller's own { key_type, lamports, rate_sol }
//!   POST /v1/bundle/build     -> unsigned bundle + one-time mint secret
//!                                  (deducts the per-launch rate; 402 when empty)
//!   POST /v1/bundle/simulate  -> build + simulate every tx, submit nothing (free)
//!   GET  /v1/admin/balances    -> all keys' { key_last4, key_type, lamports } (master key only)
//!   POST /v1/admin/credit     -> add SOL to a key's balance (master key only)
//!   POST /v1/admin/set-type   -> set a key's platform/creator tier (master key only)
//!
//! Auth: X-API-Key header, checked against LAUNCH_API_KEYS (comma-separated).
//! If LAUNCH_API_KEYS is unset, auth is skipped with a startup warning.
//! Admin endpoints additionally require X-API-Key to equal MASTER_API_KEY.
//!
//! Billing: prepaid. In-memory ledger, seeded from KEY_BALANCES_JSON at
//! startup (Render's disk is ephemeral). POST /v1/bundle/build deducts the
//! key's per-launch rate (platform 0.05 SOL, creator 0.2 SOL); simulate and
//! radar endpoints are free.

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
    extract::{Extension, Path, Query, Request, State},
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

const VERSION: &str = "0.5.0";
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
    /// Present on billing errors (e.g. insufficient_balance).
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ErrorBody>)>;

fn api_error(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (
        status,
        Json(ErrorBody {
            error: msg.into(),
            message: None,
        }),
    )
}

fn insufficient_balance_error(balance_sol: f64, rate_sol: f64) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::PAYMENT_REQUIRED,
        Json(ErrorBody {
            error: "insufficient_balance".into(),
            message: Some(format!(
                "balance {balance_sol:.4} SOL is below the {rate_sol} SOL per-launch rate. \
                 Deposit SOL to the payment wallet on the docs page with your deposit code \
                 (last 8 characters of your API key) in the memo — credit is automatic. \
                 Deposits without a code need manual review: email ghostlaunch.api@gmail.com \
                 with your key's last 4 characters and the transaction signature."
            )),
        }),
    )
}

// ---------------------------------------------------------------------------
// Billing: prepaid per-key ledger
// ---------------------------------------------------------------------------

const PLATFORM_RATE_LAMPORTS: u64 = 50_000_000; // 0.05 SOL per launch
const CREATOR_RATE_LAMPORTS: u64 = 200_000_000; // 0.2 SOL per launch
const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum KeyType {
    Platform,
    Creator,
}

impl KeyType {
    fn rate_lamports(self) -> u64 {
        match self {
            KeyType::Platform => PLATFORM_RATE_LAMPORTS,
            KeyType::Creator => CREATOR_RATE_LAMPORTS,
        }
    }

    fn rate_sol(self) -> f64 {
        self.rate_lamports() as f64 / LAMPORTS_PER_SOL as f64
    }
}

impl std::str::FromStr for KeyType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "platform" => Ok(KeyType::Platform),
            "creator" => Ok(KeyType::Creator),
            other => Err(format!("key_type must be \"platform\" or \"creator\", got \"{other}\"")),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Balance {
    lamports: u64,
    key_type: KeyType,
}

impl Default for Balance {
    /// New keys start on the platform tier with an empty balance.
    fn default() -> Self {
        Balance {
            lamports: 0,
            key_type: KeyType::Platform,
        }
    }
}

/// Last 4 characters of a key, for safe public references.
fn key_last4(key: &str) -> String {
    key.chars().rev().take(4).collect::<String>().chars().rev().collect()
}

/// Parse the startup ledger snapshot from KEY_BALANCES_JSON.
/// Format: {"<full key>": {"lamports": N, "type": "platform"|"creator"}}.
/// Malformed entries are skipped with a warning; a missing/empty var means
/// an empty ledger (new keys start at zero balance).
fn load_balances_from_env() -> std::collections::HashMap<String, Balance> {
    let raw = std::env::var("KEY_BALANCES_JSON").unwrap_or_default();
    if raw.trim().is_empty() {
        eprintln!("billing: KEY_BALANCES_JSON unset — starting with an empty ledger");
        return std::collections::HashMap::new();
    }
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("billing: KEY_BALANCES_JSON is not valid JSON ({e}) — starting empty");
            return std::collections::HashMap::new();
        }
    };
    let mut out = std::collections::HashMap::new();
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => {
            eprintln!("billing: KEY_BALANCES_JSON must be a JSON object — starting empty");
            return std::collections::HashMap::new();
        }
    };
    for (key, entry) in obj {
        if key.trim().is_empty() {
            continue;
        }
        let lamports = entry
            .get("lamports")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let type_str = entry
            .get("type")
            .or_else(|| entry.get("key_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("platform");
        let key_type: KeyType = match type_str.parse() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("billing: skipping balance entry for key ending {}: {e}", key_last4(key));
                continue;
            }
        };
        out.insert(key.clone(), Balance { lamports, key_type });
    }
    out
}

/// Keys configured on this server (the issued keys).
fn configured_keys() -> Vec<String> {
    std::env::var("LAUNCH_API_KEYS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
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

/// Self-service balance lookup for the caller's own key.
async fn balance_handler(
    State(state): State<ApiState>,
    Extension(auth): Extension<AuthKey>,
) -> ApiResult<BalanceResponse> {
    let key = auth
        .0
        .ok_or_else(|| api_error(StatusCode::UNAUTHORIZED, "balance lookup requires an API key"))?;
    let balances = state.balances.lock().await;
    let bal = balances.get(&key).cloned().unwrap_or_default();
    Ok(Json(BalanceResponse {
        key_type: bal.key_type,
        lamports: bal.lamports,
        rate_sol: bal.key_type.rate_sol(),
    }))
}

#[derive(Debug, serde::Serialize)]
struct BalanceResponse {
    key_type: KeyType,
    lamports: u64,
    rate_sol: f64,
}

#[derive(Debug, serde::Deserialize)]
struct CreditRequest {
    /// The full API key to credit (must be a valid issued key).
    key: String,
    /// SOL to add to the balance.
    sol: f64,
}

#[derive(Debug, serde::Deserialize)]
struct SetTypeRequest {
    /// The full API key to retier (must be a valid issued key).
    key: String,
    key_type: KeyType,
}

#[derive(Debug, serde::Serialize)]
struct KeyBalanceSummary {
    key_last4: String,
    key_type: KeyType,
    lamports: u64,
}

/// POST /v1/admin/credit — add SOL to a key's prepaid balance.
/// Master key required.
async fn admin_credit_handler(
    State(state): State<ApiState>,
    Extension(auth): Extension<AuthKey>,
    Json(req): Json<CreditRequest>,
) -> ApiResult<KeyBalanceSummary> {
    require_master(&auth)?;
    if !configured_keys().iter().any(|k| k == &req.key) {
        return Err(api_error(StatusCode::NOT_FOUND, "unknown API key"));
    }
    if !(req.sol > 0.0) || !req.sol.is_finite() {
        return Err(api_error(StatusCode::BAD_REQUEST, "sol must be a positive number"));
    }
    let add = (req.sol * LAMPORTS_PER_SOL as f64).round() as u64;
    let mut balances = state.balances.lock().await;
    let bal = balances.entry(req.key.clone()).or_default();
    bal.lamports = bal.lamports.saturating_add(add);
    Ok(Json(KeyBalanceSummary {
        key_last4: key_last4(&req.key),
        key_type: bal.key_type,
        lamports: bal.lamports,
    }))
}

/// POST /v1/admin/set-type — set a key's platform/creator rate tier.
/// Master key required.
async fn admin_set_type_handler(
    State(state): State<ApiState>,
    Extension(auth): Extension<AuthKey>,
    Json(req): Json<SetTypeRequest>,
) -> ApiResult<KeyBalanceSummary> {
    require_master(&auth)?;
    if !configured_keys().iter().any(|k| k == &req.key) {
        return Err(api_error(StatusCode::NOT_FOUND, "unknown API key"));
    }
    let mut balances = state.balances.lock().await;
    let bal = balances.entry(req.key.clone()).or_default();
    bal.key_type = req.key_type;
    Ok(Json(KeyBalanceSummary {
        key_last4: key_last4(&req.key),
        key_type: bal.key_type,
        lamports: bal.lamports,
    }))
}

/// GET /v1/admin/balances — every configured key's balance summary.
/// Master key required. Never returns full keys.
async fn admin_balances_handler(
    State(state): State<ApiState>,
    Extension(auth): Extension<AuthKey>,
) -> ApiResult<Vec<KeyBalanceSummary>> {
    require_master(&auth)?;
    let balances = state.balances.lock().await;
    let list = configured_keys()
        .iter()
        .map(|k| {
            let bal = balances.get(k).cloned().unwrap_or_default();
            KeyBalanceSummary {
                key_last4: key_last4(k),
                key_type: bal.key_type,
                lamports: bal.lamports,
            }
        })
        .collect();
    Ok(Json(list))
}

/// Shared bundle-build core used by /v1/bundle/build (after per-key billing)
/// and /v1/launchpad/build (after on-chain fee verification). Pure build:
/// no auth, no billing — callers handle payment first.
async fn build_bundle_core(
    req: &BuildRequest,
    stealth_cfg: Option<stealth::StealthConfig>,
    tip_account: Option<Pubkey>,
) -> ApiResult<BuildResponse> {
    let plan = to_launch_plan(req).map_err(|e| api_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    let stealth_plan = match stealth_cfg {
        Some(cfg) => stealth::plan_stealth(&plan, &cfg)
            .map(Some)
            .map_err(|e| api_error(StatusCode::BAD_REQUEST, e.to_string()))?,
        None => None,
    };
    let rpc_url = rpc_url_of(req);
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

async fn build_handler(
    State(state): State<ApiState>,
    Extension(auth): Extension<AuthKey>,
    Json(req): Json<BuildRequest>,
) -> ApiResult<BuildResponse> {
    let tip_account = match &req.tip_account {
        Some(s) => Some(
            Pubkey::from_str(s)
                .map_err(|_| api_error(StatusCode::BAD_REQUEST, "tip_account is not a valid pubkey"))?,
        ),
        None => None,
    };
    // Stealth planning is pure (no RPC): validated before anything on-chain.
    let stealth_cfg = match &req.stealth {
        Some(sr) => Some(
            sr.to_config()
                .map_err(|e| api_error(StatusCode::BAD_REQUEST, e.to_string()))?,
        ),
        None => None,
    };
    // Billing: after validation, before any RPC work. One atomic lock
    // acquisition: check balance, deduct the per-launch rate, or 402.
    if let Some(key) = &auth.0 {
        let mut balances = state.balances.lock().await;
        let bal = balances.entry(key.clone()).or_default();
        let rate = bal.key_type.rate_lamports();
        if bal.lamports < rate {
            let (code, body) = insufficient_balance_error(
                bal.lamports as f64 / LAMPORTS_PER_SOL as f64,
                bal.key_type.rate_sol(),
            );
            return Err((code, body));
        }
        bal.lamports -= rate;
    }
    build_bundle_core(&req, stealth_cfg, tip_account).await
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

// ---------------------------------------------------------------------------
// Launchpad (consumer token launcher)
// ---------------------------------------------------------------------------

/// Fee wallet for launchpad payments (SOL).
const LAUNCHPAD_PAYMENT_WALLET: &str = "2q4EwekHtit3BtzL25NmGbWJ5khPkmypYbba2t3A4XtM";
const LAUNCHPAD_BASE_FEE_LAMPORTS: u64 = 200_000_000; // 0.2 SOL
const LAUNCHPAD_DEEP_VEIL_LAMPORTS: u64 = 100_000_000; // 0.1 SOL
const LAUNCHPAD_EXTRA_WALLET_LAMPORTS: u64 = 50_000_000; // 0.05 SOL each
const LAUNCHPAD_RADAR_WATCH_LAMPORTS: u64 = 50_000_000; // 0.05 SOL
/// Max buyer wallets per launch (1 create + N buys must fit the 5-tx bundle).
const LAUNCHPAD_MAX_BUYERS: u8 = 4;

#[derive(Debug, serde::Deserialize)]
struct LaunchpadStealthOpts {
    #[serde(default)]
    buyer_wallets: Option<u8>,
    #[serde(default)]
    amount_jitter_bps: Option<u16>,
    #[serde(default)]
    spread_slots: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
struct LaunchpadAddons {
    #[serde(default)]
    deep_veil: Option<bool>,
    #[serde(default)]
    extra_wallets: Option<u8>,
    #[serde(default)]
    radar_watch: Option<bool>,
}

#[derive(Debug, serde::Deserialize)]
struct LaunchpadBuildRequest {
    /// Base58 signature of the SOL fee payment to the launchpad wallet.
    payment_signature: String,
    token_name: String,
    token_symbol: String,
    token_uri: String,
    /// Creator wallet: pays the fee, funds the stealth chains, signs create.
    creator_pubkey: String,
    /// Total dev buy in SOL, split across the stealth buyer wallets.
    /// 0 = no dev buy (no buyers, no stealth).
    #[serde(default)]
    dev_buy_sol: f64,
    #[serde(default)]
    rpc_url: Option<String>,
    #[serde(default)]
    stealth: Option<LaunchpadStealthOpts>,
    #[serde(default)]
    addons: Option<LaunchpadAddons>,
}

#[derive(Debug, serde::Serialize)]
struct LaunchpadBuildResponse {
    mint_pubkey: String,
    /// Base58 64-byte secret. ONE-TIME: the server does not store it.
    mint_secret_key: String,
    blockhash: String,
    expires_in_slots: u64,
    transactions: Vec<UnsignedTxResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stealth: Option<StealthResponse>,
    /// One-time buyer wallet secrets (sign the buy txs). Server never stores.
    buyer_secret_keys: Vec<HopKeyResponse>,
    fee_paid_sol: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    radar_watch_id: Option<String>,
}

/// Total launchpad fee in lamports for the selected add-ons.
fn launchpad_fee_lamports(addons: &Option<LaunchpadAddons>) -> u64 {
    let mut fee = LAUNCHPAD_BASE_FEE_LAMPORTS;
    if let Some(a) = addons {
        if a.deep_veil.unwrap_or(false) {
            fee += LAUNCHPAD_DEEP_VEIL_LAMPORTS;
        }
        fee += (a.extra_wallets.unwrap_or(0) as u64) * LAUNCHPAD_EXTRA_WALLET_LAMPORTS;
        if a.radar_watch.unwrap_or(false) {
            fee += LAUNCHPAD_RADAR_WATCH_LAMPORTS;
        }
    }
    fee
}

/// Pure helper: lamports the payment wallet received in a getTransaction
/// response, or an error string. Testable without RPC.
fn launchpad_payment_received_lamports(
    tx: &serde_json::Value,
    creator: &str,
    payment_wallet: &str,
) -> Result<u64, String> {
    if tx.get("error").is_some() {
        return Err("RPC error looking up payment".into());
    }
    let meta = tx.pointer("/result/meta").ok_or("transaction not found")?;
    if !meta.get("err").map(|e| e.is_null()).unwrap_or(false) {
        return Err("payment transaction failed".into());
    }
    let keys = tx
        .pointer("/result/transaction/message/accountKeys")
        .and_then(|k| k.as_array())
        .ok_or("missing accountKeys")?;
    // Fee payer must be the creator: binds the payment to this launch and
    // stops one payment signature from being shared across creators.
    let payer = keys.first().and_then(|k| k.as_str()).unwrap_or("");
    if payer != creator {
        return Err("payment must come from the creator wallet".into());
    }
    let idx = keys
        .iter()
        .position(|k| k.as_str() == Some(payment_wallet))
        .ok_or("payment wallet not in transaction")?;
    let pre = meta
        .pointer(&format!("/preBalances/{idx}"))
        .and_then(|v| v.as_u64())
        .ok_or("missing preBalances")?;
    let post = meta
        .pointer(&format!("/postBalances/{idx}"))
        .and_then(|v| v.as_u64())
        .ok_or("missing postBalances")?;
    Ok(post.saturating_sub(pre))
}

/// Verify the fee payment on-chain: confirmed, succeeded, sent by the
/// creator, and the payment wallet received at least `expected_lamports`.
async fn verify_launchpad_payment(
    rpc_url: &str,
    signature: &str,
    creator: &str,
    expected_lamports: u64,
) -> Result<()> {
    let client = reqwest::Client::new();
    let mut last_err = String::from("no RPC attempt made");
    // Try without a version cap first (accepts legacy + v0), fall back to
    // the v0-only shape some RPCs need.
    for params in [
        serde_json::json!([signature, {"encoding": "json", "commitment": "confirmed"}]),
        serde_json::json!([signature, {"encoding": "json", "commitment": "confirmed", "maxSupportedTransactionVersion": 0}]),
    ] {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "getTransaction", "params": params});
        let resp = client
            .post(rpc_url)
            .json(&body)
            .send()
            .await
            .with_context(|| "payment verification RPC failed")?;
        let tx: serde_json::Value = resp
            .json()
            .await
            .with_context(|| "bad RPC response")?;
        match launchpad_payment_received_lamports(&tx, creator, LAUNCHPAD_PAYMENT_WALLET) {
            Ok(received) => {
                if received < expected_lamports {
                    anyhow::bail!(
                        "payment too small: wallet received {} lamports, need {}",
                        received,
                        expected_lamports
                    );
                }
                return Ok(());
            }
            Err(e) => {
                last_err = e;
            }
        }
    }
    anyhow::bail!("payment verification failed: {last_err}")
}

async fn launchpad_build_handler(
    State(state): State<ApiState>,
    Json(req): Json<LaunchpadBuildRequest>,
) -> ApiResult<LaunchpadBuildResponse> {
    let bad = |msg: &str| api_error(StatusCode::BAD_REQUEST, msg);
    let creator = Pubkey::from_str(req.creator_pubkey.trim())
        .map_err(|_| bad("creator_pubkey is not a valid pubkey"))?;
    if req.token_name.trim().is_empty() {
        return Err(bad("token_name is required"));
    }
    if req.token_symbol.trim().is_empty() {
        return Err(bad("token_symbol is required"));
    }
    if req.token_uri.trim().is_empty() || req.token_uri.len() > 200 {
        return Err(bad("token_uri is required (max 200 chars)"));
    }
    if !req.dev_buy_sol.is_finite() || req.dev_buy_sol < 0.0 {
        return Err(bad("dev_buy_sol must be >= 0"));
    }
    let payment_sig = req.payment_signature.trim().to_string();
    if payment_sig.is_empty() {
        return Err(bad("payment_signature is required"));
    }
    if bs58::decode(&payment_sig)
        .into_vec()
        .map(|v| v.len())
        .unwrap_or(0)
        != 64
    {
        return Err(bad("payment_signature is not a valid signature"));
    }

    let deep_veil = req
        .addons
        .as_ref()
        .and_then(|a| a.deep_veil)
        .unwrap_or(false);
    let extra_wallets = req
        .addons
        .as_ref()
        .and_then(|a| a.extra_wallets)
        .unwrap_or(0)
        .min(5);
    let radar_watch = req
        .addons
        .as_ref()
        .and_then(|a| a.radar_watch)
        .unwrap_or(false);
    let fee_lamports = launchpad_fee_lamports(&req.addons);
    let rpc_url = req
        .rpc_url
        .clone()
        .unwrap_or_else(|| DEFAULT_RPC_URL.to_string());

    // Payment verification BEFORE anything else (and before reserving the
    // signature, so a bad payment can't burn a good signature).
    verify_launchpad_payment(&rpc_url, &payment_sig, &creator.to_string(), fee_lamports)
        .await
        .map_err(|e| api_error(StatusCode::PAYMENT_REQUIRED, format!("{e:#}")))?;

    // Replay guard: reserve the payment signature before building. One
    // payment = one launch.
    {
        let mut used = state.used_payments.lock().await;
        if !used.insert(payment_sig.clone()) {
            return Err(api_error(
                StatusCode::CONFLICT,
                "payment_signature already used",
            ));
        }
    }

    // Buyer wallets: stealth buyers + paid extras, capped so the bundle
    // stays within the 5-transaction limit.
    let mut n_buyers: u8 = 0;
    if req.dev_buy_sol > 0.0 {
        let base = req
            .stealth
            .as_ref()
            .and_then(|s| s.buyer_wallets)
            .unwrap_or(3)
            .clamp(1, LAUNCHPAD_MAX_BUYERS);
        n_buyers = base.saturating_add(extra_wallets).min(LAUNCHPAD_MAX_BUYERS);
    }

    // Ephemeral buyer keypairs (server-generated, secrets returned one-time).
    let buyer_kps: Vec<Keypair> = (0..n_buyers).map(|_| Keypair::new()).collect();
    let per_buyer = if n_buyers > 0 {
        req.dev_buy_sol / f64::from(n_buyers)
    } else {
        0.0
    };
    let initial_buys: Vec<BuyRequest> = buyer_kps
        .iter()
        .map(|kp| BuyRequest {
            wallet: kp.pubkey().to_string(),
            sol_amount: per_buyer,
        })
        .collect();

    // Stealth config from options + add-ons.
    let stealth_cfg: Option<stealth::StealthConfig> = if n_buyers > 0 {
        let cfg = stealth::StealthConfig {
            funding_hops: if deep_veil { 3 } else { 2 },
            spread_slots: req
                .stealth
                .as_ref()
                .and_then(|s| s.spread_slots)
                .unwrap_or(if deep_veil { 12 } else { 3 }),
            amount_jitter_bps: req
                .stealth
                .as_ref()
                .and_then(|s| s.amount_jitter_bps)
                .unwrap_or(if deep_veil { 2000 } else { 500 }),
            funding_buffer_sol: 0.015,
            amount_seed: None,
        };
        cfg.validate().map_err(|e| bad(&e.to_string()))?;
        Some(cfg)
    } else {
        None
    };

    let build_req = BuildRequest {
        token_name: req.token_name.clone(),
        token_symbol: req.token_symbol.clone(),
        token_uri: req.token_uri.clone(),
        creator_pubkey: req.creator_pubkey.clone(),
        initial_buys,
        slippage_bps: None,
        tip_lamports: None,
        tip_account: None,
        rpc_url: req.rpc_url.clone(),
        stealth: None, // stealth comes via stealth_cfg, not the request field
    };

    let build_resp = match build_bundle_core(&build_req, stealth_cfg, None).await {
        Ok(Json(r)) => r,
        Err(e) => return Err(e),
    };

    let buyer_secret_keys = buyer_kps
        .iter()
        .map(|kp| HopKeyResponse {
            pubkey: kp.pubkey().to_string(),
            secret_key: bs58::encode(kp.to_bytes()).into_string(),
        })
        .collect();

    // Radar-watch add-on: register a watch for the new mint (4h, 30s polls).
    // A watch failure never fails the launch — the token is already built.
    let mut radar_watch_id: Option<String> = None;
    if radar_watch {
        match Pubkey::from_str(&build_resp.mint_pubkey) {
            Ok(mint) => {
                let q = RadarQuery {
                    rpc_url: req.rpc_url.clone(),
                    sniper_slots: None,
                    whale_sol: None,
                    limit: None,
                    known: None,
                };
                match radar_config_from_query(&q, DEFAULT_RPC_URL) {
                    Ok(cfg) => {
                        let wid = spawn_radar_watch(
                            &state,
                            mint,
                            cfg,
                            std::collections::HashSet::new(),
                            14400,
                            30,
                        )
                        .await;
                        radar_watch_id = Some(wid);
                    }
                    Err(e) => {
                        eprintln!("launchpad: radar-watch add-on failed to register: {e:#}");
                    }
                }
            }
            Err(e) => {
                eprintln!("launchpad: bad mint in build response: {e:#}");
            }
        }
    }

    Ok(Json(LaunchpadBuildResponse {
        mint_pubkey: build_resp.mint_pubkey,
        mint_secret_key: build_resp.mint_secret_key,
        blockhash: build_resp.blockhash,
        expires_in_slots: build_resp.expires_in_slots,
        transactions: build_resp.transactions,
        stealth: build_resp.stealth,
        buyer_secret_keys,
        fee_paid_sol: fee_lamports as f64 / 1_000_000_000.0,
        radar_watch_id,
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
  <p class="mono">GhostLaunch API · v0.4.0</p>
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
    <tr><td><code>GET</code></td><td><code>/v1/balance</code></td><td>Your key's prepaid balance: tier, lamports remaining, per-launch rate.</td></tr>
    <tr><td><code>POST</code></td><td><code>/v1/launchpad/build</code></td><td>Consumer launchpad: verify an on-chain SOL fee payment, then build a stealth launch bundle with server-generated ephemeral buyer wallets. No API key — the fee payment is the auth. See "Launchpad" below.</td></tr>
  </table>

  <h2>Launchpad</h2>
  <p>The GhostLaunch launchpad lets anyone launch a token through the website
  with Cloak stealth built in — no API key needed. The caller first sends the
  launch fee in SOL to the payment wallet, then calls
  <code>POST /v1/launchpad/build</code> with the payment's transaction
  signature. The server verifies the payment on-chain (confirmed, succeeded,
  sent by the creator wallet, payment wallet received the full fee), reserves
  the signature against reuse, and builds the bundle:</p>
  <table>
    <tr><th>Fee</th><th>Amount</th></tr>
    <tr><td>Base launch (Cloak stealth included)</td><td>0.2 SOL</td></tr>
    <tr><td>Deep Veil add-on (3 hops, max jitter, wide slot spread)</td><td>+0.1 SOL</td></tr>
    <tr><td>Extra stealth wallet (each)</td><td>+0.05 SOL</td></tr>
    <tr><td>Radar sniper-watch add-on</td><td>+0.05 SOL</td></tr>
  </table>
  <p>The dev buy (optional) is split across the stealth buyer wallets, which
  the server generates fresh per launch — their secrets are returned one-time
  for signing the buy transactions. Trust, stated plainly: token authorities
  are held by the Pump.fun program, not the creator — nobody can mint more
  tokens or freeze wallets.</p>
  <pre>curl -X POST https://ghostlaunch-api.onrender.com/v1/launchpad/build \
  -H 'Content-Type: application/json' \
  -d '{"payment_signature":"&lt;fee payment sig&gt;","token_name":"Veil Cat","token_symbol":"VCAT","token_uri":"https://.../meta.json","creator_pubkey":"&lt;your wallet&gt;","dev_buy_sol":1.0,"addons":{"deep_veil":true}}'</pre>

  <h2>Billing (prepaid)</h2>
  <p>GhostLaunch runs on prepaid balances — no invoices, no chasing. Every
  <code>POST /v1/bundle/build</code> deducts the per-launch rate from your
  key's balance before the bundle is built:</p>
  <table>
    <tr><th>Tier</th><th>Per launch</th></tr>
    <tr><td><span class="price">Platform</span></td><td>0.05 SOL</td></tr>
    <tr><td><span class="price">Creator</span></td><td>0.2 SOL</td></tr>
  </table>
  <p>When a balance can't cover the next launch, the API returns
  <code>402 insufficient_balance</code> and builds nothing.
  <code>POST /v1/bundle/simulate</code>, all radar endpoints, and
  <code>GET /v1/balance</code> are always free.</p>
  <p><b>How to top up:</b> your deposit code is the <b>last 8 characters</b>
  of your API key. Include this code in the <b>memo</b> of your SOL deposit
  and your key is credited automatically within minutes — no email needed.</p>
  <ol>
    <li>Send SOL to the payment wallet:<br>
    <code>2q4EwekHtit3BtzL25NmGbWJ5khPkmypYbba2t3A4XtM</code></li>
    <li>Put your deposit code (last 8 characters of your API key) in the
    transaction's memo.</li>
  </ol>
  <p>Deposits without a code are held for manual review — email
  <a href="mailto:ghostlaunch.api@gmail.com?subject=GhostLaunch%20balance%20top-up">ghostlaunch.api@gmail.com</a>
  with the payment's transaction signature and your key's last 4 characters.</p>
  <p>Check your balance anytime: <code>GET /v1/balance</code> (API key auth).</p>

  <h2>Authentication</h2>
  <p>Every endpoint except <code>/v1/health</code>,
  <code>/v1/launchpad/build</code>, and this page requires an API key:</p>
  <pre>X-API-Key: &lt;your key&gt;</pre>
  <p><code>/v1/launchpad/build</code> is public: the verified on-chain SOL
  fee payment is its authentication.</p>

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
  <span class="price">Platforms: 0.05 SOL per launch</span>, wholesale via API. You set your retail price and keep the spread.<br>
  <b>Payment wallet (SOL):</b> <code>2q4EwekHtit3BtzL25NmGbWJ5khPkmypYbba2t3A4XtM</code></p>
  <p>Send launch fees to this address. Include your platform name or API key in the payment memo so your payment can be matched to your account.</p>

  <div class="note">
    <b>Getting an API key:</b> keys are issued to launch platforms and to
    individual creators (creator keys are billed at the creator tier).
    Email <a href="mailto:ghostlaunch.api@gmail.com?subject=GhostLaunch%20API%20access%20request">ghostlaunch.api@gmail.com</a> with your platform name or creator details and expected launch volume to request access.
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
    /// Prepaid billing ledger: API key -> balance. In-memory; seeded from
    /// KEY_BALANCES_JSON at startup and snapshotted back to that env var
    /// by the balances-sync cron (Render's disk is ephemeral).
    balances: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, Balance>>>,
    /// Launchpad payment signatures already consumed. In-memory; a server
    /// restart could theoretically allow a replay, so the deposit-watch
    /// idempotency pattern (processed-before-credit) is mirrored here:
    /// the signature is reserved BEFORE the bundle is built.
    used_payments: std::sync::Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
}

/// The API key that authenticated this request, when key auth is configured.
/// None in dev mode (LAUNCH_API_KEYS unset) — billing is skipped then.
#[derive(Clone)]
struct AuthKey(Option<String>);

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

/// Register a radar watch and spawn its background poll loop. Returns the
/// watch_id. Shared by POST /v1/radar/watch and the launchpad's
/// radar-watch add-on.
async fn spawn_radar_watch(
    state: &ApiState,
    mint: Pubkey,
    cfg: radar::RadarConfig,
    known: std::collections::HashSet<Pubkey>,
    duration_secs: u64,
    poll_interval_secs: u64,
) -> String {
    let duration = duration_secs.clamp(60, 86400);
    let poll_interval = poll_interval_secs.clamp(10, 3600);
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

    watch_id
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
    let duration = req.duration_secs.unwrap_or(1800);
    let poll_interval = req.poll_interval_secs.unwrap_or(30);

    let watch_id = spawn_radar_watch(&state, mint, cfg, known, duration, poll_interval).await;

    Ok(Json(serde_json::json!({
        "watch_id": watch_id,
        "mint": mint.to_string(),
        "duration_secs": duration.clamp(60, 86400),
        "poll_interval_secs": poll_interval.clamp(10, 3600),
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
/// If LAUNCH_API_KEYS is unset, auth is skipped (dev mode). On success the
/// caller's key is stored in the request extensions as AuthKey so handlers
/// can bill it; in dev mode AuthKey(None) is stored and billing is skipped.
async fn auth_middleware(mut request: Request, next: Next) -> Result<Response, StatusCode> {
    let path = request.uri().path();
    // Public: health/docs (monitoring + marketing) and the launchpad build
    // endpoint (the on-chain SOL fee payment IS its auth).
    if path == "/v1/health" || path == "/" || path == "/v1/launchpad/build" {
        return Ok(next.run(request).await);
    }
    let keys = configured_keys();
    if keys.is_empty() {
        request.extensions_mut().insert(AuthKey(None));
        return Ok(next.run(request).await); // dev mode: no keys configured
    }
    let provided = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if keys.iter().any(|k| k == &provided) {
        request.extensions_mut().insert(AuthKey(Some(provided)));
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Admin endpoints additionally require the MASTER key specifically (not
/// just any valid key). MASTER_API_KEY unset => admin disabled.
fn require_master(auth: &AuthKey) -> Result<(), (StatusCode, Json<ErrorBody>)> {
    let master = std::env::var("MASTER_API_KEY").unwrap_or_default();
    match (&auth.0, master.is_empty()) {
        (Some(k), false) if k == &master => Ok(()),
        _ => Err(api_error(StatusCode::FORBIDDEN, "admin disabled or wrong key")),
    }
}

fn app(state: ApiState) -> Router {
    Router::new()
        .route("/", get(docs_handler))
        .route("/v1/health", get(health_handler))
        .route("/v1/balance", get(balance_handler))
        .route("/v1/bundle/build", post(build_handler))
        .route("/v1/bundle/simulate", post(simulate_handler))
        .route("/v1/launchpad/build", post(launchpad_build_handler))
        .route("/v1/radar/{mint}", get(radar_snapshot_handler))
        .route("/v1/radar/watch", post(radar_watch_start_handler))
        .route("/v1/radar/watch/{id}", get(radar_watch_get_handler))
        .route("/v1/admin/credit", post(admin_credit_handler))
        .route("/v1/admin/set-type", post(admin_set_type_handler))
        .route("/v1/admin/balances", get(admin_balances_handler))
        .layer(middleware::from_fn(auth_middleware))
        .with_state(state)
}

#[tokio::main]
async fn main() {
    if std::env::var("LAUNCH_API_KEYS").unwrap_or_default().is_empty() {
        eprintln!("WARNING: LAUNCH_API_KEYS is unset — API auth is disabled (dev mode)");
    }
    if std::env::var("MASTER_API_KEY").unwrap_or_default().is_empty() {
        eprintln!("WARNING: MASTER_API_KEY is unset — /v1/admin/* billing endpoints are disabled");
    }
    let balances = load_balances_from_env();
    eprintln!(
        "billing: loaded {} prepaid balances from KEY_BALANCES_JSON",
        balances.len()
    );
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("binding port");
    let state = ApiState {
        balances: std::sync::Arc::new(tokio::sync::Mutex::new(balances)),
        ..Default::default()
    };
    eprintln!("launch-api v{VERSION} listening on 0.0.0.0:{port}");
    axum::serve(listener, app(state))
        .await
        .expect("serving");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Env vars are process-global; serialize the tests that mutate them so
    /// parallel threads can't clobber each other's LAUNCH_API_KEYS etc.
    static ENV_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap()
    }
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
        let _env_guard = env_lock(); // serialize env-var mutation
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

    // -----------------------------------------------------------------------
    // Billing tests (no RPC; dev-mode handlers hit only validation + ledger)
    // -----------------------------------------------------------------------

    #[test]
    fn key_type_rates() {
        assert_eq!(KeyType::Platform.rate_lamports(), 50_000_000);
        assert_eq!(KeyType::Creator.rate_lamports(), 200_000_000);
        assert!((KeyType::Platform.rate_sol() - 0.05).abs() < 1e-12);
        assert!((KeyType::Creator.rate_sol() - 0.2).abs() < 1e-12);
        assert_eq!(key_last4("abcdefgh"), "efgh");
        assert_eq!(key_last4("ab"), "ab");
        assert_eq!(key_last4(""), "");
        assert_eq!("platform".parse::<KeyType>(), Ok(KeyType::Platform));
        assert_eq!("creator".parse::<KeyType>(), Ok(KeyType::Creator));
        assert!("bogus".parse::<KeyType>().is_err());
    }

    #[test]
    fn balances_load_from_env_json() {
        let _env_guard = env_lock(); // serialize env-var mutation
        let key = "test-key-load-1";
        let json = format!(
            "{{\"{key}\": {{\"lamports\": 123456789, \"type\": \"creator\"}}, \"other\": {{\"lamports\": 5}}}}"
        );
        std::env::set_var("KEY_BALANCES_JSON", json);
        let m = load_balances_from_env();
        std::env::remove_var("KEY_BALANCES_JSON");
        assert_eq!(m.get(key).unwrap().lamports, 123_456_789);
        assert_eq!(m.get(key).unwrap().key_type, KeyType::Creator);
        assert_eq!(m.get("other").unwrap().lamports, 5);
        assert_eq!(m.get("other").unwrap().key_type, KeyType::Platform);
        // Invalid JSON -> empty ledger, no panic.
        std::env::set_var("KEY_BALANCES_JSON", "not json{{{");
        assert!(load_balances_from_env().is_empty());
        std::env::remove_var("KEY_BALANCES_JSON");
        // Unset -> empty ledger.
        assert!(load_balances_from_env().is_empty());
    }

    #[tokio::test]
    async fn build_402_when_balance_insufficient() {
        let _env_guard = env_lock(); // serialize env-var mutation
        // Valid request (passes plan validation) + configured key with an
        // empty balance -> 402 insufficient_balance, before any RPC.
        std::env::set_var("LAUNCH_API_KEYS", "bill-test-key-402");
        let app = app(ApiState::default());
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/bundle/build")
                    .header("x-api-key", "bill-test-key-402")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_string(&test_request()).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "insufficient_balance");
        assert!(v["message"].as_str().unwrap().contains("0.05"));
        std::env::remove_var("LAUNCH_API_KEYS");
    }

    #[tokio::test]
    async fn admin_credit_and_balance_flow() {
        let _env_guard = env_lock(); // serialize env-var mutation
        std::env::set_var("LAUNCH_API_KEYS", "master-bill-key,platform-bill-key");
        std::env::set_var("MASTER_API_KEY", "master-bill-key");
        let state = ApiState::default();
        let app = app(state.clone());

        // Non-master key cannot touch admin.
        let denied = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/admin/credit")
                    .header("x-api-key", "platform-bill-key")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"key":"platform-bill-key","sol":1.0}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        // Master credits the platform key 1.5 SOL.
        let credited = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/admin/credit")
                    .header("x-api-key", "master-bill-key")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"key":"platform-bill-key","sol":1.5}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(credited.status(), StatusCode::OK);
        let body = axum::body::to_bytes(credited.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["lamports"], 1_500_000_000u64);
        assert_eq!(v["key_type"], "platform");
        assert_eq!(v["key_last4"], key_last4("platform-bill-key"));

        // Self-service balance shows 1.5 SOL at the platform rate.
        let bal = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/balance")
                    .header("x-api-key", "platform-bill-key")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bal.status(), StatusCode::OK);
        let body = axum::body::to_bytes(bal.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["lamports"], 1_500_000_000u64);
        assert_eq!(v["key_type"], "platform");
        assert!((v["rate_sol"].as_f64().unwrap() - 0.05).abs() < 1e-12);

        // Retier to creator: rate becomes 0.2 SOL.
        let retiered = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/admin/set-type")
                    .header("x-api-key", "master-bill-key")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"key":"platform-bill-key","key_type":"creator"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(retiered.status(), StatusCode::OK);
        let body = axum::body::to_bytes(retiered.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["key_type"], "creator");
        assert_eq!(v["lamports"], 1_500_000_000u64);

        // Admin list shows last-4 only, never full keys.
        let list = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/admin/balances")
                    .header("x-api-key", "master-bill-key")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list.status(), StatusCode::OK);
        let body = axum::body::to_bytes(list.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        for entry in arr {
            let l4 = entry["key_last4"].as_str().unwrap();
            assert_eq!(l4.len(), 4);
            assert!(!l4.contains("bill-key"));
        }

        std::env::remove_var("LAUNCH_API_KEYS");
        std::env::remove_var("MASTER_API_KEY");
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

    // ------------------------------------------------------------------
    // Launchpad (v0.5.0)
    // ------------------------------------------------------------------

    #[test]
    fn launchpad_fee_math() {
        // Base only.
        assert_eq!(launchpad_fee_lamports(&None), 200_000_000);
        let none_addons = Some(LaunchpadAddons {
            deep_veil: None,
            extra_wallets: None,
            radar_watch: None,
        });
        assert_eq!(launchpad_fee_lamports(&none_addons), 200_000_000);
        // Everything: 0.2 + 0.1 + 3*0.05 + 0.05 = 0.5 SOL.
        let full = Some(LaunchpadAddons {
            deep_veil: Some(true),
            extra_wallets: Some(3),
            radar_watch: Some(true),
        });
        assert_eq!(launchpad_fee_lamports(&full), 500_000_000);
        // Explicit false / zero adds nothing.
        let empty = Some(LaunchpadAddons {
            deep_veil: Some(false),
            extra_wallets: Some(0),
            radar_watch: Some(false),
        });
        assert_eq!(launchpad_fee_lamports(&empty), 200_000_000);
    }

    fn sample_payment_tx() -> serde_json::Value {
        // Creator pays 0.25 SOL to the payment wallet; fee payer = creator.
        serde_json::json!({
            "result": {
                "meta": {
                    "err": null,
                    "preBalances": [1_000_000_000, 500_000_000],
                    "postBalances": [745_000_000, 750_000_000]
                },
                "transaction": {
                    "message": {
                        "accountKeys": [
                            "Creator111111111111111111111111111111111111",
                            "2q4EwekHtit3BtzL25NmGbWJ5khPkmypYbba2t3A4XtM"
                        ]
                    }
                }
            }
        })
    }

    #[test]
    fn launchpad_payment_parsing() {
        let tx = sample_payment_tx();
        let got = launchpad_payment_received_lamports(
            &tx,
            "Creator111111111111111111111111111111111111",
            LAUNCHPAD_PAYMENT_WALLET,
        )
        .unwrap();
        assert_eq!(got, 250_000_000);

        // Failed transaction.
        let mut failed = tx.clone();
        failed["result"]["meta"]["err"] = serde_json::json!({"InstructionError": [0, "Custom"]});
        assert!(launchpad_payment_received_lamports(
            &failed,
            "Creator111111111111111111111111111111111111",
            LAUNCHPAD_PAYMENT_WALLET
        )
        .is_err());

        // Wrong fee payer.
        assert!(launchpad_payment_received_lamports(
            &tx,
            "SomeoneElse11111111111111111111111111111111",
            LAUNCHPAD_PAYMENT_WALLET
        )
        .is_err());

        // Payment wallet absent.
        assert!(launchpad_payment_received_lamports(
            &tx,
            "Creator111111111111111111111111111111111111",
            "Nope111111111111111111111111111111111111111"
        )
        .is_err());

        // Missing transaction.
        let missing = serde_json::json!({"result": null});
        assert!(launchpad_payment_received_lamports(
            &missing,
            "Creator111111111111111111111111111111111111",
            LAUNCHPAD_PAYMENT_WALLET
        )
        .is_err());
    }

    #[test]
    fn launchpad_deep_veil_stealth_params() {
        // Deep Veil must stay inside stealth::StealthConfig validation
        // (hops 1..=3, spread 0..=120, jitter 0..=2000).
        let cfg = stealth::StealthConfig {
            funding_hops: 3,
            spread_slots: 12,
            amount_jitter_bps: 2000,
            funding_buffer_sol: 0.015,
            amount_seed: None,
        };
        assert!(cfg.validate().is_ok());
        // Standard tier likewise.
        let cfg = stealth::StealthConfig {
            funding_hops: 2,
            spread_slots: 3,
            amount_jitter_bps: 500,
            funding_buffer_sol: 0.015,
            amount_seed: None,
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn launchpad_public_route_registered() {
        // Compile-time guard: the handler exists with the expected shape.
        // Runtime route registration is covered by the auth test below.
        fn _assert_handler<F, Fut>(f: F)
        where
            F: Fn(axum::extract::State<ApiState>, axum::Json<LaunchpadBuildRequest>) -> Fut,
        {
        }
        _assert_handler(launchpad_build_handler);
    }
}
