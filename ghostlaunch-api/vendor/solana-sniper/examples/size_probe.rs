// Offline size probe: replicate the EXACT instruction sequence that
// build_pumpswap_buy_with_wrap produces (wrap SOL -> sync native ->
// create base ATA -> buy_v2), then measure the serialized v0 transaction
// against Jito's 1232-byte atomic-bundle transaction limit.
use base64::Engine as _;
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::str::FromStr;

use solana_sniper::pumpswap_tx::{
    buy_v2_instruction, legacy_token_program_id, pumpswap_ata, PumpSwapAccounts,
};
use solana_sniper::tx_builder::{
    assemble_signed_base64, create_ata_instruction, system_program_id,
    system_transfer_instruction, TxConfig,
};

fn main() {
    let kp = Keypair::new();
    let user = kp.pubkey();
    let pool = Pubkey::new_unique();
    let base_mint = Pubkey::new_unique();
    let wsol =
        Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
    let spl = legacy_token_program_id();
    let base_token_program =
        Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();

    let user_quote_ata = pumpswap_ata(&user, &spl, &wsol);
    let user_base_ata = pumpswap_ata(&user, &base_token_program, &base_mint);

    let accts = PumpSwapAccounts {
        pool,
        user,
        base_mint,
        quote_mint: wsol,
        base_token_program,
        quote_token_program: spl,
        buyback_fee_recipient: Pubkey::new_unique(),
        fee_config: None,
        pool_base_token_account: Some(Pubkey::new_unique()),
        pool_quote_token_account: Some(Pubkey::new_unique()),
    };

    // Exact sequence from build_pumpswap_buy_with_wrap:
    // 1. create wSOL ATA (assume missing) 2. transfer SOL -> wSOL ATA
    // 3. sync_native 4. create base ATA (assume missing) 5. buy_v2
    let sync_native = Instruction {
        program_id: spl,
        accounts: vec![AccountMeta::new(user_quote_ata, false)],
        data: vec![17],
    };
    let ixs = vec![
        create_ata_instruction(&user, &user_quote_ata, &user, &wsol, &spl),
        system_transfer_instruction(&user, &user_quote_ata, 10_000_000),
        sync_native,
        create_ata_instruction(
            &user,
            &user_base_ata,
            &user,
            &base_mint,
            &base_token_program,
        ),
        buy_v2_instruction(&accts, 1_000_000, 10_000_000),
    ];

    let b64 = assemble_signed_base64(
        &kp,
        ixs,
        &TxConfig {
            blockhash: Hash::new_from_array([1u8; 32]),
            compute_unit_limit: 300_000,
            compute_unit_price_microlamports: 50_000,
        },
    )
    .expect("assemble buy tx");
    let raw =
        base64::engine::general_purpose::STANDARD.decode(&b64).unwrap();
    println!("buy tx serialized size: {} bytes", raw.len());
    println!("Jito atomic-bundle limit: 1232 bytes");
    println!(
        "verdict: {}",
        if raw.len() > 1232 {
            "EXCEEDS LIMIT -> Jito 400"
        } else {
            "under limit -> size is NOT the 400 cause"
        }
    );
}
