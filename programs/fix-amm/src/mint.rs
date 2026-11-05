//! Which mints a pool can hold: those whose transfers move exactly the amount named, which no third party
//! can take from the vault, and which stay the same mint for the pool's life.
use anchor_lang::prelude::*;
use anchor_lang::solana_program::pubkey;
use crate::AmmError;

const NATIVE_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
const NATIVE_MINT_2022: Pubkey = pubkey!("9pan9bMn5HatX4EJdBwg9VgCa7Uz5HL8N1m5D3NdXejP");
/// Token-2022 mint extensions that leave a transfer's amount and the vault untouched: metadata and its
/// pointer, group and member records and their pointers, and the display-only interest and UI scale.
/// Transfer fees and hooks, permanent delegates, default frozen accounts, pausing, confidential
/// transfers, close authorities and every type added later are refused.
pub const ALLOWED_EXTENSIONS: [u16; 8] = [10, 18, 19, 20, 21, 22, 23, 25];
const BASE_MINT_LEN: usize = 82;
/// Token-2022 pads the base mint to an account's length, then writes the account type and its TLV entries.
const ACCOUNT_TYPE_OFFSET: usize = 165;
const MINT_ACCOUNT_TYPE: u8 = 1;
/// The base mint's `freeze_authority` option tag (`COption<Pubkey>`: a u32 tag, then the key), the same in both programs.
const FREEZE_AUTHORITY_TAG: core::ops::Range<usize> = 46..50;
/// USDC and USDT keep their issuers' freeze authorities; they are the only mints accepted with one, and the app
/// shows that authority on every pool and pair that holds them.
pub const FREEZE_ALLOWED: [Pubkey; 2] = [pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"), pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB")];

pub fn check(mint: &AccountInfo) -> Result<()> {
    require!(*mint.key != NATIVE_MINT && *mint.key != NATIVE_MINT_2022, AmmError::UnsupportedMint);
    require!(mint.owner == &anchor_spl::token::ID || mint.owner == &anchor_spl::token_2022::ID, AmmError::UnsupportedMint);
    let data = mint.try_borrow_data()?;
    // A freeze authority can freeze the pool's vault, which stops every swap and every withdrawal.
    require!(data.len() >= BASE_MINT_LEN && (data[FREEZE_AUTHORITY_TAG] == [0u8; 4] || FREEZE_ALLOWED.contains(mint.key)), AmmError::UnsupportedMint);
    if mint.owner == &anchor_spl::token::ID || data.len() == BASE_MINT_LEN { return Ok(()); }
    require!(data.len() > ACCOUNT_TYPE_OFFSET && data[ACCOUNT_TYPE_OFFSET] == MINT_ACCOUNT_TYPE, AmmError::UnsupportedMint);
    let mut at = ACCOUNT_TYPE_OFFSET + 1;
    while at + 4 <= data.len() {
        let kind = u16::from_le_bytes([data[at], data[at + 1]]);
        if kind == 0 { break; }
        require!(ALLOWED_EXTENSIONS.contains(&kind), AmmError::UnsupportedMint);
        at += 4 + u16::from_le_bytes([data[at + 2], data[at + 3]]) as usize;
    }
    Ok(())
}
