//! Pump.fun buy/sell transaction builders.
//!
//! Builds signed, base64-encoded versioned (v0) transactions for the Pump.fun
//! bonding-curve program `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`.
//! Account layouts are from the official pump-public-docs IDL (Oct 2026).
//!
//! Buy arg semantics (`buy_exact_sol_in`, per the official IDL):
//! `spendable_sol_in` = lamports of SOL budget (fees are deducted from it),
//! `min_tokens_out` = slippage floor in token base units. **Verify on devnet
//! before mainnet.**
//! Sell arg semantics: `amount` = token base units to sell, `min_sol_output` =
//! minimum lamports out after slippage.

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::{v0::Message as V0Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

use crate::decode::{self, DISC_SELL};

// ---------------------------------------------------------------------------
// Fixed program IDs
// ---------------------------------------------------------------------------

pub const SYSTEM_PROGRAM_STR: &str = "11111111111111111111111111111111";
pub const ATA_PROGRAM_STR: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
pub const FEE_PROGRAM_STR: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";
/// Legacy SPL Token program; new mints may use Token-2022 (resolve per mint).
pub const TOKEN_PROGRAM_STR: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM_STR: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

/// `buy_exact_sol_in` discriminator, verified against the official
/// pump-public-docs IDL (idl/pump.json, main branch, Oct 2026):
/// https://github.com/pump-fun/pump-public-docs
pub const DISC_BUY_EXACT_SOL_IN: [u8; 8] = [56, 252, 116, 8, 158, 223, 205, 95];

fn pid(s: &str) -> Pubkey {
    Pubkey::from_str(s).expect("hardcoded program id")
}

pub fn system_program_id() -> Pubkey {
    pid(SYSTEM_PROGRAM_STR)
}
pub fn ata_program_id() -> Pubkey {
    pid(ATA_PROGRAM_STR)
}
pub fn fee_program_id() -> Pubkey {
    pid(FEE_PROGRAM_STR)
}

// ---------------------------------------------------------------------------
// PDA derivations
// ---------------------------------------------------------------------------

fn pda(seeds: &[&[u8]], program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}

pub fn global_pda() -> Pubkey {
    pda(&[b"global"], &decode::pump_program_id())
}

pub fn event_authority_pda() -> Pubkey {
    pda(&[b"__event_authority"], &decode::pump_program_id())
}

pub fn global_volume_accumulator_pda() -> Pubkey {
    pda(&[b"global_volume_accumulator"], &decode::pump_program_id())
}

pub fn user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
    pda(&[b"user_volume_accumulator", user.as_ref()], &decode::pump_program_id())
}

pub fn creator_vault_pda(creator: &Pubkey) -> Pubkey {
    pda(&[b"creator-vault", creator.as_ref()], &decode::pump_program_id())
}

/// fee_config = ["fee_config", pump_program_id] under the fee program.
pub fn fee_config_pda() -> Pubkey {
    let pump = decode::pump_program_id();
    pda(&[b"fee_config", pump.as_ref()], &fee_program_id())
}

/// Standard ATA derivation: [owner, token_program, mint] under the ATA program.
pub fn ata_pda(owner: &Pubkey, token_program: &Pubkey, mint: &Pubkey) -> Pubkey {
    pda(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program_id(),
    )
}

// ---------------------------------------------------------------------------
// Pump instructions
// ---------------------------------------------------------------------------

/// Accounts needed for a Pump.fun buy/sell.
#[derive(Debug, Clone)]
pub struct PumpAccounts {
    pub fee_recipient: Pubkey,
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub user: Pubkey,
    /// The mint's token program (legacy Token or Token-2022) — resolve per mint.
    pub token_program: Pubkey,
    /// Buyback fee recipient: one of the 8 official Pump buyback wallets
    /// (see pump-public-docs docs/FEE_RECIPIENTS.md). Required on buy
    /// instructions since the buyback upgrade — the program throws
    /// BuybackFeeRecipientMissing (6062) without it. Verified on devnet
    /// 2026-10-19: buy_exact_sol_in takes it as account 17 (writable).
    pub buyback_fee_recipient: Pubkey,
}

/// One of the 8 official Pump buyback fee recipient wallets, for bonding-curve
/// buys. From pump-public-docs docs/FEE_RECIPIENTS.md ("use one of these as
/// buybackFeeRecipient for any coin").
pub fn buyback_wallet() -> Pubkey {
    pid("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD")
}

/// The 16 accounts shared by the legacy buy instructions, in IDL order.
fn buy_accounts(accts: &PumpAccounts) -> Vec<AccountMeta> {
    let pump = decode::pump_program_id();
    let bonding_curve = decode::bonding_curve_pda(&accts.mint);
    vec![
        AccountMeta::new_readonly(global_pda(), false),                       // 1
        AccountMeta::new(accts.fee_recipient, false),                          // 2
        AccountMeta::new_readonly(accts.mint, false),                          // 3
        AccountMeta::new(bonding_curve, false),                                // 4
        AccountMeta::new(ata_pda(&bonding_curve, &accts.token_program, &accts.mint), false), // 5
        AccountMeta::new(ata_pda(&accts.user, &accts.token_program, &accts.mint), false),    // 6
        AccountMeta::new(accts.user, true),                                    // 7
        AccountMeta::new_readonly(system_program_id(), false),                 // 8
        AccountMeta::new_readonly(accts.token_program, false),                 // 9
        AccountMeta::new(creator_vault_pda(&accts.creator), false),             // 10
        AccountMeta::new_readonly(event_authority_pda(), false),               // 11
        AccountMeta::new_readonly(pump, false),                                // 12
        AccountMeta::new(global_volume_accumulator_pda(), false),              // 13
        AccountMeta::new(user_volume_accumulator_pda(&accts.user), false),     // 14
        AccountMeta::new_readonly(fee_config_pda(), false),                    // 15
        AccountMeta::new_readonly(fee_program_id(), false),                    // 16
    ]
}

/// `buy_exact_sol_in`: 16 accounts (legacy). Args: spendable_sol_in (lamports of SOL
/// budget — fees are deducted from it), min_tokens_out (slippage floor),
/// track_volume, partial_fill.
///
/// This is the correct instruction for a "spend X SOL" strategy. (The legacy
/// `buy` takes a *token* amount as its first arg, so passing lamports there
/// sized positions wrong.)
pub fn buy_exact_sol_in_instruction(
    accts: &PumpAccounts,
    spendable_sol_in_lamports: u64,
    min_tokens_out: u64,
    track_volume: bool,
    partial_fill: bool,
) -> Instruction {
    let mut data = Vec::with_capacity(26);
    data.extend_from_slice(&DISC_BUY_EXACT_SOL_IN);
    data.extend_from_slice(&spendable_sol_in_lamports.to_le_bytes());
    data.extend_from_slice(&min_tokens_out.to_le_bytes());
    data.push(u8::from(track_volume));
    data.push(u8::from(partial_fill));
    Instruction {
        program_id: decode::pump_program_id(),
        accounts: buy_accounts(accts),
        data,
    }
}

/// Anchor discriminator for `buy_exact_quote_in_v3`, from the official IDL.
/// This is the current bonding-curve buy: same economics as the legacy
/// `buy_exact_sol_in`, but with the buyback fee recipient the program now
/// requires (BuybackFeeRecipientMissing/6062 on the legacy path).
const DISC_BUY_EXACT_QUOTE_IN_V3: [u8; 8] =
    [0xe1, 0xf7, 0x50, 0x1e, 0xd5, 0xb3, 0x84, 0x88];

/// Wrapped SOL mint, the quote mint for SOL-paired v3 trades.
pub fn wsol_mint() -> Pubkey {
    pid("So11111111111111111111111111111111111111112")
}

/// SPL Token program id (quote side for wSOL).
pub fn spl_token_program() -> Pubkey {
    pid("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
}

/// The 17 accounts for the v3 bonding-curve trade instructions, in IDL order.
/// Per pump-public-docs docs/instructions/TRADE_V3.md. For SOL-paired coins
/// the buyback recipient is one of the 8 official buyback wallets itself
/// (not an ATA); the quote ATAs are derived addresses that need not exist.
fn buy_v3_accounts(accts: &PumpAccounts) -> Vec<AccountMeta> {
    let pump = decode::pump_program_id();
    let bonding_curve = decode::bonding_curve_pda(&accts.mint);
    let quote_mint = wsol_mint();
    let quote_token_program = spl_token_program();
    vec![
        AccountMeta::new_readonly(global_pda(), false),                        // 1 global
        AccountMeta::new_readonly(accts.mint, false),                          // 2 base_mint
        AccountMeta::new_readonly(quote_mint, false),                          // 3 quote_mint (wSOL)
        AccountMeta::new_readonly(accts.token_program, false),                 // 4 base_token_program
        AccountMeta::new_readonly(quote_token_program, false),                 // 5 quote_token_program
        AccountMeta::new(bonding_curve, false),                                // 6 bonding_curve
        AccountMeta::new(ata_pda(&bonding_curve, &accts.token_program, &accts.mint), false), // 7
        AccountMeta::new(ata_pda(&bonding_curve, &quote_token_program, &quote_mint), false),  // 8
        AccountMeta::new(accts.user, true),                                    // 9 user (signer)
        AccountMeta::new(ata_pda(&accts.user, &accts.token_program, &accts.mint), false),     // 10
        AccountMeta::new(ata_pda(&accts.user, &quote_token_program, &quote_mint), false),    // 11
        AccountMeta::new(user_volume_accumulator_pda(&accts.user), false),     // 12
        AccountMeta::new_readonly(fee_config_pda(), false),                    // 13 fee_config
        AccountMeta::new(accts.buyback_fee_recipient, false),                  // 14 buyback_fee_recipient
        AccountMeta::new_readonly(system_program_id(), false),                 // 15 system_program
        AccountMeta::new_readonly(event_authority_pda(), false),               // 16 event_authority
        AccountMeta::new_readonly(pump, false),                                // 17 program
    ]
}

/// `buy_exact_quote_in_v3`: spend an exact SOL budget on a bonding-curve coin.
/// Args: spendable_quote_in (lamports, fees included), min_tokens_out
/// (slippage floor). `partial_fill` omitted per the docs (allowed).
pub fn buy_exact_quote_in_v3_instruction(
    accts: &PumpAccounts,
    spendable_quote_in_lamports: u64,
    min_tokens_out: u64,
) -> Instruction {
    let pump = decode::pump_program_id();
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&DISC_BUY_EXACT_QUOTE_IN_V3);
    data.extend_from_slice(&spendable_quote_in_lamports.to_le_bytes());
    data.extend_from_slice(&min_tokens_out.to_le_bytes());
    Instruction {
        program_id: pump,
        accounts: buy_v3_accounts(accts),
        data,
    }
}

/// Quote the token output of `buy_exact_sol_in`, implementing the formula
/// documented in the official pump-public-docs IDL:
///
/// ```text
/// total_fee_bps = protocol_fee_bps + creator_fee_bps
/// net_sol = floor(spendable_sol_in * 10_000 / (10_000 + total_fee_bps))
/// fees = ceil(net_sol * protocol_fee_bps / 10_000)
///      + ceil(net_sol * creator_fee_bps / 10_000)
/// if net_sol + fees > spendable_sol_in:
///     net_sol = net_sol - (net_sol + fees - spendable_sol_in)
/// tokens_out = floor((net_sol - 1) * virtual_token_reserves
///                    / (virtual_sol_reserves + net_sol - 1))
/// ```
///
/// All integer math with u128 intermediates, so realistic reserve sizes
/// cannot overflow. Returns 0 when the inputs are unusable.
pub fn quote_buy_exact_sol_in(
    virtual_sol_reserves: u64,
    virtual_token_reserves: u64,
    spendable_sol_in_lamports: u64,
    protocol_fee_bps: u64,
    creator_fee_bps: u64,
) -> u64 {
    if virtual_sol_reserves == 0 || virtual_token_reserves == 0 || spendable_sol_in_lamports == 0 {
        return 0;
    }
    let total_fee_bps = protocol_fee_bps as u128 + creator_fee_bps as u128;
    let spendable = spendable_sol_in_lamports as u128;
    let mut net_sol = spendable * 10_000 / (10_000 + total_fee_bps);
    // ceil(net_sol * bps / 10_000) == (net_sol * bps + 9_999) / 10_000
    let fees = (net_sol * protocol_fee_bps as u128 + 9_999) / 10_000
        + (net_sol * creator_fee_bps as u128 + 9_999) / 10_000;
    if net_sol + fees > spendable {
        net_sol = net_sol.saturating_sub(net_sol + fees - spendable);
    }
    if net_sol <= 1 {
        return 0;
    }
    let tokens = (net_sol - 1) * virtual_token_reserves as u128
        / (virtual_sol_reserves as u128 + net_sol - 1);
    tokens.min(u64::MAX as u128) as u64
}

/// Legacy `sell`: 14 accounts. Args: amount (token base units),
/// min_sol_output (lamports).
pub fn sell_instruction(
    accts: &PumpAccounts,
    token_amount: u64,
    min_sol_output_lamports: u64,
) -> Instruction {
    let pump = decode::pump_program_id();
    let bonding_curve = decode::bonding_curve_pda(&accts.mint);
    let accounts = vec![
        AccountMeta::new_readonly(global_pda(), false),                       // 1
        AccountMeta::new(accts.fee_recipient, false),                          // 2
        AccountMeta::new_readonly(accts.mint, false),                          // 3
        AccountMeta::new(bonding_curve, false),                                // 4
        AccountMeta::new(ata_pda(&bonding_curve, &accts.token_program, &accts.mint), false), // 5
        AccountMeta::new(ata_pda(&accts.user, &accts.token_program, &accts.mint), false),    // 6
        AccountMeta::new(accts.user, true),                                    // 7
        AccountMeta::new_readonly(system_program_id(), false),                 // 8
        AccountMeta::new(creator_vault_pda(&accts.creator), false),             // 9
        AccountMeta::new_readonly(accts.token_program, false),                 // 10
        AccountMeta::new_readonly(event_authority_pda(), false),               // 11
        AccountMeta::new_readonly(pump, false),                                // 12
        AccountMeta::new_readonly(fee_config_pda(), false),                    // 13
        AccountMeta::new_readonly(fee_program_id(), false),                   // 14
    ];
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&DISC_SELL);
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output_lamports.to_le_bytes());
    Instruction { program_id: pump, accounts, data }
}

/// Create-ATA instruction (prepend before buy when the user's ATA is missing).
pub fn create_ata_instruction(
    payer: &Pubkey,
    ata: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: ata_program_id(),
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(*ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program_id(), false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![],
    }
}

/// System transfer instruction (used for the Jito tip transaction).
pub fn system_transfer_instruction(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = vec![2u8, 0, 0, 0]; // SystemInstruction::Transfer
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: system_program_id(),
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data,
    }
}

// ---------------------------------------------------------------------------
// Transaction assembly
// ---------------------------------------------------------------------------

/// Fee/priority configuration for a transaction.
#[derive(Debug, Clone)]
pub struct TxConfig {
    pub blockhash: Hash,
    pub compute_unit_limit: u32,
    pub compute_unit_price_microlamports: u64,
}

/// Assemble instructions into a signed v0 transaction and return it as
/// base64 (the encoding Jito `sendBundle` expects). Compute-budget
/// instructions are prepended automatically.
pub fn assemble_signed_base64(
    signer: &Keypair,
    instructions: Vec<Instruction>,
    cfg: &TxConfig,
) -> Result<String> {
    let mut ixs = Vec::with_capacity(instructions.len() + 2);
    ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(
        cfg.compute_unit_limit,
    ));
    ixs.push(ComputeBudgetInstruction::set_compute_unit_price(
        cfg.compute_unit_price_microlamports,
    ));
    ixs.extend(instructions);

    let message = V0Message::try_compile(&signer.pubkey(), &ixs, &[], cfg.blockhash.clone())
        .map_err(|e| anyhow!("compiling v0 message: {e}"))?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[signer])
        .map_err(|e| anyhow!("signing transaction: {e}"))?;
    let bytes = bincode::serialize(&tx).context("serializing transaction")?;
    Ok(B64.encode(bytes))
}

// ---------------------------------------------------------------------------
// High-level order builders
// ---------------------------------------------------------------------------

/// A market buy order on the Pump.fun bonding curve.
#[derive(Debug, Clone)]
pub struct BuyOrder {
    pub accts: PumpAccounts,
    /// SOL budget in lamports. The program deducts fees from this amount.
    pub sol_to_spend_lamports: u64,
    /// Expected token output for the budget, from [`quote_buy_exact_sol_in`]
    /// against the latest observed curve state. The slippage floor
    /// (`min_tokens_out`) is derived from this; 0 = unknown, floor disabled.
    pub expected_tokens_out: u64,
    /// Slippage tolerance in basis points, applied to derive min_tokens_out.
    pub slippage_bps: u16,
    /// Prepend a create-ATA for the buyer when true.
    pub create_user_ata: bool,
    pub compute_unit_limit: u32,
    pub compute_unit_price_microlamports: u64,
}

/// Build a signed base64 buy transaction (single tx; bundle assembly happens
/// at the Jito layer). Uses `buy_exact_sol_in`: the program spends up to
/// `sol_to_spend_lamports` and guarantees at least `min_tokens_out` tokens.
/// The caller supplies `blockhash` so every transaction in a bundle shares
/// one recent blockhash (required for bundle atomicity).
pub async fn build_buy_transaction(
    signer: &Keypair,
    order: &BuyOrder,
    blockhash: Hash,
) -> Result<String> {
    let mut ixs = Vec::new();
    if order.create_user_ata {
        let ata = ata_pda(&order.accts.user, &order.accts.token_program, &order.accts.mint);
        ixs.push(create_ata_instruction(
            &order.accts.user,
            &ata,
            &order.accts.user,
            &order.accts.mint,
            &order.accts.token_program,
        ));
    }
    // Slippage floor: accept the quoted output minus tolerance. With no
    // quote available, fall back to 1 so the floor can't fail the tx —
    // simulation (protection gate) still guards against outright failure.
    let min_tokens_out = if order.expected_tokens_out > 0 {
        let floor = (order.expected_tokens_out as u128)
            .saturating_mul(10_000 - order.slippage_bps.min(10_000) as u128)
            / 10_000;
        floor.clamp(1, u64::MAX as u128) as u64
    } else {
        1
    };
    ixs.push(buy_exact_quote_in_v3_instruction(
        &order.accts,
        order.sol_to_spend_lamports,
        min_tokens_out,
    ));
    assemble_signed_base64(
        signer,
        ixs,
        &TxConfig {
            blockhash,
            compute_unit_limit: order.compute_unit_limit,
            compute_unit_price_microlamports: order.compute_unit_price_microlamports,
        },
    )
}

/// A market sell order on the Pump.fun bonding curve.
#[derive(Debug, Clone)]
pub struct SellOrder {
    pub accts: PumpAccounts,
    pub token_amount: u64,
    /// Slippage tolerance in basis points, applied to derive min_sol_output.
    /// Caller supplies the *expected* SOL output; min = expected * (1 - slip).
    pub expected_sol_output_lamports: u64,
    pub slippage_bps: u16,
    pub compute_unit_limit: u32,
    pub compute_unit_price_microlamports: u64,
}

pub async fn build_sell_transaction(
    signer: &Keypair,
    order: &SellOrder,
    blockhash: Hash,
) -> Result<String> {
    let min_sol_output = order
        .expected_sol_output_lamports
        .saturating_mul(10_000 - order.slippage_bps.min(10_000) as u64)
        / 10_000;
    let ixs = vec![sell_instruction(
        &order.accts,
        order.token_amount,
        min_sol_output,
    )];
    assemble_signed_base64(
        signer,
        ixs,
        &TxConfig {
            blockhash,
            compute_unit_limit: order.compute_unit_limit,
            compute_unit_price_microlamports: order.compute_unit_price_microlamports,
        },
    )
}

/// Build a signed base64 Jito tip transaction (SOL transfer to a tip account).
/// Takes the bundle's shared blockhash like the other builders.
pub async fn build_tip_transaction(
    signer: &Keypair,
    tip_account: &Pubkey,
    tip_lamports: u64,
    compute_unit_price_microlamports: u64,
    blockhash: Hash,
) -> Result<String> {
    let ixs = vec![system_transfer_instruction(
        &signer.pubkey(),
        tip_account,
        tip_lamports,
    )];
    assemble_signed_base64(
        signer,
        ixs,
        &TxConfig {
            blockhash,
            compute_unit_limit: 10_000,
            compute_unit_price_microlamports,
        },
    )
}

// ---------------------------------------------------------------------------
// RPC helpers
// ---------------------------------------------------------------------------

async fn rpc_call(rpc_url: &str, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
    });
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

pub async fn fetch_latest_blockhash(rpc_url: &str) -> Result<Hash> {
    let v = rpc_call(
        rpc_url,
        "getLatestBlockhash",
        serde_json::json!([{"commitment": "confirmed"}]),
    )
    .await?;
    let hash_str = v
        .pointer("/result/value/blockhash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing blockhash in response"))?;
    Hash::from_str(hash_str).map_err(|e| anyhow!("bad blockhash: {e}"))
}

/// Resolve a mint's token program (legacy Token vs Token-2022) from the
/// mint account's owner. Cache per mint at the call site.
pub async fn fetch_mint_token_program(rpc_url: &str, mint: &Pubkey) -> Result<Pubkey> {
    let v = rpc_call(
        rpc_url,
        "getAccountInfo",
        serde_json::json!([mint.to_string(), {"encoding": "base64", "commitment": "confirmed"}]),
    )
    .await?;
    let owner = v
        .pointer("/result/value/owner")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing mint owner in response"))?;
    Pubkey::from_str(owner).map_err(|e| anyhow!("bad token program id: {e}"))
}

/// Fetch a token account's balance in base units (for sizing sells).
pub async fn fetch_token_balance(rpc_url: &str, token_account: &Pubkey) -> Result<u64> {
    let v = rpc_call(
        rpc_url,
        "getTokenAccountBalance",
        serde_json::json!([token_account.to_string(), {"commitment": "confirmed"}]),
    )
    .await?;
    let amount = v
        .pointer("/result/value/amount")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing token balance in response"))?;
    amount
        .parse::<u64>()
        .map_err(|e| anyhow!("bad token balance: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_accts() -> PumpAccounts {
        PumpAccounts {
            fee_recipient: Pubkey::new_from_array([1u8; 32]),
            mint: Pubkey::new_from_array([2u8; 32]),
            creator: Pubkey::new_from_array([3u8; 32]),
            user: Pubkey::new_from_array([4u8; 32]),
            token_program: pid(TOKEN_PROGRAM_STR),
            buyback_fee_recipient: buyback_wallet(),
        }
    }

    #[test]
    fn buy_exact_quote_in_v3_instruction_layout() {
        let ix = buy_exact_quote_in_v3_instruction(&test_accts(), 100_000_000, 3_500_000_000_000);
        assert_eq!(ix.accounts.len(), 17);
        assert_eq!(ix.program_id, decode::pump_program_id());
        assert_eq!(ix.data.len(), 24);
        assert_eq!(&ix.data[..8], &DISC_BUY_EXACT_QUOTE_IN_V3);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 100_000_000);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 3_500_000_000_000);
        assert!(ix.accounts[8].is_signer); // user
        assert!(ix.accounts[8].is_writable);
        assert!(ix.accounts[13].is_writable); // buyback_fee_recipient
        assert!(!ix.accounts[13].is_signer);
    }

    #[test]
    fn quote_matches_idl_formula() {
        // Fresh-curve-ish reserves, 0.1 SOL budget, 100 + 50 bps fees.
        // Hand-computed from the IDL quote formula (see python check).
        let q = quote_buy_exact_sol_in(
            30_000_000_000,
            1_073_000_000_000_000,
            100_000_000,
            100,
            50,
        );
        assert_eq!(q, 3_512_274_906_221);
    }

    #[test]
    fn quote_zero_fees_beats_fee_case() {
        let no_fee = quote_buy_exact_sol_in(30_000_000_000, 1_073_000_000_000_000, 100_000_000, 0, 0);
        assert_eq!(no_fee, 3_564_784_017_626);
        let with_fee =
            quote_buy_exact_sol_in(30_000_000_000, 1_073_000_000_000_000, 100_000_000, 100, 50);
        assert!(with_fee < no_fee);
    }

    #[test]
    fn quote_monotonic_in_budget_and_guards_degenerate_inputs() {
        let a = quote_buy_exact_sol_in(30_000_000_000, 1_073_000_000_000_000, 50_000_000, 100, 0);
        let b = quote_buy_exact_sol_in(30_000_000_000, 1_073_000_000_000_000, 100_000_000, 100, 0);
        assert!(b > a);
        assert_eq!(quote_buy_exact_sol_in(0, 1_073_000_000_000_000, 100_000_000, 100, 0), 0);
        assert_eq!(quote_buy_exact_sol_in(30_000_000_000, 0, 100_000_000, 100, 0), 0);
        assert_eq!(quote_buy_exact_sol_in(30_000_000_000, 1_073_000_000_000_000, 0, 100, 0), 0);
    }

    #[test]
    fn sell_instruction_layout() {
        let ix = sell_instruction(&test_accts(), 1_000_000, 40_000_000);
        assert_eq!(ix.accounts.len(), 14);
        assert_eq!(ix.program_id, decode::pump_program_id());
        assert_eq!(ix.data.len(), 24);
        assert_eq!(&ix.data[..8], &DISC_SELL);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 1_000_000);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 40_000_000);
    }

    #[test]
    fn pda_derivations_are_deterministic() {
        let mint = Pubkey::new_from_array([2u8; 32]);
        assert_eq!(decode::bonding_curve_pda(&mint), decode::bonding_curve_pda(&mint));
        assert_eq!(global_pda(), global_pda());
        assert_ne!(global_pda(), event_authority_pda());
    }

    #[test]
    fn ata_uses_owner_token_program_mint_seeds() {
        let owner = Pubkey::new_from_array([4u8; 32]);
        let tp = pid(TOKEN_PROGRAM_STR);
        let mint = Pubkey::new_from_array([2u8; 32]);
        let (expected, _) = Pubkey::find_program_address(
            &[owner.as_ref(), tp.as_ref(), mint.as_ref()],
            &ata_program_id(),
        );
        assert_eq!(ata_pda(&owner, &tp, &mint), expected);
    }
}
