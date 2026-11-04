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

pub fn check(mint: &AccountInfo) -> Result<()> {
    require!(*mint.key != NATIVE_MINT && *mint.key != NATIVE_MINT_2022, AmmError::UnsupportedMint);
    if mint.owner == &anchor_spl::token::ID { return Ok(()); }
    require!(mint.owner == &anchor_spl::token_2022::ID, AmmError::UnsupportedMint);
    let data = mint.try_borrow_data()?;
    if data.len() == BASE_MINT_LEN { return Ok(()); }
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
