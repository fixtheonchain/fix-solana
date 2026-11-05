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

/// A buy right after a coin opens pays `start_bps`, falling in `steps` equal steps of `step_slots` slots each to the
/// coin's own fee rate. All zero means none. Only buys pay it; the same slot gives every buyer the same rate.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct Snipe { pub start_bps: u16, pub step_slots: u16, pub steps: u16 }
/// The longest schedule: about an hour of slots.
pub const MAX_SNIPE_SLOTS: u32 = 9_000;
impl Snipe {
    pub fn validate(&self) -> Result<()> {
        if *self == Snipe::default() { return Ok(()); }
        require!(self.start_bps < 10_000 && self.step_slots > 0 && self.steps > 0 && self.step_slots as u32 * self.steps as u32 <= MAX_SNIPE_SLOTS, LaunchError::InvalidTerms);
        Ok(())
    }
    /// The buy rate `elapsed` slots after the coin's first tradable slot, never below `base`.
    pub fn rate(&self, base: u16, elapsed: u64) -> u16 {
        if self.steps == 0 || self.start_bps <= base { return base; }
        let step = elapsed / self.step_slots as u64;
        if step >= self.steps as u64 { return base; }
        base + ((self.start_bps - base) as u64 * (self.steps as u64 - step) / self.steps as u64) as u16
    }
}

/// A market's data ends at its `Market` struct; terms added after launch follow it (`MarketExt`).
pub const MARKET_BASE_LEN: usize = 8 + Market::INIT_SPACE;
/// Room every market created since the anti-sniping upgrade reserves for its later terms.
pub const MARKET_EXT_LEN: usize = 32;
/// The config's data ends at its `Config` struct; the launch terms added later follow it (`ConfigExt`).
pub const CONFIG_BASE_LEN: usize = 8 + Config::INIT_SPACE;

/// Per-coin terms added after launch, written once at creation right after the market's `Market` data and never
/// changed. Coins created before carry none and read as all zero: no schedule, the standard type, no extra fee.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct MarketExt { pub version: u8, pub launch_type: u8, pub snipe: Snipe, pub extra_fee_bps: u16 }
/// The launch terms the config holds for coins not yet created, after its `Config` data.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct ConfigExt { pub version: u8, pub snipe: Snipe }

/// Reads the bytes after `base`, zero-filled to `T`'s size, so a shorter or missing tail reads as defaults.
fn read_tail<T: AnchorDeserialize>(info: &AccountInfo, base: usize) -> Result<T> {
    let data = info.try_borrow_data()?; let mut buffer = [0u8; 64];
    if data.len() > base { let tail = &data[base..]; let n = tail.len().min(buffer.len()); buffer[..n].copy_from_slice(&tail[..n]); }
    T::deserialize(&mut &buffer[..]).map_err(|_| error!(LaunchError::InvalidTerms))
}
fn write_tail<T: AnchorSerialize>(info: &AccountInfo, base: usize, value: &T) -> Result<()> {
    let bytes = value.try_to_vec()?; let mut data = info.try_borrow_mut_data()?;
    require!(data.len() >= base + bytes.len(), LaunchError::InvalidTerms);
    data[base..base + bytes.len()].copy_from_slice(&bytes); Ok(())
}
impl MarketExt {
    pub fn read(market: &AccountInfo) -> Result<Self> { read_tail(market, MARKET_BASE_LEN) }
    pub fn write(&self, market: &AccountInfo) -> Result<()> { write_tail(market, MARKET_BASE_LEN, self) }
}
impl ConfigExt {
    pub fn read(config: &AccountInfo) -> Result<Self> { read_tail(config, CONFIG_BASE_LEN) }
    pub fn write(&self, config: &AccountInfo) -> Result<()> { write_tail(config, CONFIG_BASE_LEN, self) }
}

/// Receipts before the anti-sniping upgrade. Kept so their transactions still decode.
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

/// Every receipt since the anti-sniping upgrade: `Receipt`'s lines, plus the anti-sniping part of a buy's fee
/// (inside `pool_fee`, which keeps it in the coin's reserves) and the creator's extra fee.
#[event]
pub struct ReceiptV2 {
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
    pub extension: Pubkey,
    pub extension_fee: u64,
    /// The part of `pool_fee` above the coin's own rate: what the anti-sniping schedule charged this buy.
    pub snipe_fee: u64,
    /// The creator's extra fee line.
    pub extra_fee: u64,
}

#[event]
pub struct CoinCreated { pub market: Pubkey, pub mint: Pubkey, pub creator: Pubkey, pub terms: Terms, pub name: String, pub symbol: String, pub uri: String }
#[event]
pub struct Graduated { pub market: Pubkey, pub real_sol: u64, pub token_reserve: u64, pub token_numerator: u128, pub token_denominator: u64, pub burned: u64, pub last_curve_price: u128, pub pool_price: u128 }
