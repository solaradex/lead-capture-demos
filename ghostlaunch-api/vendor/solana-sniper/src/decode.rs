//! Pump.fun program decoding.
//!
//! Program: 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P (bonding-curve program).
//! Discriminators and layouts from the pump-public-docs IDL (Oct 2026).
//! NOTE: v2/v3 buy/sell variants (USDC quotes, May 2026) use different
//! discriminators — the canonical decoder below covers the base variants.
//! Extend `INSTRUCTION_DISCRIMINATORS` if v2/v3 coverage is needed.

use anyhow::{anyhow, Result};
use solana_pubkey::Pubkey;
use std::str::FromStr;

pub const PUMP_AMM_PROGRAM_ID_STR: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

pub fn pump_program_id() -> Pubkey {
    Pubkey::from_str(PUMP_AMM_PROGRAM_ID_STR).expect("hardcoded pump program id")
}

/// 8-byte Anchor discriminators.
pub const DISC_SELL: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad]; // 33e685a4017f83ad
pub const DISC_TRADE_EVENT: [u8; 8] = [0xbd, 0xdb, 0x7f, 0xd3, 0x4e, 0xe6, 0x61, 0xee]; // bddb7fd34ee661ee

/// Decoded Pump.fun TradeEvent (emitted as base64 program log).
///
/// Field order follows the pump-public-docs IDL TradeEvent struct
/// (verified Oct 2026). Extended fields (reserves, fee bps) are 0 when the
/// payload predates them.
#[derive(Debug, Clone)]
pub struct TradeEvent {
    pub mint: Pubkey,
    pub sol_amount_lamports: u64,
    pub token_amount: u64,
    pub is_buy: bool,
    pub user: Pubkey,
    pub timestamp: i64,
    /// Token creator, when the payload carries the extended fields.
    /// This is the same creator the bonding-curve account used to expose;
    /// prefer it over the (currently broken on mainnet) RPC lookup.
    pub creator: Option<Pubkey>,
    /// Virtual SOL reserves right after this trade (lamports).
    pub virtual_sol_reserves: u64,
    /// Virtual token reserves right after this trade (base units).
    pub virtual_token_reserves: u64,
    /// Protocol fee charged on this trade, basis points.
    pub fee_basis_points: u64,
    /// Creator fee charged on this trade, basis points.
    pub creator_fee_basis_points: u64,
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn read<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self.pos.checked_add(N).ok_or_else(|| anyhow!("cursor overflow"))?;
        if end > self.data.len() {
            return Err(anyhow!("short buffer: need {N} bytes at {}", self.pos));
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.data[self.pos..end]);
        self.pos = end;
        Ok(out)
    }
    fn u64_le(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.read::<8>()?))
    }
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
    fn i64_le(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.read::<8>()?))
    }
    fn pubkey(&mut self) -> Result<Pubkey> {
        Ok(Pubkey::new_from_array(self.read::<32>()?))
    }
    fn bool8(&mut self) -> Result<bool> {
        match self.read::<1>()? {
            [0] => Ok(false),
            [1] => Ok(true),
            [b] => Err(anyhow!("invalid bool byte {b}")),
        }
    }
}

/// Parse a base64-decoded program-log payload into a TradeEvent.
/// Returns Ok(None) when the discriminator is not a TradeEvent.
pub fn parse_trade_event_log(payload: &[u8]) -> Result<Option<TradeEvent>> {
    if payload.len() < 8 || payload[..8] != DISC_TRADE_EVENT {
        return Ok(None);
    }
    let mut c = Cursor::new(&payload[8..]);
    let mint = c.pubkey()?;
    let sol_amount_lamports = c.u64_le()?;
    let token_amount = c.u64_le()?;
    let is_buy = c.bool8()?;
    let user = c.pubkey()?;
    let timestamp = c.i64_le()?;
    // Extended fields, in IDL order: virtual_sol_reserves, virtual_token_reserves,
    // real_sol_reserves, real_token_reserves (4 x u64), fee_recipient (32),
    // fee_basis_points (u64), fee (u64), creator (32), creator_fee_basis_points
    // (u64). Short/older payloads leave them at 0.
    let mut virtual_sol_reserves = 0u64;
    let mut virtual_token_reserves = 0u64;
    let mut fee_basis_points = 0u64;
    let mut creator_fee_basis_points = 0u64;
    let mut creator = None;
    if c.remaining() >= 8 * 4 + 32 + 8 + 8 + 32 + 8 {
        virtual_sol_reserves = c.u64_le()?;
        virtual_token_reserves = c.u64_le()?;
        let _real_sol_reserves = c.u64_le()?;
        let _real_token_reserves = c.u64_le()?;
        let _fee_recipient = c.pubkey()?;
        fee_basis_points = c.u64_le()?;
        let _fee = c.u64_le()?;
        creator = Some(c.pubkey()?);
        creator_fee_basis_points = c.u64_le()?;
    }
    Ok(Some(TradeEvent {
        mint,
        sol_amount_lamports,
        token_amount,
        is_buy,
        user,
        timestamp,
        creator,
        virtual_sol_reserves,
        virtual_token_reserves,
        fee_basis_points,
        creator_fee_basis_points,
    }))
}

/// Bonding-curve account layout (pump-public-docs IDL):
/// discriminator(8) + virtual_token_reserves(8) + virtual_sol_reserves(8) +
/// real_token_reserves(8) + real_sol_reserves(8) + token_total_supply(8) +
/// complete(1) + creator(32) + ...
pub const BONDING_CURVE_CREATOR_OFFSET: usize = 8 + 8 * 5 + 1;

/// Derive the bonding-curve PDA for a mint: seeds ["bonding-curve", mint].
pub fn bonding_curve_pda(mint: &Pubkey) -> Pubkey {
    let (pda, _bump) =
        Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &pump_program_id());
    pda
}

/// Extract the creator pubkey from raw bonding-curve account data.
pub fn bonding_curve_creator(account_data: &[u8]) -> Result<Pubkey> {
    let end = BONDING_CURVE_CREATOR_OFFSET + 32;
    if account_data.len() < end {
        return Err(anyhow!(
            "bonding curve account too short: {} < {end}",
            account_data.len()
        ));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&account_data[BONDING_CURVE_CREATOR_OFFSET..end]);
    Ok(Pubkey::new_from_array(arr))
}

/// Offset of `complete: bool` in the bonding-curve account (per IDL):
/// discriminator(8) + 5*u64 reserves/supply(40) = 48.
pub const BONDING_CURVE_COMPLETE_OFFSET: usize = 8 + 8 * 5;

/// Pump Global account layout (pump-public-docs IDL, verified Oct 2026):
/// discriminator(8) + initialized:bool(1) + authority:pubkey(32) +
/// fee_recipient:pubkey(32) + ...
pub const GLOBAL_FEE_RECIPIENT_OFFSET: usize = 8 + 1 + 32;

/// Derive the Pump Global PDA: seeds ["global"].
pub fn global_pda() -> Pubkey {
    let (pda, _bump) =
        Pubkey::find_program_address(&[b"global"], &pump_program_id());
    pda
}

/// Extract the protocol fee recipient from raw Global account data.
pub fn global_fee_recipient(account_data: &[u8]) -> Result<Pubkey> {
    let end = GLOBAL_FEE_RECIPIENT_OFFSET + 32;
    if account_data.len() < end {
        return Err(anyhow!(
            "global account too short: {} < {end}",
            account_data.len()
        ));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&account_data[GLOBAL_FEE_RECIPIENT_OFFSET..end]);
    Ok(Pubkey::new_from_array(arr))
}

/// True when the bonding curve has completed (token migrated off the curve).
/// Never buy when complete.
pub fn bonding_curve_complete(account_data: &[u8]) -> Result<bool> {
    if account_data.len() <= BONDING_CURVE_COMPLETE_OFFSET {
        return Err(anyhow!(
            "bonding curve account too short: {}",
            account_data.len()
        ));
    }
    match account_data[BONDING_CURVE_COMPLETE_OFFSET] {
        0 => Ok(false),
        1 => Ok(true),
        b => Err(anyhow!("invalid complete byte {b}")),
    }
}

/// Authorities parsed from a raw SPL Token / Token-2022 mint account.
/// Base layout (identical prefix for both programs):
/// mint_authority: COption<Pubkey> @0, supply: u64 @36, decimals: u8 @44,
/// is_initialized: u8 @45, freeze_authority: COption<Pubkey> @46.
#[derive(Debug, Clone, Default)]
pub struct MintAuthorities {
    pub mint_authority: Option<Pubkey>,
    pub freeze_authority: Option<Pubkey>,
}

pub fn parse_mint_authorities(data: &[u8]) -> Result<MintAuthorities> {
    if data.len() < 82 {
        return Err(anyhow!("mint account too short: {}", data.len()));
    }
    let read_coption = |offset: usize| -> Result<Option<Pubkey>> {
        let tag = u32::from_le_bytes(
            data[offset..offset + 4]
                .try_into()
                .map_err(|_| anyhow!("short coption at {offset}"))?,
        );
        match tag {
            0 => Ok(None),
            1 => {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&data[offset + 4..offset + 36]);
                Ok(Some(Pubkey::new_from_array(arr)))
            }
            t => Err(anyhow!("invalid COption tag {t}")),
        }
    };
    Ok(MintAuthorities {
        mint_authority: read_coption(0)?,
        freeze_authority: read_coption(46)?,
    })
}

pub fn lamports_to_sol(lamports: u64) -> f64 {
    lamports as f64 / 1_000_000_000.0
}

// ---------------------------------------------------------------------------
// Raydium AMM v4 decoding.
//
// Program: 675kPX9MHTjS2zt1qfr1NYHuzePMKpxEqxUZ41vY9YQa
// Unlike Pump.fun (Anchor, 8-byte discriminators + base64 program logs),
// Raydium AMM v4 uses single-byte instruction discriminators and emits no
// Anchor events, so pool creations and swaps are decoded from the raw
// transaction instructions (top-level and inner/CPI).
// ---------------------------------------------------------------------------

pub const RAYDIUM_AMM_PROGRAM_ID_STR: &str = "675kPX9MHTjS2zt1qfr1NYHuzePMKpxEqxUZ41vY9YQa";

pub fn raydium_program_id() -> Pubkey {
    Pubkey::from_str(RAYDIUM_AMM_PROGRAM_ID_STR).expect("hardcoded raydium program id")
}

/// Wrapped SOL mint — the quote side of Pump.fun migration pools.
pub const WSOL_MINT_STR: &str = "So11111111111111111111111111111111111111112";

/// Single-byte Raydium AMM v4 instruction discriminators.
pub const RAYDIUM_DISC_INITIALIZE: u8 = 0;
pub const RAYDIUM_DISC_INITIALIZE2: u8 = 1;
pub const RAYDIUM_DISC_SWAP_BASE_IN: u8 = 9;
pub const RAYDIUM_DISC_SWAP_BASE_OUT: u8 = 11;

/// Account positions in the initialize / initialize2 instruction, per the
/// Raydium AMM v4 IDL: token_program(0), system_program(1), rent(2),
/// amm(3), amm_authority(4), amm_open_orders(5), lp_mint(6),
/// coin_mint(7), pc_mint(8), ...
pub const RAYDIUM_INIT_POOL_ACCOUNT_IDX: usize = 3;
pub const RAYDIUM_INIT_COIN_MINT_IDX: usize = 7;
pub const RAYDIUM_INIT_PC_MINT_IDX: usize = 8;

/// A decoded Raydium pool creation.
#[derive(Debug, Clone)]
pub struct RaydiumPoolInit {
    /// The AMM pool account (accounts[3]).
    pub pool_address: Pubkey,
    /// Base asset mint (accounts[7]) — the migrated token for pump pools.
    pub coin_mint: Pubkey,
    /// Quote asset mint (accounts[8]) — wSOL for pump migrations.
    pub pc_mint: Pubkey,
    /// Initial base-side deposit, base units (initialize2 only).
    pub init_coin_amount: Option<u64>,
    /// Initial quote-side deposit, lamports (initialize2 only).
    pub init_pc_amount: Option<u64>,
}

/// Parse a Raydium initialize / initialize2 instruction.
///
/// `account_indices` is the instruction's `accounts` byte array (each byte an
/// index into `account_keys`); `account_keys` are the transaction's raw
/// 32-byte account keys. Returns `Ok(None)` when the instruction is not a
/// pool initialization.
pub fn parse_raydium_pool_init(
    program_id: &Pubkey,
    data: &[u8],
    account_indices: &[u8],
    account_keys: &[Pubkey],
) -> Result<Option<RaydiumPoolInit>> {
    if *program_id != raydium_program_id() {
        return Ok(None);
    }
    let disc = match data.first() {
        Some(d) => *d,
        None => return Ok(None),
    };
    if disc != RAYDIUM_DISC_INITIALIZE && disc != RAYDIUM_DISC_INITIALIZE2 {
        return Ok(None);
    }
    // Need at least the 9 leading accounts (through pc_mint).
    if account_indices.len() <= RAYDIUM_INIT_PC_MINT_IDX {
        return Ok(None);
    }
    let key_at = |pos: usize| -> Result<Pubkey> {
        let ki = account_indices[pos] as usize;
        account_keys
            .get(ki)
            .copied()
            .ok_or_else(|| anyhow!("raydium init account index {ki} out of bounds"))
    };
    let pool_address = key_at(RAYDIUM_INIT_POOL_ACCOUNT_IDX)?;
    let coin_mint = key_at(RAYDIUM_INIT_COIN_MINT_IDX)?;
    let pc_mint = key_at(RAYDIUM_INIT_PC_MINT_IDX)?;

    // initialize2 layout: disc(1) + nonce(1) + open_time(8) +
    // init_pc_amount(8) + init_coin_amount(8). Plain initialize carries no
    // amounts (they come from the token accounts instead).
    let (init_coin_amount, init_pc_amount) = if disc == RAYDIUM_DISC_INITIALIZE2 && data.len() >= 26
    {
        let init_pc_amount = u64::from_le_bytes(data[10..18].try_into().unwrap());
        let init_coin_amount = u64::from_le_bytes(data[18..26].try_into().unwrap());
        (Some(init_coin_amount), Some(init_pc_amount))
    } else {
        (None, None)
    };
    Ok(Some(RaydiumPoolInit {
        pool_address,
        coin_mint,
        pc_mint,
        init_coin_amount,
        init_pc_amount,
    }))
}

/// True when the mint is a Pump.fun token (vanity suffix "pump").
pub fn is_pump_mint(mint: &Pubkey) -> bool {
    mint.to_string().ends_with("pump")
}

/// One account's token balance change within a transaction.
#[derive(Debug, Clone)]
pub struct TokenBalanceDelta {
    pub mint: String,
    pub decimals: u32,
    /// Signed change in base units (post - pre).
    pub delta_base_units: i128,
}

/// Compute the execution price (SOL per token) of a Raydium swap from the
/// transaction's pre/post token balances.
///
/// Finds the two accounts with the largest absolute balance changes — one
/// should be the traded token, the other wSOL — and returns the price
/// implied by their ratio. Returns `None` when no clean two-sided swap is
/// visible (multi-hop, arbitrage, or dust-only changes).
pub fn raydium_swap_price(deltas: &[TokenBalanceDelta]) -> Option<(String, f64)> {
    // Largest absolute mover per mint (an account-level view; a swap moves
    // exactly two vaults/user accounts by the trade size).
    let mut best_token: Option<(&TokenBalanceDelta, i128)> = None;
    let mut best_wsol: Option<i128> = None;
    for d in deltas {
        let abs = d.delta_base_units.abs();
        if abs == 0 {
            continue;
        }
        if d.mint == WSOL_MINT_STR {
            if best_wsol.map(|b| abs > b).unwrap_or(true) {
                best_wsol = Some(abs);
            }
        } else if best_token.map(|(_, b)| abs > b).unwrap_or(true) {
            best_token = Some((d, abs));
        }
    }
    let (tok, tok_abs) = best_token?;
    let wsol_abs = best_wsol?;
    if tok_abs == 0 || wsol_abs == 0 {
        return None;
    }
    let tok_units = tok_abs as f64 / 10f64.powi(tok.decimals as i32);
    let wsol_sol = wsol_abs as f64 / 1_000_000_000.0;
    if !tok_units.is_finite() || tok_units <= 0.0 || !wsol_sol.is_finite() {
        return None;
    }
    Some((tok.mint.clone(), wsol_sol / tok_units))
}

// ---------------------------------------------------------------------------
// PumpSwap AMM decoding.
//
// Program: pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA
// Pump.fun's own constant-product AMM. Graduated Pump.fun tokens migrate
// here — Raydium migration was the older behavior, so a migration watcher
// pointed at Raydium AMM v4 sees nothing. Anchor program: 8-byte
// discriminators, verified against the pump_amm v0.1.0 IDL and cross-checked
// locally as sha256("global:<name>")[..8].
// ---------------------------------------------------------------------------

pub const PUMPSWAP_AMM_PROGRAM_ID_STR: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";

pub fn pumpswap_program_id() -> Pubkey {
    Pubkey::from_str(PUMPSWAP_AMM_PROGRAM_ID_STR).expect("hardcoded pumpswap program id")
}

/// Anchor discriminators for the PumpSwap instructions we track.
pub const PUMPSWAP_DISC_CREATE_POOL: [u8; 8] = [233, 146, 209, 142, 207, 104, 64, 188]; // e992d18ecf6840bc
pub const PUMPSWAP_DISC_BUY: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234]; // 66063d1201daebea
pub const PUMPSWAP_DISC_SELL: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173]; // 33e685a4017f83ad

/// Account positions in the create_pool instruction, per the pump_amm IDL:
/// pool(0), global_config(1), creator(2), base_mint(3), quote_mint(4),
/// lp_mint(5), ...
pub const PUMPSWAP_CREATE_POOL_ACCOUNT_IDX: usize = 0;
pub const PUMPSWAP_CREATE_BASE_MINT_IDX: usize = 3;
pub const PUMPSWAP_CREATE_QUOTE_MINT_IDX: usize = 4;

/// Account position of `user` in the buy / sell instructions, per the
/// pump_amm IDL: pool(0), user(1), ... Used for dev-dump attribution.
pub const PUMPSWAP_SWAP_USER_IDX: usize = 1;

/// A decoded PumpSwap pool creation.
#[derive(Debug, Clone)]
pub struct PumpSwapPoolInit {
    /// The pool PDA (accounts[0]).
    pub pool_address: Pubkey,
    /// Base asset mint (accounts[3]) — the migrated token for pump pools.
    pub base_mint: Pubkey,
    /// Quote asset mint (accounts[4]) — wSOL for pump migrations.
    pub quote_mint: Pubkey,
    /// Pool index (u16 arg). Pump-created migration pools use index 0.
    pub index: u16,
    /// Initial base-side deposit, base units.
    pub base_amount_in: u64,
    /// Initial quote-side deposit, lamports.
    pub quote_amount_in: u64,
    /// Token creator carried in the create_pool instruction args
    /// (offset 26, 32 bytes). Always present on Pump.fun migration pools;
    /// None when the instruction data is truncated (older clients).
    /// This is the zero-RPC replacement for the bonding-curve creator
    /// lookup, whose on-chain layout changed and no longer exposes the
    /// creator at the legacy offset.
    pub coin_creator: Option<Pubkey>,
}

/// Parse a PumpSwap create_pool instruction.
///
/// `account_indices` is the instruction's `accounts` byte array (each byte an
/// index into `account_keys`); `account_keys` are the transaction's raw
/// 32-byte account keys. Returns `Ok(None)` when the instruction is not a
/// pool creation.
pub fn parse_pumpswap_pool_init(
    program_id: &Pubkey,
    data: &[u8],
    account_indices: &[u8],
    account_keys: &[Pubkey],
) -> Result<Option<PumpSwapPoolInit>> {
    if *program_id != pumpswap_program_id() {
        return Ok(None);
    }
    if data.len() < 8 || data[..8] != PUMPSWAP_DISC_CREATE_POOL {
        return Ok(None);
    }
    // Need at least the 5 leading accounts (through quote_mint).
    if account_indices.len() <= PUMPSWAP_CREATE_QUOTE_MINT_IDX {
        return Ok(None);
    }
    let key_at = |pos: usize| -> Result<Pubkey> {
        let ki = account_indices[pos] as usize;
        account_keys
            .get(ki)
            .copied()
            .ok_or_else(|| anyhow!("pumpswap create_pool account index {ki} out of bounds"))
    };
    let pool_address = key_at(PUMPSWAP_CREATE_POOL_ACCOUNT_IDX)?;
    let base_mint = key_at(PUMPSWAP_CREATE_BASE_MINT_IDX)?;
    let quote_mint = key_at(PUMPSWAP_CREATE_QUOTE_MINT_IDX)?;

    // Args layout: discriminator(8) + index:u16(2) + base_amount_in:u64(8) +
    // quote_amount_in:u64(8) + coin_creator:pubkey(32) + is_mayhem_mode:bool(1).
    if data.len() < 8 + 2 + 8 + 8 {
        return Ok(None);
    }
    let index = u16::from_le_bytes(data[8..10].try_into().unwrap());
    let base_amount_in = u64::from_le_bytes(data[10..18].try_into().unwrap());
    let quote_amount_in = u64::from_le_bytes(data[18..26].try_into().unwrap());
    // coin_creator:pubkey(32) follows the deposit amounts; short payloads
    // (older clients) omit it and the trailing mayhem flag.
    let coin_creator = if data.len() >= 8 + 2 + 8 + 8 + 32 {
        Some(Pubkey::new_from_array(data[26..58].try_into().unwrap()))
    } else {
        None
    };
    Ok(Some(PumpSwapPoolInit {
        pool_address,
        base_mint,
        quote_mint,
        index,
        base_amount_in,
        quote_amount_in,
        coin_creator,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_trade_event_bytes() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&DISC_TRADE_EVENT);
        v.extend_from_slice(&[7u8; 32]); // mint
        v.extend_from_slice(&700_000_000u64.to_le_bytes()); // 0.7 SOL
        v.extend_from_slice(&1_000_000u64.to_le_bytes()); // token amount
        v.push(1); // is_buy = true
        v.extend_from_slice(&[9u8; 32]); // user
        v.extend_from_slice(&1_700_000_000i64.to_le_bytes()); // timestamp
        v.extend_from_slice(&[0u8; 32]); // reserves (unused)
        v
    }

    #[test]
    fn decodes_trade_event() {
        let ev = parse_trade_event_log(&sample_trade_event_bytes())
            .unwrap()
            .expect("should decode");
        assert!(ev.is_buy);
        assert_eq!(ev.sol_amount_lamports, 700_000_000);
        assert_eq!(ev.mint, Pubkey::new_from_array([7u8; 32]));
        assert_eq!(ev.user, Pubkey::new_from_array([9u8; 32]));
    }

    #[test]
    fn rejects_wrong_discriminator() {
        let mut v = sample_trade_event_bytes();
        v[0] = 0xff;
        assert!(parse_trade_event_log(&v).unwrap().is_none());
    }

    fn full_trade_event_bytes() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&DISC_TRADE_EVENT);
        v.extend_from_slice(&[7u8; 32]); // mint
        v.extend_from_slice(&700_000_000u64.to_le_bytes()); // 0.7 SOL
        v.extend_from_slice(&1_000_000u64.to_le_bytes()); // token amount
        v.push(1); // is_buy = true
        v.extend_from_slice(&[9u8; 32]); // user
        v.extend_from_slice(&1_700_000_000i64.to_le_bytes()); // timestamp
        v.extend_from_slice(&30_000_000_000u64.to_le_bytes()); // virtual_sol_reserves
        v.extend_from_slice(&1_073_000_000_000_000u64.to_le_bytes()); // virtual_token_reserves
        v.extend_from_slice(&7_500_000_000u64.to_le_bytes()); // real_sol_reserves
        v.extend_from_slice(&800_000_000_000_000u64.to_le_bytes()); // real_token_reserves
        v.extend_from_slice(&[5u8; 32]); // fee_recipient
        v.extend_from_slice(&100u64.to_le_bytes()); // fee_basis_points
        v.extend_from_slice(&7_000_000u64.to_le_bytes()); // fee
        v.extend_from_slice(&[6u8; 32]); // creator
        v.extend_from_slice(&50u64.to_le_bytes()); // creator_fee_basis_points
        v
    }

    #[test]
    fn decodes_extended_trade_event_fields() {
        let ev = parse_trade_event_log(&full_trade_event_bytes())
            .unwrap()
            .expect("should decode");
        assert_eq!(ev.virtual_sol_reserves, 30_000_000_000);
        assert_eq!(ev.virtual_token_reserves, 1_073_000_000_000_000);
        assert_eq!(ev.fee_basis_points, 100);
        assert_eq!(ev.creator_fee_basis_points, 50);
        assert_eq!(ev.creator, Some(Pubkey::new_from_array([6u8; 32])));
    }

    #[test]
    fn short_payload_leaves_extended_fields_zero() {
        let ev = parse_trade_event_log(&sample_trade_event_bytes())
            .unwrap()
            .expect("should decode");
        assert_eq!(ev.virtual_sol_reserves, 0);
        assert_eq!(ev.virtual_token_reserves, 0);
        assert_eq!(ev.fee_basis_points, 0);
        assert_eq!(ev.creator_fee_basis_points, 0);
        assert_eq!(ev.creator, None);
    }

    #[test]
    fn creator_offset_math() {
        assert_eq!(BONDING_CURVE_CREATOR_OFFSET, 49);
        // discriminator(8) + 5*u64(40) + complete bool(1) = 49
    }

    // ---- Raydium decoding ----

    fn raydium_keys() -> Vec<Pubkey> {
        // 20 deterministic keys; index 7/8 hold the mints, 3 the pool.
        (0u8..20).map(|b| Pubkey::new_from_array([b; 32])).collect()
    }

    fn init2_data() -> Vec<u8> {
        let mut v = vec![RAYDIUM_DISC_INITIALIZE2, 255u8]; // disc + nonce
        v.extend_from_slice(&0u64.to_le_bytes()); // open_time
        v.extend_from_slice(&85_000_000_000u64.to_le_bytes()); // init_pc_amount (85 SOL)
        v.extend_from_slice(&1_000_000_000_000_000u64.to_le_bytes()); // init_coin_amount
        v
    }

    #[test]
    fn decodes_raydium_initialize2() {
        let keys = raydium_keys();
        let indices: Vec<u8> = (0u8..20).collect();
        let init = parse_raydium_pool_init(
            &raydium_program_id(),
            &init2_data(),
            &indices,
            &keys,
        )
        .unwrap()
        .expect("should decode");
        assert_eq!(init.pool_address, keys[3]);
        assert_eq!(init.coin_mint, keys[7]);
        assert_eq!(init.pc_mint, keys[8]);
        assert_eq!(init.init_pc_amount, Some(85_000_000_000));
        assert_eq!(init.init_coin_amount, Some(1_000_000_000_000_000));
    }

    #[test]
    fn rejects_non_raydium_program() {
        let keys = raydium_keys();
        let indices: Vec<u8> = (0u8..20).collect();
        let other = Pubkey::new_from_array([99u8; 32]);
        assert!(parse_raydium_pool_init(&other, &init2_data(), &indices, &keys)
            .unwrap()
            .is_none());
    }

    #[test]
    fn rejects_swap_discriminator() {
        let keys = raydium_keys();
        let indices: Vec<u8> = (0u8..20).collect();
        let mut data = init2_data();
        data[0] = RAYDIUM_DISC_SWAP_BASE_IN;
        assert!(parse_raydium_pool_init(&raydium_program_id(), &data, &indices, &keys)
            .unwrap()
            .is_none());
    }

    #[test]
    fn rejects_short_account_list() {
        let keys = raydium_keys();
        let indices: Vec<u8> = (0u8..5).collect(); // too few for mints
        assert!(parse_raydium_pool_init(
            &raydium_program_id(),
            &init2_data(),
            &indices,
            &keys
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn pump_mint_suffix_check() {
        // A real pump mint ends in "pump"; construct one via base58 round-trip.
        let pump = Pubkey::from_str(
            "6qUw38wQ4TF8DGnpmM9jGydEQWXmgquHtmK3op5gpump",
        )
        .unwrap();
        assert!(is_pump_mint(&pump));
        assert!(!is_pump_mint(&Pubkey::new_from_array([7u8; 32])));
        assert!(!is_pump_mint(&raydium_program_id()));
    }

    #[test]
    fn swap_price_from_balance_deltas() {
        // User buys 1M tokens (6 decimals) for 0.5 SOL.
        let deltas = vec![
            TokenBalanceDelta {
                mint: WSOL_MINT_STR.to_string(),
                decimals: 9,
                delta_base_units: 500_000_000, // pool pc vault +0.5 SOL
            },
            TokenBalanceDelta {
                mint: "Tok111111111111111111111111111111111111111".to_string(),
                decimals: 6,
                delta_base_units: -(1_000_000_000_000), // pool coin vault -1M tokens
            },
        ];
        let (mint, price) = raydium_swap_price(&deltas).unwrap();
        assert_eq!(mint, "Tok111111111111111111111111111111111111111");
        assert!((price - 0.5 / 1_000_000.0).abs() < 1e-12, "price was {price}");
    }

    #[test]
    fn swap_price_needs_both_sides() {
        let deltas = vec![TokenBalanceDelta {
            mint: "Tok111111111111111111111111111111111111111".to_string(),
            decimals: 6,
            delta_base_units: 1_000_000,
        }];
        assert!(raydium_swap_price(&deltas).is_none());
        assert!(raydium_swap_price(&[]).is_none());
    }

    // ---- PumpSwap decoding ----

    fn pumpswap_keys() -> Vec<Pubkey> {
        // 20 deterministic keys; index 0 is the pool, 3/4 the mints.
        (0u8..20).map(|b| Pubkey::new_from_array([b; 32])).collect()
    }

    fn create_pool_data() -> Vec<u8> {
        let mut v = Vec::from(PUMPSWAP_DISC_CREATE_POOL);
        v.extend_from_slice(&0u16.to_le_bytes()); // index 0: canonical migration pool
        v.extend_from_slice(&206_900_000_000_000u64.to_le_bytes()); // base_amount_in
        v.extend_from_slice(&85_000_000_000u64.to_le_bytes()); // quote_amount_in (85 SOL)
        v.extend_from_slice(&[7u8; 32]); // coin_creator
        v.push(0); // is_mayhem_mode = false
        v
    }

    #[test]
    fn decodes_pumpswap_create_pool() {
        let keys = pumpswap_keys();
        let indices: Vec<u8> = (0u8..20).collect();
        let init = parse_pumpswap_pool_init(
            &pumpswap_program_id(),
            &create_pool_data(),
            &indices,
            &keys,
        )
        .unwrap()
        .expect("should decode");
        assert_eq!(init.pool_address, keys[0]);
        assert_eq!(init.base_mint, keys[3]);
        assert_eq!(init.quote_mint, keys[4]);
        assert_eq!(init.index, 0);
        assert_eq!(init.base_amount_in, 206_900_000_000_000);
        assert_eq!(init.quote_amount_in, 85_000_000_000);
        assert_eq!(
            init.coin_creator,
            Some(Pubkey::new_from_array([7u8; 32]))
        );
    }

    #[test]
    fn coin_creator_absent_on_truncated_args() {
        // Older clients may send create_pool without the trailing
        // coin_creator / mayhem flag: decode must succeed with None.
        let keys = pumpswap_keys();
        let indices: Vec<u8> = (0u8..20).collect();
        let mut data = Vec::from(PUMPSWAP_DISC_CREATE_POOL);
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&206_900_000_000_000u64.to_le_bytes());
        data.extend_from_slice(&85_000_000_000u64.to_le_bytes());
        assert_eq!(data.len(), 26);
        let init = parse_pumpswap_pool_init(&pumpswap_program_id(), &data, &indices, &keys)
            .unwrap()
            .expect("should decode");
        assert_eq!(init.coin_creator, None);
        assert_eq!(init.quote_amount_in, 85_000_000_000);
    }

    #[test]
    fn rejects_non_pumpswap_program() {
        let keys = pumpswap_keys();
        let indices: Vec<u8> = (0u8..20).collect();
        let other = Pubkey::new_from_array([99u8; 32]);
        assert!(parse_pumpswap_pool_init(&other, &create_pool_data(), &indices, &keys)
            .unwrap()
            .is_none());
    }

    #[test]
    fn rejects_buy_discriminator() {
        let keys = pumpswap_keys();
        let indices: Vec<u8> = (0u8..20).collect();
        let mut data = create_pool_data();
        data[..8].copy_from_slice(&PUMPSWAP_DISC_BUY);
        assert!(parse_pumpswap_pool_init(&pumpswap_program_id(), &data, &indices, &keys)
            .unwrap()
            .is_none());
    }

    #[test]
    fn rejects_short_account_list_pumpswap() {
        let keys = pumpswap_keys();
        let indices: Vec<u8> = (0u8..4).collect(); // too few for quote_mint
        assert!(parse_pumpswap_pool_init(
            &pumpswap_program_id(),
            &create_pool_data(),
            &indices,
            &keys,
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn rejects_truncated_args() {
        let keys = pumpswap_keys();
        let indices: Vec<u8> = (0u8..20).collect();
        let data = Vec::from(PUMPSWAP_DISC_CREATE_POOL); // discriminator only
        assert!(parse_pumpswap_pool_init(&pumpswap_program_id(), &data, &indices, &keys)
            .unwrap()
            .is_none());
    }

    #[test]
    fn pumpswap_discriminators_match_anchor_scheme() {
        // Cross-checked as sha256("global:<name>")[..8], independently of the
        // IDL. e992d18ecf6840bc / 66063d1201daebea / 33e685a4017f83ad are the
        // expected hex digests.
        assert_eq!(PUMPSWAP_DISC_CREATE_POOL, [0xe9, 0x92, 0xd1, 0x8e, 0xcf, 0x68, 0x40, 0xbc]);
        assert_eq!(PUMPSWAP_DISC_BUY, [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea]);
        assert_eq!(PUMPSWAP_DISC_SELL, [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad]);
    }
}
