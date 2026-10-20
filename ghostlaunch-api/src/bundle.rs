//! Launch bundler: create + initial buys as one Jito bundle.
//!
//! The anti-snipe property comes from Jito bundle ordering: the create
//! transaction and every initial buy land in the SAME bundle, executed
//! back-to-back in order. There is no gap between "token exists" and "dev
//! owns supply" for a sniper to slip into. (Jito executes bundle
//! transactions sequentially in one block; it does not revert the whole
//! bundle if a later transaction fails, so each transaction must also be
//! individually valid — which is why dry-run simulation matters.)

use anyhow::{anyhow, Context, Result};
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_sniper::{decode, tx_builder};
use std::collections::HashMap;
use std::str::FromStr;

use crate::create::{create_instruction, token_2022_program};
use crate::local::{assemble_signed_base64_multi, assemble_signed_base64_multi_plain, fetch_account_data};
use crate::plan::LaunchPlan;
use crate::stealth::StealthPlan;

/// A fully built launch bundle, ready to submit (or simulate).
#[derive(Debug)]
pub struct LaunchBundle {
    /// New token mint (also signs the create transaction).
    pub mint: Pubkey,
    /// Base64 transactions in execution order: [create, buy, buy, ...(, tip)].
    pub transactions: Vec<String>,
}

/// Resolve the Pump fee recipient from the on-chain Global account.
/// Falls back to an error (fail closed) rather than guessing.
async fn resolve_fee_recipient(rpc_url: &str) -> Result<Pubkey> {
    let data = fetch_account_data(rpc_url, &tx_builder::global_pda())
        .await?
        .ok_or_else(|| anyhow!("Pump Global account not found"))?;
    decode::global_fee_recipient(&data).context("parsing Global fee recipient")
}

/// Build one signed create_v2 transaction. (Signers: creator/payer + new mint.)
/// Extracted verbatim from the original bundle builder so the signed CLI
/// path and the stealth path share byte-identical logic.
async fn build_create_tx(
    plan: &LaunchPlan,
    creator_pubkey: &Pubkey,
    creator: &Keypair,
    mint_kp: &Keypair,
    blockhash: Hash,
    compute_unit_limit: u32,
    compute_unit_price_microlamports: u64,
) -> Result<String> {
    let create_ix = create_instruction(
        &mint_kp.pubkey(),
        creator_pubkey,
        creator_pubkey,
        &plan.token_name,
        &plan.token_symbol,
        &plan.token_uri,
    );
    assemble_signed_base64_multi(
        &[creator, mint_kp],
        vec![create_ix],
        blockhash,
        compute_unit_limit,
        compute_unit_price_microlamports,
    )
}

/// Build one signed buy_exact_quote_in_v3 transaction for a single buyer.
/// Extracted verbatim from the original bundle builder.
async fn build_buy_tx(
    plan: &LaunchPlan,
    buyer_kp: &Keypair,
    buyer_pubkey: &Pubkey,
    creator_pubkey: &Pubkey,
    mint: &Pubkey,
    fee_recipient: Pubkey,
    sol_lamports: u64,
    blockhash: Hash,
    compute_unit_limit: u32,
    compute_unit_price_microlamports: u64,
) -> Result<String> {
    let order = tx_builder::BuyOrder {
        accts: tx_builder::PumpAccounts {
            fee_recipient,
            mint: *mint,
            creator: *creator_pubkey,
            user: *buyer_pubkey,
            token_program: token_2022_program(),
            buyback_fee_recipient: tx_builder::buyback_wallet(),
        },
        sol_to_spend_lamports: sol_lamports,
        // Fresh mint: no prior curve state to quote against; slippage
        // floor disabled (min_tokens_out derived from slippage_bps only
        // when a quote is available).
        expected_tokens_out: 0,
        slippage_bps: plan.slippage_bps,
        create_user_ata: true, // fresh mint: buyer has no ATA yet
        compute_unit_limit,
        compute_unit_price_microlamports,
    };
    tx_builder::build_buy_transaction(buyer_kp, &order, blockhash).await
}

/// Shared preconditions for both bundle builders: creator keypair matches
/// the plan, every buyer wallet has a loaded keypair, tip has a destination.
fn check_keys(
    plan: &LaunchPlan,
    creator: &Keypair,
    buyer_keys: &HashMap<Pubkey, Keypair>,
    tip_account: Option<Pubkey>,
) -> Result<Pubkey> {
    let creator_pubkey =
        Pubkey::from_str(&plan.creator).map_err(|_| anyhow!("bad creator pubkey"))?;
    if creator.pubkey() != creator_pubkey {
        anyhow::bail!("--keypair does not match the plan's creator wallet");
    }
    // Every buyer wallet in the plan must have a loaded keypair.
    // (The creator's own keypair covers buys from the creator wallet.)
    for buy in &plan.initial_buys {
        let w = Pubkey::from_str(&buy.wallet).map_err(|_| anyhow!("bad buyer wallet"))?;
        if w != creator.pubkey() && !buyer_keys.contains_key(&w) {
            anyhow::bail!(
                "no keypair loaded for buyer wallet {w} (pass --buyer-key <path>)"
            );
        }
    }
    if plan.tip_lamports > 0 && tip_account.is_none() {
        anyhow::bail!("plan requests a tip but no --tip-account was given");
    }
    Ok(creator_pubkey)
}

/// Build the complete launch bundle.
///
/// * `plan` — validated launch plan.
/// * `creator` — creator keypair (pays, signs create + its own buys).
/// * `buyer_keys` — pubkey -> keypair for every buyer wallet in the plan.
/// * `tip_account` — Jito tip destination (required when plan.tip_lamports > 0).
pub async fn build_launch_bundle(
    rpc_url: &str,
    plan: &LaunchPlan,
    creator: &Keypair,
    buyer_keys: &HashMap<Pubkey, Keypair>,
    tip_account: Option<Pubkey>,
    compute_unit_limit: u32,
    compute_unit_price_microlamports: u64,
) -> Result<LaunchBundle> {
    let creator_pubkey = check_keys(plan, creator, buyer_keys, tip_account)?;

    // Fresh mint for the new token. It must sign the create transaction.
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();

    let fee_recipient = resolve_fee_recipient(rpc_url).await?;
    // One recent blockhash shared by every transaction in the bundle.
    let blockhash = tx_builder::fetch_latest_blockhash(rpc_url).await?;

    // --- 1. Create transaction (signers: creator/payer + new mint). ---
    let create_tx = build_create_tx(
        plan,
        &creator_pubkey,
        creator,
        &mint_kp,
        blockhash.clone(),
        compute_unit_limit,
        compute_unit_price_microlamports,
    )
    .await?;

    // --- 2. One buy transaction per initial buy, in plan order. ---
    let mut txs = vec![create_tx];
    for buy in &plan.initial_buys {
        let buyer_pubkey = Pubkey::from_str(&buy.wallet).unwrap();
        let buyer_kp: &Keypair = if buyer_pubkey == creator.pubkey() {
            creator
        } else {
            &buyer_keys[&buyer_pubkey]
        };
        let sol_lamports = (buy.sol_amount * 1_000_000_000.0) as u64;
        let buy_tx = build_buy_tx(
            plan,
            buyer_kp,
            &buyer_pubkey,
            &creator_pubkey,
            &mint,
            fee_recipient,
            sol_lamports,
            blockhash.clone(),
            compute_unit_limit,
            compute_unit_price_microlamports,
        )
        .await?;
        txs.push(buy_tx);
    }

    // --- 3. Optional Jito tip (improves landing priority). ---
    if plan.tip_lamports > 0 {
        let tip_tx = tx_builder::build_tip_transaction(
            creator,
            &tip_account.unwrap(),
            plan.tip_lamports,
            compute_unit_price_microlamports,
            blockhash,
        )
        .await?;
        txs.push(tip_tx);
    }

    if txs.len() > 5 {
        anyhow::bail!(
            "bundle would hold {} transactions; Jito limit is 5",
            txs.len()
        );
    }

    Ok(LaunchBundle {
        mint,
        transactions: txs,
    })
}

// ---------------------------------------------------------------------------
// Stealth builders
// ---------------------------------------------------------------------------

/// Build the SIGNED multi-hop funding transactions for a stealth plan, in
/// global submit order. These are plain SOL transfers (no compute-budget
/// instructions — ordinary-looking, and exactly 5,000 lamports fee, which is
/// what the hop funding math assumes) submitted via direct RPC *before* the
/// launch — never part of the Jito bundle.
pub fn build_funding_transactions(
    splan: &StealthPlan,
    creator: &Keypair,
    blockhash: Hash,
) -> Result<Vec<String>> {
    let mut txs = Vec::with_capacity(splan.funding_transfers.len());
    for t in &splan.funding_transfers {
        let from =
            Pubkey::from_str(&t.from).map_err(|_| anyhow!("bad funding sender"))?;
        let to = Pubkey::from_str(&t.to).map_err(|_| anyhow!("bad funding recipient"))?;
        let signer = splan
            .signer_keypair(creator, &from)
            .ok_or_else(|| anyhow!("no signer available for funding sender {from}"))?;
        let ix = tx_builder::system_transfer_instruction(&from, &to, t.lamports);
        txs.push(assemble_signed_base64_multi_plain(
            &[signer],
            vec![ix],
            blockhash.clone(),
        )?);
    }
    Ok(txs)
}

/// Stealth launch transactions, split by submission path.
pub struct StealthBundleTxs {
    /// New token mint.
    pub mint: Pubkey,
    /// Submitted as ONE Jito bundle: [create] (+ tip). When spread_slots == 0
    /// the buys stay here too: [create, buys…, tip].
    pub bundle_transactions: Vec<String>,
    /// (buy_index, signed tx) — submitted separately via direct RPC at
    /// create_slot + target_slot_offset. Empty when spread_slots == 0.
    pub spread_buys: Vec<(usize, String)>,
}

/// Build the stealth launch transactions. Buy amounts come from the stealth
/// plan (jittered); with spread_slots > 0 the buys are returned separately
/// for staggered submission instead of riding in the Jito bundle.
pub async fn build_stealth_bundle_txs(
    rpc_url: &str,
    plan: &LaunchPlan,
    creator: &Keypair,
    buyer_keys: &HashMap<Pubkey, Keypair>,
    tip_account: Option<Pubkey>,
    compute_unit_limit: u32,
    compute_unit_price_microlamports: u64,
    splan: &StealthPlan,
) -> Result<StealthBundleTxs> {
    let creator_pubkey = check_keys(plan, creator, buyer_keys, tip_account)?;

    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();

    let fee_recipient = resolve_fee_recipient(rpc_url).await?;
    let blockhash = tx_builder::fetch_latest_blockhash(rpc_url).await?;

    let create_tx = build_create_tx(
        plan,
        &creator_pubkey,
        creator,
        &mint_kp,
        blockhash.clone(),
        compute_unit_limit,
        compute_unit_price_microlamports,
    )
    .await?;

    let spread = splan.config.spread_slots > 0;
    let mut bundle_txs = vec![create_tx];
    let mut spread_buys: Vec<(usize, String)> = Vec::new();

    for (i, buy) in plan.initial_buys.iter().enumerate() {
        let buyer_pubkey = Pubkey::from_str(&buy.wallet).unwrap();
        let buyer_kp: &Keypair = if buyer_pubkey == creator.pubkey() {
            creator
        } else {
            &buyer_keys[&buyer_pubkey]
        };
        let sol_lamports = splan.buyers[i].jittered_amount_lamports;
        let buy_tx = build_buy_tx(
            plan,
            buyer_kp,
            &buyer_pubkey,
            &creator_pubkey,
            &mint,
            fee_recipient,
            sol_lamports,
            blockhash.clone(),
            compute_unit_limit,
            compute_unit_price_microlamports,
        )
        .await?;
        if spread {
            spread_buys.push((i, buy_tx));
        } else {
            bundle_txs.push(buy_tx);
        }
    }

    if plan.tip_lamports > 0 {
        let tip_tx = tx_builder::build_tip_transaction(
            creator,
            &tip_account.unwrap(),
            plan.tip_lamports,
            compute_unit_price_microlamports,
            blockhash,
        )
        .await?;
        bundle_txs.push(tip_tx);
    }

    if bundle_txs.len() > 5 {
        anyhow::bail!(
            "bundle would hold {} transactions; Jito limit is 5",
            bundle_txs.len()
        );
    }

    Ok(StealthBundleTxs {
        mint,
        bundle_transactions: bundle_txs,
        spread_buys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{InitialBuy, LaunchPlan};

    fn plan() -> LaunchPlan {
        LaunchPlan {
            token_name: "T".into(),
            token_symbol: "T".into(),
            token_uri: "https://example.com/m.json".into(),
            creator: "So11111111111111111111111111111111111111112".into(),
            initial_buys: vec![InitialBuy {
                wallet: "So11111111111111111111111111111111111111112".into(),
                sol_amount: 0.5,
            }],
            tip_lamports: 0,
            slippage_bps: 500,
            stealth: None,
        }
    }

    #[tokio::test]
    async fn rejects_wrong_creator_keypair() {
        let p = plan();
        let wrong = Keypair::new();
        let mut buyers = HashMap::new();
        buyers.insert(wrong.pubkey(), Keypair::new()); // unused; fails earlier
        let err = build_launch_bundle(
            "http://localhost:1",
            &p,
            &wrong,
            &buyers,
            None,
            300_000,
            50_000,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("does not match"));
    }

    #[tokio::test]
    async fn rejects_missing_buyer_keypair() {
        let p = plan();
        // Keypair matching the creator but no buyer entry.
        let seed = [7u8; 32];
        let creator = Keypair::new_from_array(seed);
        let mut plan2 = p.clone();
        plan2.creator = creator.pubkey().to_string();
        plan2.initial_buys[0].wallet = "11111111111111111111111111111111".to_string();
        let buyers = HashMap::new();
        let err = build_launch_bundle(
            "http://localhost:1",
            &plan2,
            &creator,
            &buyers,
            None,
            300_000,
            50_000,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no keypair loaded"));
    }
}
