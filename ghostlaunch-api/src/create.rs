//! Pump.fun `create_v2` instruction builder.
//!
//! Verified against the official Pump IDL
//! (https://github.com/pump-fun/pump-public-docs/blob/main/idl/pump.json)
//! and the official COIN_CREATION.md doc:
//!
//! - discriminator: [214, 144, 76, 236, 95, 139, 49, 180]
//! - args (Anchor-encoded): name: String, symbol: String, uri: String,
//!   creator: Pubkey, is_mayhem_mode: bool.
//!   The trailing optional args (is_cashback_enabled, creator_fee_bps,
//!   is_holder_reward) are OMITTED — the official docs state they "may be
//!   left off the end of the instruction data; a missing argument reads as
//!   false / 0". This yields a standard SOL-paired coin: no mayhem mode,
//!   no cashback (deprecated — create_v2 rejects true), standard fee
//!   schedule, regular (non-holder-reward) coin.
//! - accounts in IDL order:
//!     0  mint                     (writable, signer)   <- fresh Token-2022 keypair, decimals=6
//!     1  mint_authority                              <- Pump PDA ["mint-authority"]
//!     2  bonding_curve            (writable)          <- PDA ["bonding-curve", mint]
//!     3  associated_bonding_curve (writable)          <- ATA(bonding_curve, mint) via Token-2022
//!     4  global                                      <- PDA ["global"]
//!     5  user                     (writable, signer) <- payer / creator
//!     6  system_program
//!     7  token_program                               <- Token-2022 (NOT legacy Token)
//!     8  associated_token_program
//!     9  mayhem_program_id        (writable)          <- MAyhSmzXzV1pTf7LsNkrNwkWKTo4ougAJ1PPg47MD4e
//!     10 global_params                               <- Mayhem PDA ["global-params"]
//!     11 sol_vault                (writable)          <- Mayhem PDA ["sol-vault"]
//!     12 mayhem_state             (writable)          <- Mayhem PDA ["mayhem-state", mint]
//!     13 mayhem_token_vault       (writable)          <- ATA(sol_vault, mint) via Token-2022
//!     14 event_authority                             <- PDA ["__event_authority"]
//!     15 program                                     <- pump program id
//!
//! The mint keypair MUST sign (it is created by this instruction) along with
//! the payer, so the create transaction needs two signers.
//!
//! NOTE: the legacy `create` instruction (SPL Token, Metaplex metadata) is
//! dead on current mainnet. `create_v2` is the only supported path.

use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use solana_sniper::{decode, tx_builder};

/// Anchor discriminator for `create_v2`, from the official IDL.
pub const DISC_CREATE_V2: [u8; 8] = [214, 144, 76, 236, 95, 139, 49, 180];

/// Token-2022 program. All `create_v2` mints use this (decimals = 6).
pub fn token_2022_program() -> Pubkey {
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
        .parse()
        .expect("hardcoded Token-2022 program id")
}

/// Mayhem program, pinned by the official COIN_CREATION.md doc.
pub fn mayhem_program_id() -> Pubkey {
    "MAyhSmzXzV1pTf7LsNkrNwkWKTo4ougAJ1PPg47MD4e"
        .parse()
        .expect("hardcoded mayhem program id")
}

/// Mint authority PDA for `create_v2`: ["mint-authority"] under the pump program.
/// (Different from v1, which reused the bonding-curve PDA as mint authority.)
pub fn mint_authority_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"mint-authority"], &decode::pump_program_id()).0
}

/// Mayhem global-params PDA: ["global-params"] under the mayhem program.
pub fn mayhem_global_params_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"global-params"], &mayhem_program_id()).0
}

/// Mayhem sol-vault PDA: ["sol-vault"] under the mayhem program.
pub fn mayhem_sol_vault_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"sol-vault"], &mayhem_program_id()).0
}

/// Mayhem state PDA: ["mayhem-state", mint] under the mayhem program.
pub fn mayhem_state_pda(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"mayhem-state", mint.as_ref()], &mayhem_program_id()).0
}

fn anchor_string(s: &str, out: &mut Vec<u8>) {
    let bytes = s.as_bytes();
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Build the Pump `create_v2` instruction.
///
/// * `mint` — fresh keypair pubkey for the new Token-2022 token (must sign the tx).
/// * `creator` — the creator wallet recorded on-chain (usually == payer).
/// * `payer` — funds the creation, signs the tx.
///
/// Produces a standard SOL-paired coin: mayhem mode off, no cashback,
/// standard fee schedule, regular (non-holder-reward) coin.
pub fn create_instruction(
    mint: &Pubkey,
    creator: &Pubkey,
    payer: &Pubkey,
    name: &str,
    symbol: &str,
    uri: &str,
) -> Instruction {
    let pump = decode::pump_program_id();
    let bonding_curve = decode::bonding_curve_pda(mint);
    let token_program = token_2022_program();
    let mayhem = mayhem_program_id();
    let sol_vault = mayhem_sol_vault_pda();
    let accounts = vec![
        AccountMeta::new(*mint, true),                        // 0 mint (signer)
        AccountMeta::new_readonly(mint_authority_pda(), false), // 1 mint_authority
        AccountMeta::new(bonding_curve, false),               // 2 bonding_curve
        AccountMeta::new(                                     // 3 associated_bonding_curve
            tx_builder::ata_pda(&bonding_curve, &token_program, mint),
            false,
        ),
        AccountMeta::new_readonly(tx_builder::global_pda(), false), // 4 global
        AccountMeta::new(*payer, true),                       // 5 user (signer)
        AccountMeta::new_readonly(tx_builder::system_program_id(), false), // 6
        AccountMeta::new_readonly(token_program, false),      // 7 token_program (Token-2022)
        AccountMeta::new_readonly(tx_builder::ata_program_id(), false),   // 8
        AccountMeta::new(mayhem, false),                      // 9 mayhem_program_id
        AccountMeta::new_readonly(mayhem_global_params_pda(), false), // 10 global_params
        AccountMeta::new(sol_vault, false),                   // 11 sol_vault
        AccountMeta::new(mayhem_state_pda(mint), false),      // 12 mayhem_state
        AccountMeta::new(                                     // 13 mayhem_token_vault
            tx_builder::ata_pda(&sol_vault, &token_program, mint),
            false,
        ),
        AccountMeta::new_readonly(tx_builder::event_authority_pda(), false), // 14
        AccountMeta::new_readonly(pump, false),               // 15 program
    ];

    let mut data = Vec::with_capacity(8 + 128);
    data.extend_from_slice(&DISC_CREATE_V2);
    anchor_string(name, &mut data);
    anchor_string(symbol, &mut data);
    anchor_string(uri, &mut data);
    data.extend_from_slice(creator.as_ref());
    data.push(0); // is_mayhem_mode = false
    // Trailing optionals (is_cashback_enabled, creator_fee_bps,
    // is_holder_reward) intentionally omitted: per the official docs a
    // missing trailing arg reads as false / 0.

    Instruction {
        program_id: pump,
        accounts,
        data,
    }
}

/// All accounts the `create_v2` instruction needs, for pre-flight verification.
#[allow(dead_code)]
pub fn create_account_metas(
    mint: &Pubkey,
    payer: &Pubkey,
) -> Vec<AccountMeta> {
    let bonding_curve = decode::bonding_curve_pda(mint);
    let token_program = token_2022_program();
    let mayhem = mayhem_program_id();
    let sol_vault = mayhem_sol_vault_pda();
    let pump = decode::pump_program_id();
    vec![
        AccountMeta::new(*mint, true),
        AccountMeta::new_readonly(mint_authority_pda(), false),
        AccountMeta::new(bonding_curve, false),
        AccountMeta::new(
            tx_builder::ata_pda(&bonding_curve, &token_program, mint),
            false,
        ),
        AccountMeta::new_readonly(tx_builder::global_pda(), false),
        AccountMeta::new(*payer, true),
        AccountMeta::new_readonly(tx_builder::system_program_id(), false),
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new_readonly(tx_builder::ata_program_id(), false),
        AccountMeta::new(mayhem, false),
        AccountMeta::new_readonly(mayhem_global_params_pda(), false),
        AccountMeta::new(sol_vault, false),
        AccountMeta::new(mayhem_state_pda(mint), false),
        AccountMeta::new(
            tx_builder::ata_pda(&sol_vault, &token_program, mint),
            false,
        ),
        AccountMeta::new_readonly(tx_builder::event_authority_pda(), false),
        AccountMeta::new_readonly(pump, false),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_v2_instruction_matches_idl() {
        let mint: Pubkey = "So11111111111111111111111111111111111111112"
            .parse()
            .unwrap();
        let creator: Pubkey = "11111111111111111111111111111111".parse().unwrap();
        let payer = creator;
        let ix = create_instruction(&mint, &creator, &payer, "T", "T", "u");

        // Program + create_v2 discriminator.
        assert_eq!(ix.program_id, decode::pump_program_id());
        assert_eq!(&ix.data[..8], &DISC_CREATE_V2);

        // 16 accounts in IDL order, with the IDL's writability/signer flags.
        assert_eq!(ix.accounts.len(), 16);
        let flags: Vec<(bool, bool)> = ix
            .accounts
            .iter()
            .map(|m| (m.is_writable, m.is_signer))
            .collect();
        assert_eq!(
            flags,
            vec![
                (true, true),   // mint
                (false, false), // mint_authority
                (true, false),  // bonding_curve
                (true, false),  // associated_bonding_curve
                (false, false), // global
                (true, true),   // user
                (false, false), // system_program
                (false, false), // token_program
                (false, false), // associated_token_program
                (true, false),  // mayhem_program_id
                (false, false), // global_params
                (true, false),  // sol_vault
                (true, false),  // mayhem_state
                (true, false),  // mayhem_token_vault
                (false, false), // event_authority
                (false, false), // program
            ]
        );

        // Fixed addresses pinned by the IDL / official docs.
        assert_eq!(
            ix.accounts[7].pubkey.to_string(),
            "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
        );
        assert_eq!(
            ix.accounts[9].pubkey.to_string(),
            "MAyhSmzXzV1pTf7LsNkrNwkWKTo4ougAJ1PPg47MD4e"
        );
        // No Metaplex metadata accounts in v2.
        assert!(!ix
            .accounts
            .iter()
            .any(|m| m.pubkey.to_string()
                == "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s"));

        // Args decode back: name/symbol/uri/creator/is_mayhem_mode=false,
        // with trailing optionals omitted.
        let mut off = 8;
        for expected in ["T", "T", "u"] {
            let len =
                u32::from_le_bytes(ix.data[off..off + 4].try_into().unwrap()) as usize;
            off += 4;
            assert_eq!(&ix.data[off..off + len], expected.as_bytes());
            off += len;
        }
        assert_eq!(&ix.data[off..off + 32], creator.as_ref());
        off += 32;
        assert_eq!(ix.data[off], 0); // is_mayhem_mode = false
        assert_eq!(ix.data.len(), off + 1); // nothing after: optionals omitted
    }

    #[test]
    fn mayhem_pdas_are_deterministic() {
        let mint: Pubkey = "So11111111111111111111111111111111111111112"
            .parse()
            .unwrap();
        assert_eq!(mayhem_global_params_pda(), mayhem_global_params_pda());
        assert_eq!(mayhem_sol_vault_pda(), mayhem_sol_vault_pda());
        assert_eq!(mayhem_state_pda(&mint), mayhem_state_pda(&mint));
        assert_eq!(mint_authority_pda(), mint_authority_pda());
    }

    #[test]
    fn mayhem_token_vault_is_ata_of_sol_vault() {
        let mint: Pubkey = "So11111111111111111111111111111111111111112"
            .parse()
            .unwrap();
        let sol_vault = mayhem_sol_vault_pda();
        let expected =
            tx_builder::ata_pda(&sol_vault, &token_2022_program(), &mint);
        let ix = create_instruction(&mint, &mint, &mint, "T", "T", "u");
        assert_eq!(ix.accounts[13].pubkey, expected);
    }
}
