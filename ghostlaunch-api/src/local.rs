//! Small helpers that live in this crate (not in the sniper repo, which is
//! off-limits for modification):
//! - loading Solana CLI keypair files,
//! - assembling multi-signer v0 transactions (Pump `create` needs the new
//!   mint AND the payer to sign),
//! - a minimal getAccountInfo fetch for resolving the Pump fee recipient.

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_message::{v0::Message as V0Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

/// Load a Solana CLI keypair file (JSON array of 64 bytes).
/// The secret is only ever held in memory to construct the Keypair —
/// it is never logged, printed, or written anywhere.
pub fn load_keypair_file(path: &str) -> Result<Keypair> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading keypair file {path}"))?;
    let bytes: Vec<u8> =
        serde_json::from_str(raw.trim()).context("keypair file must be a JSON array")?;
    let arr: [u8; 64] = bytes
        .try_into()
        .map_err(|_| anyhow!("keypair file must contain exactly 64 bytes"))?;
    // CLI format: 32-byte seed followed by 32-byte public key.
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&arr[..32]);
    Ok(Keypair::new_from_array(seed))
}

/// Assemble instructions into a v0 transaction signed by ALL given signers,
/// returned as base64 (what Jito `sendBundle` expects). Compute-budget
/// instructions are prepended. `signers[0]` is the fee payer.
pub fn assemble_signed_base64_multi(    signers: &[&Keypair],
    instructions: Vec<Instruction>,
    blockhash: Hash,
    compute_unit_limit: u32,
    compute_unit_price_microlamports: u64,
) -> Result<String> {
    if signers.is_empty() {
        anyhow::bail!("at least one signer is required");
    }
    let mut ixs = Vec::with_capacity(instructions.len() + 2);
    ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(
        compute_unit_limit,
    ));
    ixs.push(ComputeBudgetInstruction::set_compute_unit_price(
        compute_unit_price_microlamports,
    ));
    ixs.extend(instructions);

    let message = V0Message::try_compile(&signers[0].pubkey(), &ixs, &[], blockhash)
        .map_err(|e| anyhow!("compiling v0 message: {e}"))?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), signers)
        .map_err(|e| anyhow!("signing transaction: {e}"))?;
    let bytes = bincode::serialize(&tx).context("serializing transaction")?;
    Ok(B64.encode(bytes))
}

/// Assemble a PLAIN v0 transaction: no compute-budget instructions, just the
/// given instructions. Used for stealth funding transfers, which should look
/// like ordinary wallet transfers — and whose fee is then exactly 5,000
/// lamports (one signature), matching the hop funding math.
/// `signers[0]` is the fee payer.
pub fn assemble_signed_base64_multi_plain(
    signers: &[&Keypair],
    instructions: Vec<Instruction>,
    blockhash: Hash,
) -> Result<String> {
    if signers.is_empty() {
        anyhow::bail!("at least one signer is required");
    }
    let message = V0Message::try_compile(&signers[0].pubkey(), &instructions, &[], blockhash)
        .map_err(|e| anyhow!("compiling v0 message: {e}"))?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), signers)
        .map_err(|e| anyhow!("signing transaction: {e}"))?;
    let bytes = bincode::serialize(&tx).context("serializing transaction")?;
    Ok(B64.encode(bytes))
}

/// Minimal getAccountInfo (base64). Used to read the Pump Global account for
/// the current fee recipient.
pub async fn fetch_account_data(rpc_url: &str, pubkey: &Pubkey) -> Result<Option<Vec<u8>>> {    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getAccountInfo",
        "params": [pubkey.to_string(), {"encoding": "base64", "commitment": "confirmed"}],
    });
    let resp: serde_json::Value = reqwest::Client::new()
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
    match resp.pointer("/result/value/data/0").and_then(|v| v.as_str()) {
        Some(b64) => Ok(Some(B64.decode(b64).context("decoding account data")?)),
        None => Ok(None),
    }
}

async fn rpc_call(rpc_url: &str, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": method, "params": params,
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

/// Submit a base64 transaction via direct RPC and wait for confirmation.
/// Used for stealth funding hops and spread buys — everything that is NOT
/// part of the Jito bundle.
pub async fn send_and_confirm_tx(rpc_url: &str, base64_tx: &str) -> Result<String> {
    let resp = rpc_call(
        rpc_url,
        "sendTransaction",
        serde_json::json!([base64_tx, {"encoding": "base64", "preflightCommitment": "confirmed"}]),
    )
    .await?;
    let sig = resp
        .pointer("/result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("no signature in sendTransaction response"))?
        .to_string();

    for _ in 0..45 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let st = rpc_call(
            rpc_url,
            "getSignatureStatuses",
            serde_json::json!([[sig], {"searchTransactionHistory": true}]),
        )
        .await?;
        let val = st.pointer("/result/value/0");
        if let Some(v) = val {
            if v.is_null() {
                continue;
            }
            if let Some(err) = v.get("err") {
                if !err.is_null() {
                    return Err(anyhow!("transaction failed on-chain: {err}"));
                }
            }
            let status = v
                .get("confirmationStatus")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown");
            if status == "confirmed" || status == "finalized" {
                return Ok(sig);
            }
        }
    }
    Err(anyhow!("timed out waiting for confirmation of {sig}"))
}

/// Current slot, for scheduling spread buys (submit at create_slot + offset).
pub async fn fetch_current_slot(rpc_url: &str) -> Result<u64> {
    let resp = rpc_call(rpc_url, "getSlot", serde_json::json!([{"commitment": "confirmed"}])).await?;
    resp.pointer("/result")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("no slot in getSlot response"))
}

/// Lamport balance of an account (confirmed).
pub async fn fetch_balance_lamports(rpc_url: &str, pubkey: &Pubkey) -> Result<u64> {
    let resp = rpc_call(
        rpc_url,
        "getBalance",
        serde_json::json!([pubkey.to_string(), {"commitment": "confirmed"}]),
    )
    .await?;
    resp.pointer("/result/value")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("no balance in getBalance response"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypair_file_roundtrip() {
        // Solana CLI format: 32-byte seed ++ 32-byte pubkey.
        let seed = [42u8; 32];
        let kp = Keypair::new_from_array(seed);
        let mut bytes = seed.to_vec();
        bytes.extend_from_slice(kp.pubkey().as_ref());
        assert_eq!(bytes.len(), 64);
        let json = serde_json::to_string(&bytes).unwrap();
        let dir = std::env::temp_dir();
        let path = dir.join("launch_bundle_test_keypair.json");
        std::fs::write(&path, &json).unwrap();
        let loaded = load_keypair_file(path.to_str().unwrap()).unwrap();
        assert_eq!(loaded.pubkey(), kp.pubkey());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_short_keypair_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("launch_bundle_bad_keypair.json");
        std::fs::write(&path, "[1,2,3]").unwrap();
        assert!(load_keypair_file(path.to_str().unwrap()).is_err());
        std::fs::remove_file(&path).ok();
    }
}
