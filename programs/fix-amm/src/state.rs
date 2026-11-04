use anchor_lang::prelude::*;
use crate::{math, AmmError};

/// A pool's fee: `fee_bps` of the SOL side of every swap, of which `protocol_share_bps` goes to the
/// protocol and the rest stays in the pool for its liquidity providers.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, InitSpace, Debug, PartialEq, Eq)]
pub struct Terms {
    pub fee_bps: u16,
    pub protocol_share_bps: u16,
}
impl Terms {
    pub fn validate(&self) -> Result<()> {
        require!((self.fee_bps as u64) < math::BPS && (self.protocol_share_bps as u64) <= math::BPS, AmmError::InvalidTerms);
        Ok(())
    }
}

/// The template new pools copy their terms from. Changing it reaches only pools created later.
#[account]
#[derive(InitSpace)]
pub struct Config {
    pub revision: u64,
    pub admin: Pubkey,
    pub pending_admin: Pubkey,
    /// Stops new pools only; live pools trade and take liquidity either way.
    pub paused: bool,
    pub terms: Terms,
    pub bump: u8,
}

/// Protocol fees accrue across this many `["protocol", shard]` PDAs, so pools never share one write lock.
pub const PROTOCOL_SHARDS: u8 = 64;

#[account]
#[derive(InitSpace)]
pub struct Protocol { pub accrued: u64, pub shard: u8, pub bump: u8 }

/// One pool per mint against SOL. SOL reserves sit in this account above its rent, tokens in its vault.
#[account]
#[derive(InitSpace)]
pub struct Pool {
    pub mint: Pubkey,
    /// SPL Token or Token-2022: the program that owns the mint and the vault.
    pub token_program: Pubkey,
    pub creator: Pubkey,
    /// Copied from the config at creation; nothing changes them afterwards.
    pub terms: Terms,
    pub sol_reserve: u64,
    pub token_reserve: u64,
    /// Shares outstanding across every position.
    pub shares: u64,
    pub created_slot: u64,
    /// The slot liquidity last went into the empty pool. Swaps open in the slot after it.
    pub seeded_slot: u64,
    pub receipt_sequence: u64,
    pub bump: u8,
    pub vault_bump: u8,
}
impl Pool {
    pub fn price(&self) -> u128 { math::spot(self.sol_reserve, self.token_reserve) }
}

/// A wallet's shares in one pool. There is no LP token: shares live here and move only by add and remove.
#[account]
#[derive(InitSpace)]
pub struct Position {
    pub pool: Pubkey,
    pub owner: Pubkey,
    pub shares: u64,
    pub bump: u8,
}

pub const CREATE: u8 = 0;
pub const BUY: u8 = 1;
pub const SELL: u8 = 2;
pub const ADD: u8 = 3;
pub const REMOVE: u8 = 4;

#[event]
pub struct PoolCreated {
    pub pool: Pubkey,
    pub mint: Pubkey,
    pub token_program: Pubkey,
    pub creator: Pubkey,
    pub terms: Terms,
    pub decimals: u8,
    pub created_slot: u64,
}

/// Every pool action's itemized receipt. On a buy `gross` is the SOL paid and `net` the part that bought
/// tokens; on a sell `gross` is the tokens' SOL value and `net` the SOL paid out. `gross` and `net` are the
/// SOL deposited or withdrawn on create, add and remove. The reserves and shares are those after the action.
#[event]
pub struct Receipt {
    pub pool: Pubkey,
    pub actor: Pubkey,
    pub action: u8,
    pub sequence: u64,
    pub timestamp: i64,
    pub gross: u64,
    pub protocol_fee: u64,
    pub lp_fee: u64,
    pub net: u64,
    pub tokens: u64,
    pub shares: u64,
    pub total_shares: u64,
    pub sol_reserve: u64,
    pub token_reserve: u64,
    pub price_before: u128,
    pub price_after: u128,
    pub price_impact_bps: i64,
}
