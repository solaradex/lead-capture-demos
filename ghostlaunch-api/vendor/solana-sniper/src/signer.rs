//! Isolated wallet signer.
//!
//! Loads a single hot-wallet keypair from the environment and keeps it in
//! memory for transaction signing. Security rules:
//! - The secret never appears in logs, errors, or serialized output.
//! - Only `WALLET_PRIVATE_KEY` (base58, 64-byte secret) is read.
//! - Use a dedicated hot wallet holding only session risk capital.

use std::env;

use anyhow::{anyhow, Context, Result};
use solana_keypair::Keypair;use solana_pubkey::Pubkey;
use solana_signer::Signer;

/// In-memory hot-wallet signer.
pub struct WalletSigner {
    keypair: Keypair,
}

impl WalletSigner {
    /// Load from `WALLET_PRIVATE_KEY` (base58-encoded 64-byte secret key).
    pub fn from_env() -> Result<Self> {
        let raw = env::var("WALLET_PRIVATE_KEY")
            .context("WALLET_PRIVATE_KEY is not set")?;
        Self::from_base58(&raw)
    }

    /// Build from a base58-encoded 64-byte secret key.
    pub fn from_base58(secret_bs58: &str) -> Result<Self> {
        let keypair = Keypair::try_from_base58_string(secret_bs58.trim())
            .map_err(|e| anyhow!("invalid WALLET_PRIVATE_KEY: {e}"))?;
        Ok(Self { keypair })
    }

    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    pub(crate) fn keypair(&self) -> &Keypair {
        &self.keypair
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_length() {
        assert!(WalletSigner::from_base58("3b6a27bcceb6a42d62a3a8d483a7f926").is_err());
    }

    #[test]
    fn rejects_non_base58() {
        assert!(WalletSigner::from_base58("not!valid!base58!!!").is_err());
    }
}
