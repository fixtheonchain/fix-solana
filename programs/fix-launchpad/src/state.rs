use anchor_lang::prelude::*;
use crate::{LaunchError, math::{self, BPS}};

pub const CURVE: u8 = 0;
pub const POOL: u8 = 1;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, InitSpace, Debug)]
pub struct Terms {
    pub total_supply: u64,
    pub decimals: u8,
    pub virtual_sol: u64,
    pub virtual_token: u64,
    pub curve_tokens: u64,
    pub curve_fee_bps: u16,
    pub pool_fee_bps: u16,
    pub protocol_share_bps: u16,
    pub creator_share_bps: u16,
    pub pool_share_bps: u16,
}
impl Terms {
    pub fn validate(&self) -> Result<()> {
        require!(self.total_supply >= self.virtual_token && self.virtual_token > self.curve_tokens && self.curve_tokens > 0 && self.virtual_sol > 0, LaunchError::InvalidTerms);
        require!(self.decimals <= 18 && self.curve_fee_bps < 10_000 && self.pool_fee_bps < 10_000, LaunchError::InvalidTerms);
        require!(self.protocol_share_bps as u32 + self.creator_share_bps as u32 + self.pool_share_bps as u32 == 10_000, LaunchError::InvalidTerms);
        let initial_price=math::spot(self.virtual_sol,self.virtual_token as u128,1)?;
        let maximum_fee=self.curve_fee_bps.max(self.pool_fee_bps) as u64;
        require!(math::wide_div(math::U256::from(initial_price)*math::U256::from(BPS-maximum_fee),math::U256::from(BPS),false)?>0,LaunchError::InvalidTerms);
        // A launch must at least be able to complete its initial curve within
        // the same u64 amount domain used by Solana transfers and token supply.
        let completion_net=math::curve_buy_net(self.virtual_sol,self.virtual_token,self.curve_tokens).map_err(|_|error!(LaunchError::InvalidTerms))?;
        let completion_rate=self.curve_fee_bps;
        let completion_gross=math::gross_for_net(completion_net,completion_rate).map_err(|_|error!(LaunchError::InvalidTerms))?;
        let completion_fees=math::fees(completion_gross,completion_rate,self)?;
        require!((self.virtual_sol as u128)+(completion_net as u128)+(completion_fees.pool as u128)<=u64::MAX as u128,LaunchError::InvalidTerms);
        Ok(())
    }
}

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub revision: u64,
    pub admin: Pubkey,
    pub pending_admin: Pubkey,
    pub paused: bool,
    pub terms: Terms,
    pub bump: u8,
}

/// Protocol fees accrue across this many `["protocol", shard]` PDAs. A trade
/// may credit any shard, so markets never share one write lock.
pub const PROTOCOL_SHARDS: u8 = 64;

#[account]
#[derive(InitSpace)]
pub struct Protocol { pub accrued: u64, pub shard: u8, pub bump: u8 }

/// The PDA an extension program signs routed trades with, derived under that program.
pub const EXTENSION_AUTHORITY_SEED: &[u8] = b"fix-extension";

/// A registered extension: a separate program that routes trades into the core by CPI
/// and receives `share_bps` of the protocol's part of each routed trade's fee.
/// The core never calls out; pausing or removing an entry stops new routed trades only.
#[account]
#[derive(InitSpace)]
pub struct Extension {
    pub program: Pubkey,
    /// `["fix-extension"]` under `program`; only that program can sign as it.
    pub authority: Pubkey,
    /// The account credited with the extension's share.
    pub recipient: Pubkey,
    pub share_bps: u16,
    pub paused: bool,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Market {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub terms: Terms,
    pub phase: u8,
    pub virtual_sol: u64,
    pub virtual_token: u64,
    pub curve_remaining: u64,
    pub real_sol: u64,
    pub pool_token_num: u128,
    pub pool_token_den: u64,
    pub pool_tokens: u64,
    pub burned_tokens: u64,
    pub creator_fees: u64,
    pub receipt_sequence: u64,
    pub bump: u8,
    pub vault_bump: u8,
    /// Immutable creation slot. Every buyer waits until a strictly later slot.
    pub created_slot: u64,
}
impl Market {
    pub fn rate(&self) -> u16 { if self.phase == POOL { self.terms.pool_fee_bps } else { self.terms.curve_fee_bps } }
    pub fn price(&self) -> Result<u128> {
        if self.phase == POOL { math::spot(self.real_sol, self.pool_token_num, self.pool_token_den) }
        else { math::spot(self.virtual_sol, self.virtual_token as u128, 1) }
    }

}

#[event]
pub struct Receipt {
    pub market: Pubkey,
    pub actor: Pubkey,
    pub action: u8,
    pub phase: u8,
    pub sequence: u64,
    pub timestamp: i64,
    pub is_creator: bool,
    pub gross: u64,
    pub protocol_fee: u64,
    pub creator_fee: u64,
    pub pool_fee: u64,
    pub net: u64,
    pub tokens: u64,
    pub refund: u64,
    pub price_before: u128,
    pub price_after: u128,
    pub price_impact_bps: i64,
    /// The extension that routed this trade, or the default key for none.
    pub extension: Pubkey,
    /// The extension's line, paid out of the protocol's part: protocol_fee + extension_fee is the protocol share.
    pub extension_fee: u64,
}

#[event]
pub struct CoinCreated { pub market: Pubkey, pub mint: Pubkey, pub creator: Pubkey, pub terms: Terms, pub name: String, pub symbol: String, pub uri: String }
#[event]
pub struct Graduated { pub market: Pubkey, pub real_sol: u64, pub token_reserve: u64, pub token_numerator: u128, pub token_denominator: u64, pub burned: u64, pub last_curve_price: u128, pub pool_price: u128 }
