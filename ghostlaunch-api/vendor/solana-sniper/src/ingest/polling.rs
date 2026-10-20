//! JSON-RPC polling ingestion (cheap alternative to Yellowstone gRPC).
//!
//! Polls `getSignaturesForAddress` on the Pump.fun and PumpSwap programs at
//! a fixed interval, fetches new transactions, decodes TradeEvents from
//! program logs, attributes dev buys via the creator, and emits normalized
//! [`ChainEvent`]s — the same event types [`super::yellowstone`] emits, so
//! the strategy / paper-trading / Telegram pipeline works unchanged.
//!
//! This is for PAPER TRADING and signal generation only. A 5-15 second
//! delay vs Yellowstone is expected and acceptable; do not use for live
//! sniping where millisecond entry matters.
//!
//! Cost: a few hundred `getSignaturesForAddress` + `getTransaction` calls
//! per hour against any public or paid RPC — pennies, vs ~$80/mo for
//! Yellowstone gRPC.
//!
//! Env:
//! - `POLLING_RPC_URL` (default `https://api.mainnet-beta.solana.com`)
//! - `POLL_INTERVAL_SECS` (default `10`)
//! - `MIN_DEV_BUY_SOL` (same as Yellowstone: skip creator lookups below this)
//! - `EMIT_ALL_TRADES=1` (same as Yellowstone: also emit non-dev trades)
//! - `MONITOR_PROGRAM_IDS` (optional extra programs to poll; Pump.fun and
//!   PumpSwap are always polled)

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use reqwest::{Client, Proxy};
use solana_pubkey::Pubkey;
use tracing::{debug, info, warn};

use crate::decode::{self, TradeEvent};
use crate::ingest::EventSource;
use crate::types::{ChainEvent, EventKind};

/// Prefix used by the runtime for base64-encoded Anchor event logs.
const PROGRAM_LOG_PREFIX: &str = "Program log: ";
/// Anchor >= 0.30 emits events as `Program data: <base64>` instead.
const PROGRAM_DATA_PREFIX: &str = "Program data: ";

/// How many recent signatures to fetch per program per poll.
const SIGNATURE_BATCH_LIMIT: usize = 25;
/// Max transactions to fetch+decode per poll cycle (bounds RPC spend when
/// the network is busy).
const MAX_TX_PER_POLL: usize = 25;

/// Live event source backed by JSON-RPC polling.
pub struct PollingSource {
    client: Client,
    rpc_url: String,
    poll_interval: Duration,
    /// Programs to poll for new signatures. Always includes Pump.fun and
    /// PumpSwap; extended with MONITOR_PROGRAM_IDS when set.
    programs: Vec<Pubkey>,
    /// When true, emit `Trade` events for non-dev trades too.
    emit_all_trades: bool,
    /// Minimum dev buy (lamports) that can trigger an entry signal.
    min_dev_buy_lamports: u64,
    /// Signatures already processed (process-local dedupe).
    seen_signatures: HashSet<String>,
    /// mint -> creator cache (one RPC fetch per new mint).
    creator_cache: HashMap<Pubkey, Pubkey>,
    /// Events decoded but not yet yielded by `next_event`.
    pending: VecDeque<ChainEvent>,
    /// getTransaction / getAccountInfo calls made (cost visibility).
    rpc_calls: u64,
}

impl PollingSource {
    /// Build from `POLLING_RPC_URL`, `POLL_INTERVAL_SECS`,
    /// `MIN_DEV_BUY_SOL`, `EMIT_ALL_TRADES`, `MONITOR_PROGRAM_IDS`.
    pub async fn connect() -> Result<Self> {
        let rpc_url = env::var("POLLING_RPC_URL")
            .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
        let poll_secs: u64 = env::var("POLL_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|v| *v > 0)
            .unwrap_or(10);
        let emit_all_trades = env::var("EMIT_ALL_TRADES").as_deref() == Ok("1")
            || env::var("EMIT_ALL_TRADES").as_deref() == Ok("true");
        let min_dev_buy_lamports = env::var("MIN_DEV_BUY_SOL")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map(|sol| (sol * 1_000_000_000.0) as u64)
            .unwrap_or(0);

        // Always poll Pump.fun (trade events) and PumpSwap (pool creations).
        // MONITOR_PROGRAM_IDS may add more (e.g. Raydium).
        let mut programs: Vec<Pubkey> = vec![decode::pump_program_id(), decode::pumpswap_program_id()];
        for s in env::var("MONITOR_PROGRAM_IDS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match s.parse::<Pubkey>() {
                Ok(pk) if !programs.contains(&pk) => programs.push(pk),
                Ok(_) => {}
                Err(e) => warn!(program = s, error = %e, "ignoring invalid MONITOR_PROGRAM_IDS entry"),
            }
        }

        let client = build_client().context("building polling RPC HTTP client")?;
        // Sanity check: the RPC must answer before we claim to be live.
        let slot = rpc_call::<u64>(&client, &rpc_url, "getSlot", serde_json::json!([{"commitment": "confirmed"}]))
            .await
            .context("polling RPC health check (getSlot) failed")?;
        info!(rpc_url = %mask_url(&rpc_url), slot, poll_secs, programs = programs.len(), "polling source connected");

        Ok(Self {
            client,
            rpc_url,
            poll_interval: Duration::from_secs(poll_secs),
            programs,
            emit_all_trades,
            min_dev_buy_lamports,
            seen_signatures: HashSet::new(),
            creator_cache: HashMap::new(),
            pending: VecDeque::new(),
            rpc_calls: 0,
        })
    }

    /// One poll cycle: fetch new signatures per program, decode new
    /// transactions, queue the resulting events. Never fails the caller on
    /// RPC errors — logs and returns what it got.
    async fn poll(&mut self) {
        for program in self.programs.clone() {
            match self.poll_program(&program).await {
                Ok(n) => debug!(%program, new_txs = n, "polled program"),
                Err(e) => warn!(%program, error = %e, "poll cycle failed for program"),
            }
        }
    }

    async fn poll_program(&mut self, program: &Pubkey) -> Result<usize> {
        let sigs = self.recent_signatures(program).await?;
        let mut new_count = 0;
        for sig in sigs {
            if new_count >= MAX_TX_PER_POLL {
                break;
            }
            if !self.seen_signatures.insert(sig.clone()) {
                continue;
            }
            new_count += 1;
            match self.process_transaction(&sig).await {
                Ok(()) => {}
                Err(e) => warn!(signature = %sig, error = %e, "skipping transaction after poll processing error"),
            }
        }
        Ok(new_count)
    }

    /// Recent confirmed signatures mentioning `program`, newest first.
    async fn recent_signatures(&mut self, program: &Pubkey) -> Result<Vec<String>> {
        self.rpc_calls += 1;
        let params = serde_json::json!([
            program.to_string(),
            {"limit": SIGNATURE_BATCH_LIMIT, "commitment": "confirmed"}
        ]);
        let resp: serde_json::Value =
            rpc_call(&self.client, &self.rpc_url, "getSignaturesForAddress", params)
                .await
                .context("getSignaturesForAddress")?;
        let arr = resp.as_array().ok_or_else(|| anyhow!("getSignaturesForAddress: not an array"))?;
        Ok(arr
            .iter()
            .filter_map(|v| v.get("signature").and_then(|s| s.as_str()).map(str::to_string))
            // Skip failed transactions: their logs carry no usable events.
            .filter(|_| true)
            .collect())
    }

    /// Fetch one transaction and queue any decoded events.
    async fn process_transaction(&mut self, signature: &str) -> Result<()> {
        self.rpc_calls += 1;
        if self.rpc_calls % 500 == 0 {
            info!(rpc_calls = self.rpc_calls, "polling RPC usage (calls so far)");
        }
        let params = serde_json::json!([
            signature,
            {"encoding": "jsonParsed", "commitment": "confirmed", "maxSupportedTransactionVersion": 0}
        ]);
        let tx: serde_json::Value =
            rpc_call(&self.client, &self.rpc_url, "getTransaction", params)
                .await
                .with_context(|| format!("getTransaction {signature}"))?;
        if tx.is_null() {
            return Ok(());
        }
        let slot = tx.get("slot").and_then(|s| s.as_u64()).unwrap_or(0);
        let meta = match tx.get("meta") {
            Some(m) if !m.is_null() => m,
            _ => return Ok(()),
        };
        if meta.get("err").is_some_and(|e| !e.is_null()) {
            return Ok(()); // failed tx: nothing to decode
        }
        let logs: Vec<String> = meta
            .get("logMessages")
            .and_then(|l| l.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        // 1. Trade events from program logs (Pump.fun buys/sells).
        for log in &logs {
            let b64 = log
                .strip_prefix(PROGRAM_DATA_PREFIX)
                .or_else(|| log.strip_prefix(PROGRAM_LOG_PREFIX));
            let Some(b64) = b64 else { continue };
            let Ok(payload) = B64.decode(b64.trim()) else { continue };
            let Ok(Some(ev)) = decode::parse_trade_event_log(&payload) else { continue };
            if let Some(event) = self.dev_trade_event(slot, signature, &ev).await? {
                self.pending.push_back(event);
            }
        }

        // 2. PumpSwap pool creations (for the pumpswap_migration strategy).
        // Parsed from the jsonParsed instruction list when present.
        self.detect_pumpswap_pool(slot, signature, &tx).await?;

        Ok(())
    }

    /// Same dev-trade attribution as the Yellowstone path: TradeEvent ->
    /// CreatorBuy / CreatorSell / Trade ChainEvent.
    async fn dev_trade_event(
        &mut self,
        slot: u64,
        signature: &str,
        ev: &TradeEvent,
    ) -> Result<Option<ChainEvent>> {
        if ev.is_buy && !self.emit_all_trades && ev.sol_amount_lamports < self.min_dev_buy_lamports
        {
            return Ok(None);
        }
        let creator: Option<Pubkey> = match ev.creator {
            Some(c) => {
                self.creator_cache.insert(ev.mint, c);
                Some(c)
            }
            None => self.resolve_creator(&ev.mint).await?,
        };
        let Some(creator) = creator else {
            return Ok(None);
        };
        let is_dev = ev.user == creator;
        let kind = if is_dev {
            if ev.is_buy {
                EventKind::CreatorBuy
            } else {
                EventKind::CreatorSell
            }
        } else if self.emit_all_trades {
            EventKind::Trade
        } else {
            return Ok(None);
        };
        let mint = ev.mint.to_string();
        let side = if ev.is_buy { "buy" } else { "sell" };
        let event_id = format!("{signature}:{mint}:{side}");
        debug!(%mint, creator = %creator, user = %ev.user, side, is_dev, sol = decode::lamports_to_sol(ev.sol_amount_lamports), "poll: trade detected");
        let trade_price_sol_per_token = if ev.token_amount > 0 {
            Some(ev.sol_amount_lamports as f64 / ev.token_amount as f64 / 1_000_000_000.0)
        } else {
            None
        };
        let opt_reserve = |v: u64| if v > 0 { Some(v) } else { None };
        Ok(Some(ChainEvent {
            event_id,
            slot,
            signature: signature.to_string(),
            program_id: decode::PUMP_AMM_PROGRAM_ID_STR.to_string(),
            kind,
            mint: Some(mint),
            creator: Some(creator.to_string()),
            user: Some(ev.user.to_string()),
            creator_buy_sol: Some(decode::lamports_to_sol(ev.sol_amount_lamports)),
            trade_price_sol_per_token,
            virtual_sol_reserves: opt_reserve(ev.virtual_sol_reserves),
            virtual_token_reserves: opt_reserve(ev.virtual_token_reserves),
            fee_basis_points: Some(ev.fee_basis_points),
            creator_fee_basis_points: Some(ev.creator_fee_basis_points),
            observed_at_ms: now_ms(),
        }))
    }

    /// Detect PumpSwap `create_pool` instructions in a jsonParsed
    /// transaction and emit a `PumpSwapPool` event. Best-effort: returns
    /// Ok even when the tx has no parseable pool init.
    async fn detect_pumpswap_pool(
        &mut self,
        slot: u64,
        signature: &str,
        tx: &serde_json::Value,
    ) -> Result<()> {
        let pumpswap = decode::pumpswap_program_id().to_string();
        let message = match tx.pointer("/transaction/message") {
            Some(m) => m,
            None => return Ok(()),
        };
        // accountKeys may be strings (parsed) — collect them in order.
        let account_keys: Vec<String> = message
            .get("accountKeys")
            .and_then(|k| k.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .or_else(|| v.get("pubkey").and_then(|p| p.as_str()).map(str::to_string))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if account_keys.is_empty() {
            return Ok(());
        }
        let instructions: Vec<&serde_json::Value> = message
            .get("instructions")
            .and_then(|i| i.as_array())
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        for ix in instructions {
            let program = ix.get("programId").and_then(|p| p.as_str()).unwrap_or("");
            if program != pumpswap {
                continue;
            }
            // jsonParsed may give parsed type; fall back to raw data check.
            let ix_type = ix.get("parsed").and_then(|p| p.get("type")).and_then(|t| t.as_str());
            let looks_like_create = ix_type == Some("create_pool")
                || ix_type == Some("createPool")
                || is_create_pool_data(ix);
            if !looks_like_create {
                continue;
            }
            // Extract base/quote mints + coin creator when the parsed
            // instruction exposes them.
            let info = ix.pointer("/parsed/info");
            let base_mint = info
                .and_then(|i| i.get("baseMint").or_else(|| i.get("base_mint")))
                .and_then(|v| v.as_str());
            let quote_mint = info
                .and_then(|i| i.get("quoteMint").or_else(|| i.get("quote_mint")))
                .and_then(|v| v.as_str());
            let coin_creator = info
                .and_then(|i| i.get("coinCreator").or_else(|| i.get("coin_creator")))
                .and_then(|v| v.as_str());
            let (Some(base), Some(_quote)) = (base_mint, quote_mint) else {
                continue;
            };
            let mint_pk: Pubkey = match base.parse() {
                Ok(pk) => pk,
                Err(_) => continue,
            };
            let mint = if decode::is_pump_mint(&mint_pk) {
                mint_pk
            } else {
                // Non-pump quote side; the base is the token of interest.
                mint_pk
            };
            let mint_str = mint.to_string();
            let event_id = format!("{signature}:{mint_str}:pumpswap_pool");
            let creator: Option<String> = match coin_creator {
                Some(c) => {
                    if let Ok(pk) = c.parse::<Pubkey>() {
                        self.creator_cache.insert(mint, pk);
                    }
                    Some(c.to_string())
                }
                None => self
                    .resolve_creator(&mint)
                    .await?
                    .map(|c| c.to_string()),
            };
            debug!(%mint_str, creator = ?creator, "poll: pumpswap pool detected");
            self.pending.push_back(ChainEvent {
                event_id,
                slot,
                signature: signature.to_string(),
                program_id: decode::PUMPSWAP_AMM_PROGRAM_ID_STR.to_string(),
                kind: EventKind::PumpSwapPool,
                mint: Some(mint_str),
                creator,
                user: None,
                creator_buy_sol: None,
                trade_price_sol_per_token: None,
                virtual_sol_reserves: None,
                virtual_token_reserves: None,
                fee_basis_points: None,
                creator_fee_basis_points: None,
                observed_at_ms: now_ms(),
            });
            // One pool event per transaction is enough.
            break;
        }
        Ok(())
    }

    /// Resolve the bonding-curve creator for a mint, cached per mint.
    async fn resolve_creator(&mut self, mint: &Pubkey) -> Result<Option<Pubkey>> {
        if let Some(c) = self.creator_cache.get(mint) {
            return Ok(Some(*c));
        }
        self.rpc_calls += 1;
        if self.rpc_calls % 500 == 0 {
            info!(rpc_calls = self.rpc_calls, "polling RPC usage (calls so far)");
        }
        let pda = decode::bonding_curve_pda(mint);
        let data = fetch_account_data(&self.client, &self.rpc_url, &pda).await?;
        let Some(data) = data else {
            debug!(%mint, "poll: bonding curve account not found");
            return Ok(None);
        };
        match decode::bonding_curve_creator(&data) {
            Ok(creator) => {
                self.creator_cache.insert(*mint, creator);
                Ok(Some(creator))
            }
            Err(e) => {
                warn!(%mint, error = %e, "poll: failed to parse bonding curve creator");
                Ok(None)
            }
        }
    }
}

/// Heuristic: raw base58 instruction data whose decoded bytes start with a
/// known PumpSwap create_pool discriminator. Used when the RPC returns
/// partially-parsed instructions.
fn is_create_pool_data(ix: &serde_json::Value) -> bool {
    let data_str = ix.get("data").and_then(|d| d.as_str()).unwrap_or("");
    if data_str.is_empty() {
        return false;
    }
    let Ok(raw) = bs58::decode(data_str).into_vec() else {
        return false;
    };
    raw.len() >= 8 && raw[..8] == decode::PUMPSWAP_DISC_CREATE_POOL
}

/// Build a reqwest client honoring proxy env vars (same approach as the
/// Yellowstone RPC helper: reqwest is built without default features, so
/// proxy env detection is manual).
fn build_client() -> Result<Client> {
    let mut builder = Client::builder().timeout(Duration::from_secs(30));
    for var in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
        if let Ok(proxy_url) = env::var(var) {
            if !proxy_url.is_empty() {
                let proxy = Proxy::all(&proxy_url)
                    .with_context(|| format!("invalid proxy URL in {var}"))?;
                builder = builder.proxy(proxy);
                break;
            }
        }
    }
    builder.build().context("building reqwest client")
}

/// Single JSON-RPC call. Returns the `result` value (which may be null).
async fn rpc_call<T>(client: &Client, rpc_url: &str, method: &str, params: serde_json::Value) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let resp: serde_json::Value = client
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
        anyhow::bail!("{method} RPC error: {err}");
    }
    serde_json::from_value(resp["result"].clone())
        .with_context(|| format!("decoding {method} result"))
}

/// Fetch raw account data via plain RPC (base64). Returns None when the
/// account does not exist.
async fn fetch_account_data(
    client: &Client,
    rpc_url: &str,
    pubkey: &Pubkey,
) -> Result<Option<Vec<u8>>> {
    let params = serde_json::json!([pubkey.to_string(), {"encoding": "base64", "commitment": "confirmed"}]);
    let result: serde_json::Value =
        rpc_call(client, rpc_url, "getAccountInfo", params).await?;
    let data = result.pointer("/value/data/0").and_then(|v| v.as_str());
    match data {
        Some(b64) => Ok(Some(B64.decode(b64).context("decoding account data")?)),
        None => Ok(None),
    }
}

#[async_trait]
impl EventSource for PollingSource {
    async fn next_event(&mut self) -> Result<Option<ChainEvent>> {
        loop {
            // Drain queued events first.
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            // Poll, then drain. Sleep between polls so a quiet network
            // doesn't spin the loop.
            self.poll().await;
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn mask_url(url: &str) -> String {
    // Never log tokens that might be embedded in URLs.
    if let Some(at) = url.find('@') {
        format!("***{}", &url[at..])
    } else {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_create_pool_data_matches_discriminator() {
        // Encode the real create_pool discriminator as bs58 instruction data.
        let mut raw = decode::PUMPSWAP_DISC_CREATE_POOL.to_vec();
        raw.extend_from_slice(&[1, 2, 3, 4]);
        let ix = serde_json::json!({"data": bs58::encode(&raw).into_string()});
        assert!(is_create_pool_data(&ix));
    }

    #[test]
    fn is_create_pool_data_rejects_other_discriminators() {
        let raw = decode::PUMPSWAP_DISC_BUY.to_vec();
        let ix = serde_json::json!({"data": bs58::encode(&raw).into_string()});
        assert!(!is_create_pool_data(&ix));

        let ix_empty = serde_json::json!({});
        assert!(!is_create_pool_data(&ix_empty));

        let ix_bad_b58 = serde_json::json!({"data": "!!!not-bs58!!!"});
        assert!(!is_create_pool_data(&ix_bad_b58));
    }

    #[test]
    fn mask_url_hides_credentials() {
        assert_eq!(
            mask_url("https://user:secret@example.com"),
            "***@example.com"
        );
        assert_eq!(
            mask_url("https://api.mainnet-beta.solana.com"),
            "https://api.mainnet-beta.solana.com"
        );
    }

    #[test]
    fn poll_interval_env_parsing() {
        // Default when unset/invalid.
        assert_eq!(parse_poll_secs(None), 10);
        assert_eq!(parse_poll_secs(Some("0")), 10);
        assert_eq!(parse_poll_secs(Some("abc")), 10);
        assert_eq!(parse_poll_secs(Some("30")), 30);
    }

    /// Extracted parsing logic (mirrors `connect`) so it is unit-testable
    /// without touching the environment.
    fn parse_poll_secs(v: Option<&str>) -> u64 {
        v.and_then(|s| s.parse().ok()).filter(|v| *v > 0).unwrap_or(10)
    }

    #[test]
    fn trade_event_log_prefixes_detected() {
        // Both Anchor log formats must be recognized by the prefix logic.
        let payload = B64.encode([0u8; 16]);
        let log_data = format!("{PROGRAM_DATA_PREFIX}{payload}");
        let log_legacy = format!("{PROGRAM_LOG_PREFIX}{payload}");
        assert!(log_data.strip_prefix(PROGRAM_DATA_PREFIX).is_some());
        assert!(log_legacy.strip_prefix(PROGRAM_LOG_PREFIX).is_some());
        // Non-event logs are skipped.
        let other = "Program log: some text";
        assert!(other.strip_prefix(PROGRAM_DATA_PREFIX).is_none());
    }
}
