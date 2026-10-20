//! PumpSwap AMM swap transaction builders.
//!
//! Builds signed, base64-encoded versioned (v0) transactions for the PumpSwap
//! AMM program `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`.
//!
//! Instruction layouts are from the official pump-public-docs IDL
//! (idl/pump_amm.json, main branch, Oct 2026):
//! https://github.com/pump-fun/pump-public-docs
//!
//! ## Critical finding (Oct 2026, devnet verification)
//!
//! The v1 `buy` / `sell` instructions (23/21 accounts) FAIL on devnet with
//! `Custom(3012)`. The v2 instructions (`buy_v2` / `sell_v2`, 17 accounts)
//! are what actually works on-chain — verified against a successful devnet
//! swap transaction. This module uses v2 exclusively.
//!
//! `buy_v2` args: `base_amount_out` (u64, exact base tokens to receive),
//!   `max_quote_amount_in` (u64, maximum quote lamports willing to pay).
//! `sell_v2` args: `base_amount_in` (u64, exact base tokens to sell),
//!   `min_quote_amount_out` (u64, minimum quote lamports to receive).
//!
//! **Verify on devnet before mainnet.**

use std::str::FromStr;

use anyhow::Result;
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_pubkey::Pubkey;

use crate::decode;
use crate::tx_builder::{
    assemble_signed_base64, ata_program_id, create_ata_instruction, system_program_id,
    TxConfig, TOKEN_2022_PROGRAM_STR, TOKEN_PROGRAM_STR,
};

/// `buy_v2` discriminator, from the official pump_amm.json IDL:
/// [184, 23, 238, 97, 103, 197, 211, 61] = 0xb817ee6167c5d33d
/// Verified against a successful devnet swap.
pub const PUMPSWAP_DISC_BUY_V2: [u8; 8] = [184, 23, 238, 97, 103, 197, 211, 61];

/// `sell_v2` discriminator, from the official pump_amm.json IDL:
/// [93, 246, 130, 60, 231, 233, 64, 178] = 0x5df6823ce7e940b2
pub const PUMPSWAP_DISC_SELL_V2: [u8; 8] = [93, 246, 130, 60, 231, 233, 64, 178];

fn pid(s: &str) -> Pubkey {
    Pubkey::from_str(s).expect("hardcoded program id")
}

fn pumpswap_program_id() -> Pubkey {
    decode::pumpswap_program_id()
}

fn pda(seeds: &[&[u8]], program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}

// ---------------------------------------------------------------------------
// PumpSwap PDA derivations (all under the PumpSwap AMM program unless noted)
// ---------------------------------------------------------------------------

/// global_config = ["global_config"] under the PumpSwap AMM program.
pub fn pumpswap_global_config() -> Pubkey {
    pda(&[b"global_config"], &pumpswap_program_id())
}

/// event_authority = ["__event_authority"] under the PumpSwap AMM program.
pub fn pumpswap_event_authority() -> Pubkey {
    pda(&[b"__event_authority"], &pumpswap_program_id())
}

/// user_volume_accumulator = ["user_volume_accumulator", user] under PumpSwap AMM.
pub fn pumpswap_user_volume_accumulator(user: &Pubkey) -> Pubkey {
    pda(&[b"user_volume_accumulator", user.as_ref()], &pumpswap_program_id())
}

/// fee_config = ["fee_config", pump_program_id] under the fee config program.
/// Note: the IDL shows the fee_config PDA's program as a fixed address
/// (bytes [12, 53, 255, ...]), NOT the standard fee program. We derive
/// using the address from the IDL.
pub fn pumpswap_fee_config() -> Pubkey {
    // PumpSwap buy_v2/sell_v2 fee_config: Pump Fees PDA with seeds
    // [b"fee_config", pump_amm_program_id], under the Pump Fees program.
    // Verified 2026-10-16 against pump-public-docs PUMP_SWAP_TRADE_V2.md and
    // mainnet simulation (ConstraintSeeds resolved).
    // Address: 5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx
    let fee_config_program = pid("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
    let pump_amm = pumpswap_program_id();
    pda(&[b"fee_config", pump_amm.as_ref()], &fee_config_program)
}

/// One of the 8 official PumpSwap buyback fee recipient wallets.
/// The buy_v2 instruction takes this wallet's wSOL ATA (not the wallet itself).
/// From pump-public-docs docs/FEE_RECIPIENTS.md (2026-10-16).
pub fn pumpswap_buyback_wallet() -> Pubkey {
    pid("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD")
}

/// Standard ATA derivation.
pub fn pumpswap_ata(owner: &Pubkey, token_program: &Pubkey, mint: &Pubkey) -> Pubkey {
    pda(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program_id(),
    )
}

// ---------------------------------------------------------------------------
// Swap accounts (v2 — 17 accounts)
// ---------------------------------------------------------------------------

/// All accounts needed for a PumpSwap v2 buy or sell.
///
/// The `buyback_fee_recipient` must be read from chain (it is set by the
/// program admin in the fee config).
#[derive(Debug, Clone)]
pub struct PumpSwapAccounts {
    pub pool: Pubkey,
    pub user: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    /// Token program owning the base mint (legacy SPL Token or Token-2022).
    pub base_token_program: Pubkey,
    /// Token program owning the quote mint (usually legacy SPL Token for wSOL).
    pub quote_token_program: Pubkey,
    /// Buyback fee recipient (read from fee_config account data).
    pub buyback_fee_recipient: Pubkey,
    /// Fee config PDA. If None, derived via `pumpswap_fee_config()`.
    /// Prefer passing the known-good address from chain.
    pub fee_config: Option<Pubkey>,
    /// Pool's base token account. If None, derived as PDA
    /// [pool, base_token_program, base_mint]. Prefer reading from pool data.
    pub pool_base_token_account: Option<Pubkey>,
    /// Pool's quote token account. If None, derived as PDA
    /// [pool, quote_token_program, quote_mint]. Prefer reading from pool data.
    pub pool_quote_token_account: Option<Pubkey>,
}

impl PumpSwapAccounts {
    /// Derive the user's base-token ATA.
    pub fn user_base_ata(&self) -> Pubkey {
        pumpswap_ata(&self.user, &self.base_token_program, &self.base_mint)
    }

    /// Derive the user's quote-token (wSOL) ATA.
    pub fn user_quote_ata(&self) -> Pubkey {
        pumpswap_ata(&self.user, &self.quote_token_program, &self.quote_mint)
    }

    /// Get the pool's base token account (explicit or derived).
    pub fn pool_base_ta(&self) -> Pubkey {
        self.pool_base_token_account.unwrap_or_else(|| {
            pda(
                &[
                    self.pool.as_ref(),
                    self.base_token_program.as_ref(),
                    self.base_mint.as_ref(),
                ],
                &ata_program_id(),
            )
        })
    }

    /// Get the pool's quote token account (explicit or derived).
    pub fn pool_quote_ta(&self) -> Pubkey {
        self.pool_quote_token_account.unwrap_or_else(|| {
            pda(
                &[
                    self.pool.as_ref(),
                    self.quote_token_program.as_ref(),
                    self.quote_mint.as_ref(),
                ],
                &ata_program_id(),
            )
        })
    }

    /// Get the fee config account (explicit or derived).
    pub fn fee_config_account(&self) -> Pubkey {
        self.fee_config.unwrap_or_else(pumpswap_fee_config)
    }
}

/// The 17 accounts for `buy_v2` / `sell_v2`, in IDL order.
/// Verified against a successful devnet swap transaction.
fn swap_v2_accounts(accts: &PumpSwapAccounts) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(accts.pool, false),                             // 1  pool
        AccountMeta::new(accts.user, true),                              // 2  user (signer)
        AccountMeta::new_readonly(pumpswap_global_config(), false),      // 3  global_config
        AccountMeta::new_readonly(accts.base_mint, false),               // 4  base_mint
        AccountMeta::new_readonly(accts.quote_mint, false),              // 5  quote_mint
        AccountMeta::new(accts.user_base_ata(), false),                  // 6  user_base_token_account
        AccountMeta::new(accts.user_quote_ata(), false),                 // 7  user_quote_token_account
        AccountMeta::new(accts.pool_base_ta(), false),                   // 8  pool_base_token_account
        AccountMeta::new(accts.pool_quote_ta(), false),                  // 9  pool_quote_token_account
        AccountMeta::new_readonly(accts.base_token_program, false),      // 10 base_token_program
        AccountMeta::new_readonly(accts.quote_token_program, false),     // 11 quote_token_program
        AccountMeta::new_readonly(system_program_id(), false),           // 12 system_program
        AccountMeta::new(pumpswap_user_volume_accumulator(&accts.user), false), // 13 user_volume_accumulator
        AccountMeta::new_readonly(accts.fee_config_account(), false),        // 14 fee_config
        AccountMeta::new(accts.buyback_fee_recipient, false),            // 15 buyback_fee_recipient
        AccountMeta::new_readonly(pumpswap_event_authority(), false),    // 16 event_authority
        AccountMeta::new_readonly(pumpswap_program_id(), false),         // 17 program
    ]
}

// ---------------------------------------------------------------------------
// Instructions (v2)
// ---------------------------------------------------------------------------

/// `buy_v2`: buy exactly `base_amount_out` base tokens,
/// paying at most `max_quote_amount_in` lamports of quote (wSOL).
/// Args: discriminator(8) + base_amount_out:u64 + max_quote_amount_in:u64.
pub fn buy_v2_instruction(
    accts: &PumpSwapAccounts,
    base_amount_out: u64,
    max_quote_amount_in_lamports: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&PUMPSWAP_DISC_BUY_V2);
    data.extend_from_slice(&base_amount_out.to_le_bytes());
    data.extend_from_slice(&max_quote_amount_in_lamports.to_le_bytes());
    Instruction {
        program_id: pumpswap_program_id(),
        accounts: swap_v2_accounts(accts),
        data,
    }
}

/// `sell_v2`: sell exactly `base_amount_in` base tokens,
/// receiving at least `min_quote_amount_out` lamports of quote (wSOL).
/// Args: discriminator(8) + base_amount_in:u64 + min_quote_amount_out:u64.
pub fn sell_v2_instruction(
    accts: &PumpSwapAccounts,
    base_amount_in: u64,
    min_quote_amount_out_lamports: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&PUMPSWAP_DISC_SELL_V2);
    data.extend_from_slice(&base_amount_in.to_le_bytes());
    data.extend_from_slice(&min_quote_amount_out_lamports.to_le_bytes());
    Instruction {
        program_id: pumpswap_program_id(),
        accounts: swap_v2_accounts(accts),
        data,
    }
}

// ---------------------------------------------------------------------------
// High-level order builders
// ---------------------------------------------------------------------------

/// A market buy order on a PumpSwap pool (spend SOL, receive tokens).
///
/// Note: v2 `buy` takes an exact token output amount, not a SOL budget.
/// The caller should quote the expected output off-chain and apply slippage
/// to derive `max_quote_amount_in`.
#[derive(Debug, Clone)]
pub struct PumpSwapBuyOrder {
    pub accts: PumpSwapAccounts,
    /// Exact base tokens to receive.
    pub base_amount_out: u64,
    /// Maximum quote lamports willing to pay (slippage ceiling).
    pub max_quote_amount_in_lamports: u64,
    /// Prepend a create-ATA for the buyer's base-token account when true.
    pub create_user_base_ata: bool,
    pub compute_unit_limit: u32,
    pub compute_unit_price_microlamports: u64,
}

/// Build a signed base64 buy transaction (v2).
pub async fn build_pumpswap_buy_transaction(
    signer: &Keypair,
    order: &PumpSwapBuyOrder,
    blockhash: Hash,
) -> Result<String> {
    let mut ixs = Vec::new();
    if order.create_user_base_ata {
        let ata = order.accts.user_base_ata();
        ixs.push(create_ata_instruction(
            &order.accts.user,
            &ata,
            &order.accts.user,
            &order.accts.base_mint,
            &order.accts.base_token_program,
        ));
    }
    ixs.push(buy_v2_instruction(
        &order.accts,
        order.base_amount_out,
        order.max_quote_amount_in_lamports,
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

/// A market sell order on a PumpSwap pool (sell tokens, receive SOL).
#[derive(Debug, Clone)]
pub struct PumpSwapSellOrder {
    pub accts: PumpSwapAccounts,
    /// Exact base tokens to sell.
    pub token_amount: u64,
    /// Minimum quote lamports to receive (slippage floor).
    pub min_quote_amount_out_lamports: u64,
    /// Prepend a create-ATA for the user's quote (wSOL) account when true.
    pub create_user_quote_ata: bool,
    pub compute_unit_limit: u32,
    pub compute_unit_price_microlamports: u64,
}

/// Build a signed base64 sell transaction (v2).
pub async fn build_pumpswap_sell_transaction(
    signer: &Keypair,
    order: &PumpSwapSellOrder,
    blockhash: Hash,
) -> Result<String> {
    let mut ixs = Vec::new();
    if order.create_user_quote_ata {
        let ata = order.accts.user_quote_ata();
        ixs.push(create_ata_instruction(
            &order.accts.user,
            &ata,
            &order.accts.user,
            &order.accts.quote_mint,
            &order.accts.quote_token_program,
        ));
    }
    ixs.push(sell_v2_instruction(
        &order.accts,
        order.token_amount,
        order.min_quote_amount_out_lamports,
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

/// Resolve the legacy SPL Token program id.
pub fn legacy_token_program_id() -> Pubkey {
    pid(TOKEN_PROGRAM_STR)
}

/// Resolve the Token-2022 program id.
pub fn token_2022_program_id() -> Pubkey {
    pid(TOKEN_2022_PROGRAM_STR)
}

/// Find the PumpSwap pool for a base mint via getProgramAccounts.
/// Filters by base_mint at offset 43 (after disc(8)+bump(1)+index(2)+creator(32)).
/// Returns the pool pubkey and its decoded vault addresses.
/// Uses the public RPC (Helius getProgramAccounts ignores filters).
/// Retries 3x with delay — new pools take a few seconds to index.
pub async fn find_pumpswap_pool(
    _rpc_url: &str,
    base_mint: &Pubkey,
) -> Result<(Pubkey, Pubkey, Pubkey)> {
    use base64::Engine as _;
    // Public RPC: Helius v1 getProgramAccounts doesn't honor memcmp filters.
    let rpc_url = "https://api.mainnet-beta.solana.com";
    let client = reqwest::Client::new();
    let mut last_err = anyhow::anyhow!("no attempts");
    for attempt in 0..4 {
        if attempt > 0 {
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
        }
        let resp: serde_json::Value = match client
            .post(rpc_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getProgramAccounts",
                "params": [
                    pumpswap_program_id().to_string(),
                    {
                        "encoding": "base64",
                        "filters": [
                            {"dataSize": 301},
                            {"memcmp": {"offset": 43, "bytes": base_mint.to_string()}}
                        ]
                    }
                ]
            }))
            .send()
            .await
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(e) => {
                    last_err = anyhow::anyhow!("json parse: {}", e);
                    continue;
                }
            },
            Err(e) => {
                last_err = anyhow::anyhow!("rpc: {}", e);
                continue;
            }
        };
        let accounts = match resp["result"].as_array() {
            Some(a) => a,
            None => {
                last_err = anyhow::anyhow!("getProgramAccounts failed: {}", resp);
                continue;
            }
        };
        if let Some(first) = accounts.first() {
            let pool = Pubkey::from_str(
                first["pubkey"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("missing pubkey"))?,
            )?;
            let data_b64 = first["account"]["data"][0]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing account data"))?;
            let data = base64::engine::general_purpose::STANDARD.decode(data_b64)?;
            if data.len() < 203 {
                anyhow::bail!("pool data too short: {}", data.len());
            }
            let pool_base_ta = Pubkey::try_from(&data[139..171])
                .map_err(|_| anyhow::anyhow!("bad pool_base_ta"))?;
            let pool_quote_ta = Pubkey::try_from(&data[171..203])
                .map_err(|_| anyhow::anyhow!("bad pool_quote_ta"))?;
            return Ok((pool, pool_base_ta, pool_quote_ta));
        }
        last_err = anyhow::anyhow!("no PumpSwap pool found for mint {} (attempt {})", base_mint, attempt + 1);
    }
    Err(last_err)
}

/// Build a complete PumpSwap buy_v2 transaction (base64, signed) with SOL wrapping.
/// Discovers the pool, wraps SOL to wSOL, creates ATAs as needed, then buys.
/// This is the end-to-end buy path for migration signals.
///
/// `sol_to_spend_lamports`: total SOL budget (wrapped + fees).
/// `min_tokens_out`: slippage floor for base tokens.
pub async fn build_pumpswap_buy_with_wrap(
    signer: &Keypair,
    rpc_url: &str,
    base_mint: &Pubkey,
    base_token_program: &Pubkey,
    sol_to_spend_lamports: u64,
    min_tokens_out: u64,
    compute_unit_limit: u32,
    compute_unit_price_microlamports: u64,
    blockhash: Hash,
) -> Result<String> {
    let user = signer.pubkey();
    let wsol = Pubkey::from_str("So11111111111111111111111111111111111111112")?;
    let spl_token = Pubkey::from_str(TOKEN_PROGRAM_STR)?;

    // Discover the pool
    let (pool, pool_base_ta, pool_quote_ta) = find_pumpswap_pool(rpc_url, base_mint).await?;

    // Derive accounts
    let user_base_ata = pumpswap_ata(&user, base_token_program, base_mint);
    let user_quote_ata = pumpswap_ata(&user, &spl_token, &wsol);
    let buyback_ata = pumpswap_ata(&pumpswap_buyback_wallet(), &spl_token, &wsol);

    let mut ixs = Vec::new();

    // 1. Create wSOL ATA if missing
    if !account_exists(rpc_url, &user_quote_ata).await? {
        ixs.push(create_ata_instruction(
            &user,
            &user_quote_ata,
            &user,
            &wsol,
            &spl_token,
        ));
    }
    // 2. Wrap SOL: transfer lamports to wSOL ATA + sync native
    ixs.push(sol_to_wsol_instruction(&user, &user_quote_ata, sol_to_spend_lamports));
    ixs.push(sync_native_instruction(&user_quote_ata));

    // 3. Create base token ATA if missing
    let create_base_ata = !account_exists(rpc_url, &user_base_ata).await?;

    // 4. Build the buy order
    let swap_accts = PumpSwapAccounts {
        pool,
        user,
        base_mint: *base_mint,
        quote_mint: wsol,
        base_token_program: *base_token_program,
        quote_token_program: spl_token,
        buyback_fee_recipient: buyback_ata,
        fee_config: None, // derived via pumpswap_fee_config()
        pool_base_token_account: Some(pool_base_ta),
        pool_quote_token_account: Some(pool_quote_ta),
    };
    let order = PumpSwapBuyOrder {
        accts: swap_accts,
        base_amount_out: min_tokens_out.max(1),
        max_quote_amount_in_lamports: sol_to_spend_lamports,
        create_user_base_ata: create_base_ata,
        compute_unit_limit,
        compute_unit_price_microlamports,
    };

    // 5. Prepend wSOL instructions to the buy transaction
    // Build buy ixs manually to prepend wrap instructions
    let mut all_ixs = ixs;
    if order.create_user_base_ata {
        let ata = order.accts.user_base_ata();
        all_ixs.push(create_ata_instruction(
            &order.accts.user,
            &ata,
            &order.accts.user,
            &order.accts.base_mint,
            &order.accts.base_token_program,
        ));
    }
    all_ixs.push(buy_v2_instruction(
        &order.accts,
        order.base_amount_out,
        order.max_quote_amount_in_lamports,
    ));

    assemble_signed_base64(
        signer,
        all_ixs,
        &TxConfig {
            blockhash,
            compute_unit_limit: order.compute_unit_limit,
            compute_unit_price_microlamports: order.compute_unit_price_microlamports,
        },
    )
}

/// Check if an account exists via RPC.
pub async fn account_exists(rpc_url: &str, pubkey: &Pubkey) -> Result<bool> {
    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getAccountInfo",
            "params": [pubkey.to_string(), {"encoding": "base64"}]
        }))
        .send()
        .await?
        .json()
        .await?;
    Ok(!resp["result"]["value"].is_null())
}

/// Transfer SOL to a wSOL ATA (part of wrapping).
fn sol_to_wsol_instruction(
    user: &Pubkey,
    wsol_ata: &Pubkey,
    lamports: u64,
) -> Instruction {
    Instruction {
        program_id: system_program_id(),
        accounts: vec![
            AccountMeta::new(*user, true),
            AccountMeta::new(*wsol_ata, false),
        ],
        data: {
            let mut d = vec![2, 0, 0, 0]; // system transfer discriminator
            d.extend_from_slice(&lamports.to_le_bytes());
            d
        },
    }
}

/// Sync native (wrap the SOL in the ATA to wSOL).
fn sync_native_instruction(wsol_ata: &Pubkey) -> Instruction {
    Instruction {
        program_id: Pubkey::from_str(TOKEN_PROGRAM_STR).expect("spl token"),
        accounts: vec![AccountMeta::new(*wsol_ata, false)],
        data: vec![17], // sync_native discriminator
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_accounts() -> PumpSwapAccounts {
        let user = Pubkey::new_unique();
        let pool = Pubkey::new_unique();
        let base_mint = Pubkey::new_unique();
        let quote_mint =
            Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
        PumpSwapAccounts {
            pool,
            user,
            base_mint,
            quote_mint,
            base_token_program: legacy_token_program_id(),
            quote_token_program: legacy_token_program_id(),
            buyback_fee_recipient: Pubkey::new_unique(),
            fee_config: None,
            pool_base_token_account: None,
            pool_quote_token_account: None,
        }
    }

    #[test]
    fn buy_v2_instruction_has_correct_discriminator() {
        let accts = test_accounts();
        let ix = buy_v2_instruction(&accts, 1_000_000, 500_000);
        assert_eq!(ix.program_id, pumpswap_program_id());
        assert_eq!(&ix.data[..8], &PUMPSWAP_DISC_BUY_V2);
        // buy_v2 discriminator matches the successful devnet swap
        assert_eq!(
            PUMPSWAP_DISC_BUY_V2,
            [184, 23, 238, 97, 103, 197, 211, 61]
        );
        assert_eq!(ix.data.len(), 8 + 8 + 8);
        // 17 accounts per the IDL (verified against devnet)
        assert_eq!(ix.accounts.len(), 17);
        // user is account[1] and is a signer
        assert!(ix.accounts[1].is_signer);
        assert_eq!(ix.accounts[1].pubkey, accts.user);
    }

    #[test]
    fn sell_v2_instruction_has_correct_discriminator() {
        let accts = test_accounts();
        let ix = sell_v2_instruction(&accts, 500_000, 400_000);
        assert_eq!(ix.program_id, pumpswap_program_id());
        assert_eq!(&ix.data[..8], &PUMPSWAP_DISC_SELL_V2);
        assert_eq!(
            PUMPSWAP_DISC_SELL_V2,
            [93, 246, 130, 60, 231, 233, 64, 178]
        );
        assert_eq!(ix.data.len(), 8 + 8 + 8);
        assert_eq!(ix.accounts.len(), 17);
        assert!(ix.accounts[1].is_signer);
        assert_eq!(ix.accounts[1].pubkey, accts.user);
    }

    #[test]
    fn v2_account_order_matches_devnet() {
        // The 17-account layout was verified against devnet transaction
        // 355wg1ntWhxYfrB7TL2zDRLoZmUrg4RQWMN86k8QrPg5yNqRRrNEZT5qVeJAmjLgpQMTr9E62YtTmcbsYRRDXnHb
        // which used buy_v2 successfully.
        let accts = test_accounts();
        let accounts = swap_v2_accounts(&accts);
        // Spot-check key positions
        assert_eq!(accounts[0].pubkey, accts.pool); // pool
        assert_eq!(accounts[2].pubkey, pumpswap_global_config()); // global_config
        assert_eq!(accounts[11].pubkey, system_program_id()); // system_program
        assert_eq!(accounts[15].pubkey, pumpswap_event_authority()); // event_authority
        assert_eq!(accounts[16].pubkey, pumpswap_program_id()); // program
    }
}
