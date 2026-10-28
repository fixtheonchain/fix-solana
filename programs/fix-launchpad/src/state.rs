use anchor_lang::prelude::*;
use crate::{LaunchError, math::{self, PRICE_SCALE, BPS}};

/// One fixed account holds both edges of every window up to four hours.
pub const PRICE_HISTORY_CAPACITY: usize = 241;
pub const PRICE_MINUTE_SECONDS: i64 = 60;
pub const PRICE_HISTORY_RETENTION_SECONDS: i64 = (PRICE_HISTORY_CAPACITY as i64 - 1) * PRICE_MINUTE_SECONDS;
/// Technical storage bounds, not default economic terms. Even the longest
/// allowed scoring window leaves two hours for permissionless checkpointing.
pub const MAX_SCORING_WINDOW_SECONDS: i64 = 7200;
pub const MIN_SETTLEMENT_MARGIN_SECONDS: i64 = 7200;
const _: () = assert!(MAX_SCORING_WINDOW_SECONDS + MIN_SETTLEMENT_MARGIN_SECONDS <= PRICE_HISTORY_RETENTION_SECONDS);
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
    /// Fraction of the protocol share; never added to the trader's fee.
    pub call_share_bps: u16,
    pub min_horizon: i64,
    pub max_horizon: i64,
    pub scoring_window: i64,
}
impl Terms {
    pub fn validate(&self) -> Result<()> {
        require!(self.total_supply >= self.virtual_token && self.virtual_token > self.curve_tokens && self.curve_tokens > 0 && self.virtual_sol > 0, LaunchError::InvalidTerms);
        require!(self.decimals <= 18 && self.curve_fee_bps < 10_000 && self.pool_fee_bps < 10_000, LaunchError::InvalidTerms);
        require!(self.protocol_share_bps as u32 + self.creator_share_bps as u32 + self.pool_share_bps as u32 == 10_000 && self.call_share_bps <= 10_000, LaunchError::InvalidTerms);
        require!(self.scoring_window > 0 && self.scoring_window % PRICE_MINUTE_SECONDS == 0 && self.scoring_window <= MAX_SCORING_WINDOW_SECONDS && self.min_horizon >= self.scoring_window && self.max_horizon.checked_sub(self.min_horizon).is_some_and(|width| width >= PRICE_MINUTE_SECONDS - 1), LaunchError::InvalidTerms);
        let initial_price=math::spot(self.virtual_sol,self.virtual_token as u128,1)?;
        let maximum_fee=self.curve_fee_bps.max(self.pool_fee_bps) as u64;
        require!(math::wide_div(math::U256::from(initial_price)*math::U256::from(BPS-maximum_fee),math::U256::from(BPS),false)?>0,LaunchError::InvalidTerms);
        // A launch must at least be able to complete its initial curve within
        // the same u64 amount domain used by Solana transfers and token supply.
        let completion_net=math::curve_buy_net(self.virtual_sol,self.virtual_token,self.curve_tokens).map_err(|_|error!(LaunchError::InvalidTerms))?;
        let completion_rate=self.curve_fee_bps;
        let completion_gross=math::gross_for_net(completion_net,completion_rate).map_err(|_|error!(LaunchError::InvalidTerms))?;
        let completion_fees=math::fees(completion_gross,completion_rate,self,false)?;
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

#[account]
#[derive(InitSpace)]
pub struct Protocol { pub accrued: u64, pub bump: u8 }

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
    pub oracle_count: u64,
    pub oracle_timestamp: i64,
    pub oracle_exit_price: u128,
    pub oracle_cumulative_lo: u128,
    pub oracle_cumulative_hi: u128,
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
    pub fn exit_price(&self) -> Result<u128> {
        math::wide_div(math::U256::from(self.price()?) * math::U256::from(BPS - self.rate() as u64), math::U256::from(BPS), false)
    }
    pub fn oracle_cumulative(&self) -> math::U256 {
        (math::U256::from(self.oracle_cumulative_hi)<<128) | math::U256::from(self.oracle_cumulative_lo)
    }
}

/// Explicit u64 limbs give identical eight-byte alignment on host and SBF.
#[zero_copy]
#[repr(C)]
#[derive(Debug)]
pub struct MinuteObservation {
    pub minute: i64,
    pub cumulative_lo: [u64; 2],
    pub cumulative_hi: [u64; 2],
}
impl MinuteObservation {
    pub fn cumulative(&self) -> math::U256 {
        math::U256([self.cumulative_lo[0], self.cumulative_lo[1], self.cumulative_hi[0], self.cumulative_hi[1]])
    }
    pub fn new(minute:i64, cumulative:math::U256) -> Self {
        Self {minute,cumulative_lo:[cumulative.0[0],cumulative.0[1]],cumulative_hi:[cumulative.0[2],cumulative.0[3]]}
    }
}

/// Created once with the market. Trades borrow this data in place and never
/// allocate, resize, or depend on a keeper. Every retained record is the exact
/// cumulative price at a UTC minute boundary, including crossed quiet minutes.
#[account(zero_copy)]
#[repr(C)]
pub struct PriceHistory {
    pub market: Pubkey,
    pub latest_minute: i64,
    pub count: u64,
    pub bump: u8,
    pub padding: [u8; 7],
    pub observations: [MinuteObservation; PRICE_HISTORY_CAPACITY],
}
impl PriceHistory {
    pub const SPACE: usize = 8 + core::mem::size_of::<Self>();
}
const _: () = assert!(PriceHistory::SPACE == 9704 && PriceHistory::SPACE <= 10240);

/// A small durable replay marker scoped to one caller and market. It cannot
/// close, so closing a Call never allows the same address to be initialized
/// again. Other callers never change this sequence.
#[account]
#[derive(InitSpace)]
pub struct CallerSequence {
    pub market: Pubkey,
    pub caller: Pubkey,
    pub next_id: u64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Call {
    pub market: Pubkey,
    pub caller: Pubkey,
    pub id: u64,
    pub opened_at: i64,
    pub window_start: i64,
    pub horizon: i64,
    pub price_integral: u128,
    /// High half of the 256-bit integral. Low-half compatibility does not limit
    /// how long an otherwise valid high-price market can remain observable.
    pub price_integral_hi: u128,
    pub checkpointed: bool,
    /// A missing historical edge permanently selects the follower refund path.
    pub evidence_missing: bool,
    pub position_count: u64,
    pub follower_count: u64,
    pub settled_positions: u64,
    pub earned: u64,
    pub refunded: u64,
    pub claimable_payout: u64,
    pub bump: u8,
}
impl Call {
    pub fn integral(&self) -> math::U256 {
        (math::U256::from(self.price_integral_hi) << 128) | math::U256::from(self.price_integral)
    }
    pub fn average(&self, window:i64) -> Result<u128> {
        math::wide_div(self.integral(),math::U256::from(window as u128),false)
    }
    fn mark(&self, tokens: u64, window: i64) -> Result<math::U256> {
        let numerator=math::U256::from(tokens).checked_mul(self.integral()).ok_or_else(||error!(LaunchError::MathOverflow))?;
        Ok(numerator / (math::U256::from(window as u128) * math::U256::from(PRICE_SCALE)))
    }
    pub fn value(&self, tokens: u64, window: i64) -> Result<u64> {
        let value=self.mark(tokens,window)?;
        // Cost is u64. Any greater mark is necessarily profitable; cap only the
        // displayed mark rather than blocking the escrow payout.
        Ok(if value>math::U256::from(u64::MAX) {u64::MAX}else{value.low_u64()})
    }
    pub fn is_profitable(&self, tokens:u64, cost:u64, window:i64) -> Result<bool> {
        // Compare before display saturation, including cost == u64::MAX.
        Ok(self.mark(tokens,window)?>math::U256::from(cost))
    }
}

#[account]
#[derive(InitSpace)]
pub struct CallPosition {
    pub call: Pubkey,
    pub market: Pubkey,
    pub follower: Pubkey,
    pub caller: Pubkey,
    pub horizon: i64,
    pub rent_payer: Pubkey,
    pub cost: u64,
    pub tokens: u64,
    pub escrow: u64,
    pub has_activity: bool,
    pub settled: bool,
    pub profitable: bool,
    pub exit_value: u64,
    pub bump: u8,
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
    pub call_fee: u64,
    pub pool_fee: u64,
    pub net: u64,
    pub tokens: u64,
    pub refund: u64,
    pub price_before: u128,
    pub price_after: u128,
    pub price_impact_bps: i64,
    pub call: Pubkey,
}

#[event]
pub struct CoinCreated { pub market: Pubkey, pub mint: Pubkey, pub creator: Pubkey, pub terms: Terms, pub name: String, pub symbol: String, pub uri: String }
#[event]
pub struct Graduated { pub market: Pubkey, pub real_sol: u64, pub token_reserve: u64, pub token_numerator: u128, pub token_denominator: u64, pub burned: u64, pub last_curve_price: u128, pub pool_price: u128 }
#[event]
pub struct CallOpened { pub call: Pubkey, pub market: Pubkey, pub caller: Pubkey, pub window_start: i64, pub horizon: i64 }
#[event]
pub struct CallSettled { pub call: Pubkey, pub follower: Pubkey, pub caller: Pubkey, pub profitable: bool, pub cost: u64, pub tokens: u64, pub exit_value: u64, pub payout: u64, pub price_integral: u128, pub price_integral_hi: u128, pub payout_deferred: bool, pub evidence_missing: bool }
#[event]
pub struct CallCheckpointed { pub call: Pubkey, pub window_start: i64, pub horizon: i64, pub price_integral: u128, pub price_integral_hi: u128, pub evidence_missing: bool }
#[event]
pub struct CallClosed { pub call: Pubkey, pub caller: Pubkey, pub position_count: u64, pub earned: u64, pub refunded: u64, pub payout: u64 }
#[event]
pub struct CallPayoutClaimed { pub call: Pubkey, pub caller: Pubkey, pub payout: u64 }
#[event]
pub struct PriceRecorded { pub market: Pubkey, pub index: u64, pub timestamp: i64, pub cumulative_lo: u128, pub cumulative_hi: u128, pub exit_price: u128 }
#[event]
pub struct PositionClosed { pub call: Pubkey, pub follower: Pubkey, pub rent_payer: Pubkey }
