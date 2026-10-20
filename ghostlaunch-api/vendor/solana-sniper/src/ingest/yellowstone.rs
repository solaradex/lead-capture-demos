//! Live Yellowstone gRPC ingestion.
//!
//! Subscribes to transactions mentioning the configured program IDs
//! (Pump.fun AMM, etc.), decodes Pump.fun TradeEvents from program logs,
//! attributes dev buys via the bonding-curve creator, and emits normalized
//! [`ChainEvent`]s. This is the live counterpart to [`ReplaySource`].

use std::collections::{HashMap, HashSet};
use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use futures::StreamExt;
use reqwest::{Client, Proxy};
use solana_pubkey::Pubkey;
use tonic::transport::ClientTlsConfig;
use tracing::{debug, info, warn};
use yellowstone_grpc_client::{GeyserGrpcClient, GeyserStream, SubscribeRequestSink};
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterTransactions, SubscribeUpdateTransactionInfo,
};
use yellowstone_grpc_proto::solana::storage::confirmed_block::TransactionStatusMeta;

use crate::decode::{self, TradeEvent};
use crate::ingest::EventSource;
use crate::proxy::{self, ProxyConnector};
use crate::types::{ChainEvent, EventKind};

/// Prefix used by the runtime for base64-encoded Anchor event logs.
const PROGRAM_LOG_PREFIX: &str = "Program log: ";
/// Anchor >= 0.30 emits events as `Program data: <base64>` instead.
const PROGRAM_DATA_PREFIX: &str = "Program data: ";

/// Live event source backed by Yellowstone gRPC.
pub struct YellowstoneSource {
    /// Held for the life of the source: dropping it half-closes the
    /// subscription and the server stops sending updates.
    _sink: SubscribeRequestSink,
    stream: GeyserStream,
    rpc_url: String,
    /// When true, emit `Trade` events for non-dev trades too (launch-snipe mode).
    emit_all_trades: bool,
    /// Minimum dev buy (lamports) that can trigger an entry signal. Used to
    /// skip the creator RPC lookup for buys that are too small to matter.
    min_dev_buy_lamports: u64,
    /// mint -> creator cache (one RPC fetch per new mint).
    creator_cache: HashMap<Pubkey, Pubkey>,
    /// Mints we've observed a Raydium pool init for this session. Swap
    /// price events are only emitted for these (or pump-suffixed mints),
    /// so the firehose of unrelated Raydium swaps doesn't flood the loop.
    pool_mints: HashSet<Pubkey>,
    /// Signatures already emitted (process-local dedupe).
    seen: HashSet<String>,
    /// getAccountInfo calls made (cost visibility for Alchemy PAYG).
    rpc_calls: u64,
}

impl YellowstoneSource {
    /// Connect using YELLOWSTONE_GRPC_ENDPOINT, YELLOWSTONE_X_TOKEN,
    /// SOLANA_RPC_URL and MONITOR_PROGRAM_IDS from the environment.
    /// EMIT_ALL_TRADES=1 also emits non-dev trades (for launch sniping).
    pub async fn connect() -> Result<Self> {
        let endpoint = env::var("YELLOWSTONE_GRPC_ENDPOINT")
            .context("YELLOWSTONE_GRPC_ENDPOINT is not set")?;
        let x_token =
            env::var("YELLOWSTONE_X_TOKEN").context("YELLOWSTONE_X_TOKEN is not set")?;
        let rpc_url =
            env::var("SOLANA_RPC_URL").context("SOLANA_RPC_URL is not set")?;
        let programs: Vec<String> = env::var("MONITOR_PROGRAM_IDS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if programs.is_empty() {
            return Err(anyhow!("MONITOR_PROGRAM_IDS is empty"));
        }
        let emit_all_trades = env::var("EMIT_ALL_TRADES").as_deref() == Ok("1")
            || env::var("EMIT_ALL_TRADES").as_deref() == Ok("true");
        // Same env var the strategy reads; used to skip creator RPC lookups
        // for buys too small to ever trigger a signal.
        let min_dev_buy_lamports = env::var("MIN_DEV_BUY_SOL")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map(|sol| (sol * 1_000_000_000.0) as u64)
            .unwrap_or(0);

        info!(endpoint = %mask_endpoint(&endpoint), programs = ?programs, "connecting to Yellowstone gRPC");
        let builder = GeyserGrpcClient::build_from_shared(endpoint)?
            .x_token(Some(x_token))?
            .tls_config(ClientTlsConfig::new().with_native_roots())?;
        // Sandboxed egress goes through an HTTP CONNECT proxy (HTTPS_PROXY);
        // tonic ignores proxy env vars, so tunnel explicitly when configured.
        let mut client = if proxy::proxy_from_env().is_some() {
            info!("egress proxy detected, tunneling gRPC through CONNECT proxy");
            builder
                .connect_with_connector(ProxyConnector::from_env()?)
                .await
                .context("connecting to Yellowstone gRPC via proxy")?
        } else {
            builder
                .connect()
                .await
                .context("connecting to Yellowstone gRPC")?
        };

        let mut filters = HashMap::new();
        filters.insert(
            "sniper".to_string(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                account_include: programs,
                ..Default::default()
            },
        );

        let (sink, stream) = client
            .subscribe_with_request(Some(SubscribeRequest {
                transactions: filters,
                commitment: Some(CommitmentLevel::Confirmed as i32),
                ..Default::default()
            }))
            .await
            .context("subscribe")?;
        info!("Yellowstone subscription active");

        Ok(Self {
            _sink: sink,
            stream,
            rpc_url,
            emit_all_trades,
            min_dev_buy_lamports,
            creator_cache: HashMap::new(),
            pool_mints: HashSet::new(),
            seen: HashSet::new(),
            rpc_calls: 0,
        })
    }

    /// Resolve the bonding-curve creator for a mint, cached per mint.
    async fn resolve_creator(&mut self, mint: &Pubkey) -> Result<Option<Pubkey>> {
        if let Some(c) = self.creator_cache.get(mint) {
            return Ok(Some(*c));
        }
        self.rpc_calls += 1;
        if self.rpc_calls % 500 == 0 {
            info!(rpc_calls = self.rpc_calls, "Alchemy RPC usage (getAccountInfo calls so far)");
        }
        let pda = decode::bonding_curve_pda(mint);
        // Clone the RPC URL out: `GeyserStream` is not `Sync`, so `&self`
        // must not be held across an await. The HTTP client is built fresh
        // per call inside `fetch_account_data` to pick up rotated proxy creds.
        let rpc_url = self.rpc_url.clone();
        let data = fetch_account_data(&rpc_url, &pda).await?;
        let Some(data) = data else {
            debug!(%mint, "bonding curve account not found");
            return Ok(None);
        };
        match decode::bonding_curve_creator(&data) {
            Ok(creator) => {
                self.creator_cache.insert(*mint, creator);
                Ok(Some(creator))
            }
            Err(e) => {
                warn!(%mint, error = %e, "failed to parse bonding curve creator");
                Ok(None)
            }
        }
    }

    /// Scan one transaction's logs for a dev-buy TradeEvent.
    async fn process_logs(
        &mut self,
        slot: u64,
        signature: &str,
        logs: &[String],
    ) -> Result<Option<ChainEvent>> {
        for log in logs {
            // Anchor >= 0.30 emits events under "Program data:"; older programs
            // (and free-form logging) use "Program log:". Check both.
            let b64 = log
                .strip_prefix(PROGRAM_DATA_PREFIX)
                .or_else(|| log.strip_prefix(PROGRAM_LOG_PREFIX));
            let Some(b64) = b64 else {
                continue;
            };
            let Ok(payload) = B64.decode(b64.trim()) else {
                continue;
            };
            let Ok(Some(ev)) = decode::parse_trade_event_log(&payload) else {
                continue;
            };
            // Both buys and sells flow through: buys feed the entry strategy,
            // creator sells feed the dev-dump tripwire.
            if let Some(chain_event) = self.dev_trade_event(slot, signature, &ev).await? {
                return Ok(Some(chain_event));
            }
        }
        Ok(None)
    }

    async fn dev_trade_event(
        &mut self,
        slot: u64,
        signature: &str,
        ev: &TradeEvent,
    ) -> Result<Option<ChainEvent>> {
        // Cost guard: a buy below the signal threshold can never trigger an
        // entry, so don't spend an RPC call resolving its creator. (Skipped
        // in launch-snipe mode, where every trade is emitted.)
        if ev.is_buy && !self.emit_all_trades && ev.sol_amount_lamports < self.min_dev_buy_lamports
        {
            return Ok(None);
        }
        // Creator attribution, cheapest source first: the TradeEvent payload
        // itself carries the creator when the extended fields are present
        // (zero RPC). Falls back to the bonding-curve account lookup for
        // short/older payloads. Either way the result is cached per mint.
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
        // Dev trades feed entry (buys) and the dump tripwire (sells).
        // Non-dev trades are only emitted in launch-snipe mode.
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
        if !self.seen.insert(event_id.clone()) {
            return Ok(None);
        }
        debug!(%mint, creator = %creator, user = %ev.user, side, is_dev, sol = decode::lamports_to_sol(ev.sol_amount_lamports), "trade detected");
        let trade_price_sol_per_token = if ev.token_amount > 0 {
            Some(ev.sol_amount_lamports as f64 / ev.token_amount as f64 / 1_000_000_000.0)
        } else {
            None
        };
        // Curve state for quoting buy_exact_sol_in. Zero means the payload
        // predates the field — report as unknown rather than a real value.
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

    /// Scan one transaction for Raydium AMM activity: pool creations
    /// (initialize / initialize2, top-level or CPI from e.g. Pump.fun's
    /// migrate) and swaps (for the price feed that drives exits).
    ///
    /// Returns the first interesting event found, if any. Pure detection —
    /// no RPC calls except the bounded creator lookup for pump-mint pools.
    async fn process_raydium(
        &mut self,
        slot: u64,
        signature: &str,
        tx_info: &SubscribeUpdateTransactionInfo,
        meta: &TransactionStatusMeta,
    ) -> Result<Option<ChainEvent>> {
        let msg = tx_info
            .transaction
            .as_ref()
            .and_then(|t| t.message.as_ref());
        let Some(msg) = msg else { return Ok(None) };
        // Raw 32-byte account keys -> Pubkey. Bail on malformed keys rather
        // than misattributing accounts.
        let account_keys: Vec<Pubkey> = msg
            .account_keys
            .iter()
            .map(|k| <[u8; 32]>::try_from(k.as_slice()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(|v| v.into_iter().map(Pubkey::new_from_array).collect())
            .unwrap_or_default();
        if account_keys.len() != msg.account_keys.len() {
            return Ok(None);
        }
        let raydium = decode::raydium_program_id();
        if !account_keys.contains(&raydium) {
            return Ok(None);
        }

        // Flatten top-level + inner (CPI) instructions into one scan list.
        // Each entry: (program_id_index, account_indices, data).
        let mut ixs: Vec<(u32, &[u8], &[u8])> = Vec::new();
        for ix in &msg.instructions {
            ixs.push((ix.program_id_index, ix.accounts.as_slice(), ix.data.as_slice()));
        }
        for inner in &meta.inner_instructions {
            for ix in &inner.instructions {
                ixs.push((ix.program_id_index, ix.accounts.as_slice(), ix.data.as_slice()));
            }
        }
        let is_raydium = |pid_idx: u32| -> bool {
            account_keys.get(pid_idx as usize).copied() == Some(raydium)
        };

        // 1. Pool creations.
        for (pid_idx, accounts, data) in &ixs {
            if !is_raydium(*pid_idx) {
                continue;
            }
            let pid = raydium; // checked above
            if let Some(init) =
                decode::parse_raydium_pool_init(&pid, data, accounts, &account_keys)?
            {
                if let Some(event) = self.raydium_pool_event(slot, signature, &init).await? {
                    return Ok(Some(event));
                }
                // A pool-init tx won't carry that pool's swaps; skip the
                // (expensive) swap scan for this transaction.
                return Ok(None);
            }
        }

        // 2. Swaps -> price feed. Only worth emitting for mints we might
        // hold: pump-suffixed mints or pools seen this session.
        let has_swap = ixs.iter().any(|(pid_idx, _, data)| {
            is_raydium(*pid_idx)
                && matches!(
                    data.first(),
                    Some(d) if *d == decode::RAYDIUM_DISC_SWAP_BASE_IN
                        || *d == decode::RAYDIUM_DISC_SWAP_BASE_OUT
                )
        });
        if !has_swap {
            return Ok(None);
        }
        self.raydium_swap_event(slot, signature, meta, &ixs, &account_keys)
            .await
    }

    /// Scan one transaction for PumpSwap AMM activity: pool creations
    /// (create_pool, top-level or CPI from Pump.fun's migrate instruction)
    /// and swaps (for the price feed that drives exits).
    ///
    /// Returns the first interesting event found, if any. Pure detection —
    /// no RPC calls except the bounded creator lookup for pump-mint pools.
    async fn process_pumpswap(
        &mut self,
        slot: u64,
        signature: &str,
        tx_info: &SubscribeUpdateTransactionInfo,
        meta: &TransactionStatusMeta,
    ) -> Result<Option<ChainEvent>> {
        let msg = tx_info
            .transaction
            .as_ref()
            .and_then(|t| t.message.as_ref());
        let Some(msg) = msg else { return Ok(None) };
        // Raw 32-byte account keys -> Pubkey. Bail on malformed keys rather
        // than misattributing accounts.
        let account_keys: Vec<Pubkey> = msg
            .account_keys
            .iter()
            .map(|k| <[u8; 32]>::try_from(k.as_slice()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(|v| v.into_iter().map(Pubkey::new_from_array).collect())
            .unwrap_or_default();
        if account_keys.len() != msg.account_keys.len() {
            return Ok(None);
        }
        let pumpswap = decode::pumpswap_program_id();
        if !account_keys.contains(&pumpswap) {
            return Ok(None);
        }

        // Flatten top-level + inner (CPI) instructions into one scan list.
        // Each entry: (program_id_index, account_indices, data).
        let mut ixs: Vec<(u32, &[u8], &[u8])> = Vec::new();
        for ix in &msg.instructions {
            ixs.push((ix.program_id_index, ix.accounts.as_slice(), ix.data.as_slice()));
        }
        for inner in &meta.inner_instructions {
            for ix in &inner.instructions {
                ixs.push((ix.program_id_index, ix.accounts.as_slice(), ix.data.as_slice()));
            }
        }
        let is_pumpswap = |pid_idx: u32| -> bool {
            account_keys.get(pid_idx as usize).copied() == Some(pumpswap)
        };

        // 1. Pool creations.
        for (pid_idx, accounts, data) in &ixs {
            if !is_pumpswap(*pid_idx) {
                continue;
            }
            let pid = pumpswap; // checked above
            if let Some(init) =
                decode::parse_pumpswap_pool_init(&pid, data, accounts, &account_keys)?
            {
                if let Some(event) = self.pumpswap_pool_event(slot, signature, &init).await? {
                    return Ok(Some(event));
                }
                // A pool-init tx won't carry that pool's swaps; skip the
                // (expensive) swap scan for this transaction.
                return Ok(None);
            }
        }

        // 2. Swaps -> price feed. Only worth emitting for mints we might
        // hold: pump-suffixed mints or pools seen this session.
        let has_swap = ixs.iter().any(|(pid_idx, _, data)| {
            is_pumpswap(*pid_idx)
                && data.len() >= 8
                && (data[..8] == decode::PUMPSWAP_DISC_BUY || data[..8] == decode::PUMPSWAP_DISC_SELL)
        });
        if !has_swap {
            return Ok(None);
        }
        self.pumpswap_swap_event(slot, signature, meta, &ixs, &account_keys)
            .await
    }

    /// Build a PumpSwapPool ChainEvent for a freshly created pool.
    async fn pumpswap_pool_event(
        &mut self,
        slot: u64,
        signature: &str,
        init: &decode::PumpSwapPoolInit,
    ) -> Result<Option<ChainEvent>> {
        // The migrated token is the pump-suffixed side; otherwise the base.
        let mint = if decode::is_pump_mint(&init.base_mint) {
            init.base_mint
        } else if decode::is_pump_mint(&init.quote_mint) {
            init.quote_mint
        } else {
            init.base_mint
        };
        self.pool_mints.insert(mint);
        let mint_str = mint.to_string();
        let event_id = format!("{signature}:{mint_str}:pumpswap_pool");
        if !self.seen.insert(event_id.clone()) {
            return Ok(None);
        }
        // Resolve the Pump.fun creator for later dev-dump attribution.
        // Prefer the coin_creator carried in the create_pool instruction:
        // zero RPC, always present on migration pools. The legacy fallback
        // (bonding-curve account lookup) is broken on current mainnet
        // layout and fails gracefully to None.
        let creator_pk: Option<Pubkey> = match init.coin_creator {
            Some(c) => {
                // Cache it so the swap-path tripwire can attribute later sells.
                self.creator_cache.insert(mint, c);
                Some(c)
            }
            None if decode::is_pump_mint(&mint) => self.resolve_creator(&mint).await?,
            None => None,
        };
        let creator = creator_pk.map(|c| c.to_string());
        Ok(Some(ChainEvent {
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
        }))
    }

    /// Build a Trade (or CreatorSell) ChainEvent from a PumpSwap swap's
    /// balance changes. Returns None when the swap isn't for a tracked mint
    /// or no clean price is visible.
    async fn pumpswap_swap_event(
        &mut self,
        slot: u64,
        signature: &str,
        meta: &TransactionStatusMeta,
        ixs: &[(u32, &[u8], &[u8])],
        account_keys: &[Pubkey],
    ) -> Result<Option<ChainEvent>> {
        // Per-(account, mint) balance deltas in base units.
        let mut pre: HashMap<(u32, &str), (i128, u32)> = HashMap::new();
        for b in &meta.pre_token_balances {
            if let Some(ui) = &b.ui_token_amount {
                if let Ok(amt) = ui.amount.parse::<i128>() {
                    pre.insert((b.account_index, b.mint.as_str()), (amt, ui.decimals));
                }
            }
        }
        let mut deltas: Vec<decode::TokenBalanceDelta> = Vec::new();
        for b in &meta.post_token_balances {
            let Some(ui) = &b.ui_token_amount else { continue };
            let Ok(post_amt) = ui.amount.parse::<i128>() else { continue };
            let key = (b.account_index, b.mint.as_str());
            let (pre_amt, _) = pre.get(&key).copied().unwrap_or((0, ui.decimals));
            let delta = post_amt - pre_amt;
            if delta != 0 {
                deltas.push(decode::TokenBalanceDelta {
                    mint: b.mint.clone(),
                    decimals: ui.decimals,
                    delta_base_units: delta,
                });
            }
        }
        // Accounts closed mid-tx (in pre, absent from post).
        for b in &meta.pre_token_balances {
            let seen_post = meta
                .post_token_balances
                .iter()
                .any(|p| p.account_index == b.account_index && p.mint == b.mint);
            if seen_post {
                continue;
            }
            if let Some(ui) = &b.ui_token_amount {
                if let Ok(amt) = ui.amount.parse::<i128>() {
                    if amt != 0 {
                        deltas.push(decode::TokenBalanceDelta {
                            mint: b.mint.clone(),
                            decimals: ui.decimals,
                            delta_base_units: -amt,
                        });
                    }
                }
            }
        }
        // Same venue-agnostic balance-delta price heuristic as Raydium: the
        // two largest movers should be the traded token and wSOL.
        let Some((mint_str, price)) = decode::raydium_swap_price(&deltas) else {
            return Ok(None);
        };
        // Relevance filter: pump mints or pools seen this session only.
        let Ok(mint_pk) = mint_str.parse::<Pubkey>() else { return Ok(None) };
        if !decode::is_pump_mint(&mint_pk) && !self.pool_mints.contains(&mint_pk) {
            return Ok(None);
        }
        let event_id = format!("{signature}:{mint_str}:swap");
        if !self.seen.insert(event_id.clone()) {
            return Ok(None);
        }
        // Dev-dump attribution from the cached creator only (no RPC here).
        // `user` sits at accounts[1] of a buy / sell, per the pump_amm IDL.
        let pumpswap = decode::pumpswap_program_id();
        let mut swapper: Option<Pubkey> = None;
        for (pid_idx, accounts, data) in ixs {
            if account_keys.get(*pid_idx as usize).copied() != Some(pumpswap) {
                continue;
            }
            if data.len() >= 8
                && (data[..8] == decode::PUMPSWAP_DISC_BUY || data[..8] == decode::PUMPSWAP_DISC_SELL)
            {
                if accounts.len() > decode::PUMPSWAP_SWAP_USER_IDX {
                    swapper = account_keys
                        .get(accounts[decode::PUMPSWAP_SWAP_USER_IDX] as usize)
                        .copied();
                }
                break;
            }
        }
        let creator = self.creator_cache.get(&mint_pk).copied();
        let kind = match (swapper, creator) {
            (Some(u), Some(c)) if u == c => EventKind::CreatorSell,
            _ => EventKind::Trade,
        };
        Ok(Some(ChainEvent {
            event_id,
            slot,
            signature: signature.to_string(),
            program_id: decode::PUMPSWAP_AMM_PROGRAM_ID_STR.to_string(),
            kind,
            mint: Some(mint_str),
            creator: creator.map(|c| c.to_string()),
            user: swapper.map(|u| u.to_string()),
            creator_buy_sol: None,
            trade_price_sol_per_token: Some(price),
            virtual_sol_reserves: None,
            virtual_token_reserves: None,
            fee_basis_points: None,
            creator_fee_basis_points: None,
            observed_at_ms: now_ms(),
        }))
    }

    /// Build a RaydiumPool ChainEvent for a freshly created pool.
    async fn raydium_pool_event(
        &mut self,
        slot: u64,
        signature: &str,
        init: &decode::RaydiumPoolInit,
    ) -> Result<Option<ChainEvent>> {
        // The migrated token is the pump-suffixed side; otherwise the base.
        let mint = if decode::is_pump_mint(&init.coin_mint) {
            init.coin_mint
        } else if decode::is_pump_mint(&init.pc_mint) {
            init.pc_mint
        } else {
            init.coin_mint
        };
        self.pool_mints.insert(mint);
        let mint_str = mint.to_string();
        let event_id = format!("{signature}:{mint_str}:raydium_pool");
        if !self.seen.insert(event_id.clone()) {
            return Ok(None);
        }
        // Resolve the Pump.fun creator for later dev-dump attribution.
        // Bounded: one RPC call per new pump mint, then cached. Note: the
        // bonding-curve lookup is broken on current mainnet layout (account
        // is 49 bytes, creator field gone), so this typically resolves to
        // None; Raydium is a secondary venue and the PumpSwap path carries
        // the creator in-instruction instead.
        let creator = if decode::is_pump_mint(&mint) {
            self.resolve_creator(&mint).await?.map(|c| c.to_string())
        } else {
            None
        };
        Ok(Some(ChainEvent {
            event_id,
            slot,
            signature: signature.to_string(),
            program_id: decode::RAYDIUM_AMM_PROGRAM_ID_STR.to_string(),
            kind: EventKind::RaydiumPool,
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
        }))
    }

    /// Build a Trade (or CreatorSell) ChainEvent from a Raydium swap's
    /// balance changes. Returns None when the swap isn't for a tracked mint
    /// or no clean price is visible.
    async fn raydium_swap_event(
        &mut self,
        slot: u64,
        signature: &str,
        meta: &TransactionStatusMeta,
        ixs: &[(u32, &[u8], &[u8])],
        account_keys: &[Pubkey],
    ) -> Result<Option<ChainEvent>> {
        // Per-(account, mint) balance deltas in base units.
        let mut pre: HashMap<(u32, &str), (i128, u32)> = HashMap::new();
        for b in &meta.pre_token_balances {
            if let Some(ui) = &b.ui_token_amount {
                if let Ok(amt) = ui.amount.parse::<i128>() {
                    pre.insert((b.account_index, b.mint.as_str()), (amt, ui.decimals));
                }
            }
        }
        let mut deltas: Vec<decode::TokenBalanceDelta> = Vec::new();
        for b in &meta.post_token_balances {
            let Some(ui) = &b.ui_token_amount else { continue };
            let Ok(post_amt) = ui.amount.parse::<i128>() else { continue };
            let key = (b.account_index, b.mint.as_str());
            let (pre_amt, _) = pre.get(&key).copied().unwrap_or((0, ui.decimals));
            let delta = post_amt - pre_amt;
            if delta != 0 {
                deltas.push(decode::TokenBalanceDelta {
                    mint: b.mint.clone(),
                    decimals: ui.decimals,
                    delta_base_units: delta,
                });
            }
        }
        // Accounts closed mid-tx (in pre, absent from post).
        for b in &meta.pre_token_balances {
            let seen_post = meta
                .post_token_balances
                .iter()
                .any(|p| p.account_index == b.account_index && p.mint == b.mint);
            if seen_post {
                continue;
            }
            if let Some(ui) = &b.ui_token_amount {
                if let Ok(amt) = ui.amount.parse::<i128>() {
                    if amt != 0 {
                        deltas.push(decode::TokenBalanceDelta {
                            mint: b.mint.clone(),
                            decimals: ui.decimals,
                            delta_base_units: -amt,
                        });
                    }
                }
            }
        }
        let Some((mint_str, price)) = decode::raydium_swap_price(&deltas) else {
            return Ok(None);
        };
        // Relevance filter: pump mints or pools seen this session only.
        let Ok(mint_pk) = mint_str.parse::<Pubkey>() else { return Ok(None) };
        if !decode::is_pump_mint(&mint_pk) && !self.pool_mints.contains(&mint_pk) {
            return Ok(None);
        }
        let event_id = format!("{signature}:{mint_str}:swap");
        if !self.seen.insert(event_id.clone()) {
            return Ok(None);
        }
        // Dev-dump attribution from the cached creator only (no RPC here).
        // userSourceOwner sits at accounts[17] of a direct swap_base_in/out.
        let raydium = decode::raydium_program_id();
        let mut swapper: Option<Pubkey> = None;
        for (pid_idx, accounts, data) in ixs {
            if account_keys.get(*pid_idx as usize).copied() != Some(raydium) {
                continue;
            }
            match data.first() {
                Some(d)
                    if *d == decode::RAYDIUM_DISC_SWAP_BASE_IN
                        || *d == decode::RAYDIUM_DISC_SWAP_BASE_OUT =>
                {
                    if accounts.len() > 17 {
                        swapper = account_keys.get(accounts[17] as usize).copied();
                    }
                    break;
                }
                _ => {}
            }
        }
        let creator = self.creator_cache.get(&mint_pk).copied();
        let kind = match (swapper, creator) {
            (Some(u), Some(c)) if u == c => EventKind::CreatorSell,
            _ => EventKind::Trade,
        };
        Ok(Some(ChainEvent {
            event_id,
            slot,
            signature: signature.to_string(),
            program_id: decode::RAYDIUM_AMM_PROGRAM_ID_STR.to_string(),
            kind,
            mint: Some(mint_str),
            creator: creator.map(|c| c.to_string()),
            user: swapper.map(|u| u.to_string()),
            creator_buy_sol: None,
            trade_price_sol_per_token: Some(price),
            virtual_sol_reserves: None,
            virtual_token_reserves: None,
            fee_basis_points: None,
            creator_fee_basis_points: None,
            observed_at_ms: now_ms(),
        }))
    }
}

/// Fetch raw account data via plain RPC (base64). Free function so callers
/// don't hold `&YellowstoneSource` across an await (`GeyserStream` is not `Sync`).
///
/// Builds a fresh HTTP client per call so rotating egress-proxy credentials
/// are always picked up from the environment. A cached `Client` would keep
/// using stale proxy auth after rotation and fail every request.
pub(crate) async fn fetch_account_data(
    rpc_url: &str,
    pubkey: &Pubkey,
) -> Result<Option<Vec<u8>>> {
    // Build a fresh client per call with explicit proxy from env.
    // (reqwest was compiled with default-features=false, so automatic
    // proxy env detection is disabled; we configure it manually.)
    // This also picks up rotated proxy credentials.
    let mut builder = Client::builder();
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
    let client = builder.build().context("building RPC HTTP client")?;
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getAccountInfo",
        "params": [pubkey.to_string(), {"encoding": "base64", "commitment": "confirmed"}],
    });
    let resp: serde_json::Value = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context("getAccountInfo request")?
        .error_for_status()
        .context("getAccountInfo http error")?
        .json()
        .await
        .context("parsing getAccountInfo response")?;
    let data = resp.pointer("/result/value/data/0").and_then(|v| v.as_str());
    match data {
        Some(b64) => Ok(Some(B64.decode(b64).context("decoding account data")?)),
        None => Ok(None),
    }
}

#[async_trait]
impl EventSource for YellowstoneSource {
    async fn next_event(&mut self) -> Result<Option<ChainEvent>> {
        while let Some(msg) = self.stream.next().await {
            let update = msg.context("yellowstone stream error")?;
            let Some(UpdateOneof::Transaction(tx_update)) = update.update_oneof else {
                continue;
            };
            let (Some(tx_info), slot) = (tx_update.transaction, tx_update.slot) else {
                continue;
            };
            // Borrow meta: `process_raydium` also needs `&tx_info`.
            let Some(meta) = tx_info.meta.as_ref() else {
                continue;
            };
            // Signature is raw 64 bytes; present as bs58 like explorers do.
            let signature = bs58::encode(&tx_info.signature).into_string();
            // Per-transaction processing must never kill the stream: an RPC
            // blip (e.g. getAccountInfo failing) is logged and skipped, not
            // propagated. Only a broken stream itself ends the loop.
            match self.process_logs(slot, &signature, &meta.log_messages).await {
                Ok(Some(event)) => return Ok(Some(event)),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(%signature, error = %e, "skipping transaction after processing error");
                }
            }
            // Raydium pool creations (migration entries) + swap price feed.
            // `tx_info.meta` was moved into `meta` above; `tx_info.tx` and
            // `tx_info.signature` are still accessible (partial move).
            match self.process_raydium(slot, &signature, &tx_info, &meta).await {
                Ok(Some(event)) => return Ok(Some(event)),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(%signature, error = %e, "skipping transaction after raydium processing error");
                }
            }
            // PumpSwap pool creations (the venue Pump.fun migrations actually
            // land on) + swap price feed.
            match self.process_pumpswap(slot, &signature, &tx_info, &meta).await {
                Ok(Some(event)) => return Ok(Some(event)),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(%signature, error = %e, "skipping transaction after pumpswap processing error");
                }
            }
        }
        Ok(None)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn mask_endpoint(endpoint: &str) -> String {
    // Never log tokens that might be embedded in URLs.
    if let Some(at) = endpoint.find('@') {
        format!("***{}", &endpoint[at..])
    } else {
        endpoint.to_string()
    }
}
