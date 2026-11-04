//! Exact integer pool math. The SDK mirrors every routine here, rounding included, and every
//! rounding falls on the pool's side.
use anchor_lang::prelude::*;
use crate::{AmmError, Terms};
mod wide { uint::construct_uint! { pub struct U256(4); } }
pub use wide::U256;

pub const BPS: u64 = 10_000;

pub fn add(a: u64, b: u64) -> Result<u64> { a.checked_add(b).ok_or(error!(AmmError::MathOverflow)) }
pub fn sub(a: u64, b: u64) -> Result<u64> { a.checked_sub(b).ok_or(error!(AmmError::InsufficientLiquidity)) }
pub fn to_u64(v: u128) -> Result<u64> { u64::try_from(v).map_err(|_| error!(AmmError::MathOverflow)) }
pub fn mul_div(a: u64, b: u64, d: u64) -> Result<u64> {
    require!(d > 0, AmmError::MathOverflow);
    to_u64((a as u128) * (b as u128) / d as u128)
}
pub fn mul_div_ceil(a: u64, b: u64, d: u64) -> Result<u64> {
    require!(d > 0, AmmError::MathOverflow);
    let n = (a as u128) * (b as u128);
    to_u64(n / d as u128 + u128::from(n % d as u128 != 0))
}

/// One fee with two lines: the protocol's part rounds down and the liquidity providers keep the rest.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Fees { pub total: u64, pub protocol: u64, pub lp: u64 }

pub fn fees(gross: u64, terms: &Terms) -> Result<Fees> {
    let total = mul_div(gross, terms.fee_bps as u64, BPS)?;
    let protocol = mul_div(total, terms.protocol_share_bps as u64, BPS)?;
    Ok(Fees { total, protocol, lp: total - protocol })
}

/// The smallest gross whose amount after the fee covers `net`. A binary search keeps fee rounding exact.
pub fn gross_for_net(net: u64, rate: u16) -> Result<u64> {
    require!((rate as u64) < BPS, AmmError::InvalidTerms);
    if net == 0 { return Ok(0); }
    require!(u64::MAX - mul_div(u64::MAX, rate as u64, BPS)? >= net, AmmError::MathOverflow);
    let mut lo = net;
    let ceiling = (net as u128 * BPS as u128).div_ceil((BPS - rate as u64) as u128);
    let mut hi = ceiling.min(u64::MAX as u128) as u64;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if mid - mul_div(mid, rate as u64, BPS)? >= net { hi = mid; } else { lo = mid + 1; }
    }
    Ok(lo)
}

/// Tokens out for `net` SOL into reserves `x` SOL and `y` tokens.
pub fn buy_out(x: u64, y: u64, net: u64) -> Result<u64> {
    to_u64((y as u128) * (net as u128) / add(x, net)? as u128)
}
/// SOL into the reserves that buys exactly `out` tokens.
pub fn buy_net(x: u64, y: u64, out: u64) -> Result<u64> {
    require!(out < y, AmmError::InsufficientLiquidity);
    to_u64(((x as u128) * (out as u128)).div_ceil((y - out) as u128))
}
/// SOL out of the reserves, before the fee, for `input` tokens.
pub fn sell_out(x: u64, y: u64, input: u64) -> Result<u64> {
    to_u64((x as u128) * (input as u128) / add(y, input)? as u128)
}
/// Tokens in that take exactly `gross` SOL out of the reserves.
pub fn sell_in(x: u64, y: u64, gross: u64) -> Result<u64> {
    require!(gross < x, AmmError::InsufficientLiquidity);
    to_u64(((y as u128) * (gross as u128)).div_ceil((x - gross) as u128))
}

/// The first deposit into an empty pool mints the geometric mean of its two amounts as shares.
pub fn initial_shares(sol: u64, tokens: u64) -> Result<u64> {
    require!(sol > 0 && tokens > 0, AmmError::ZeroAmount);
    let shares = isqrt((sol as u128) * (tokens as u128));
    require!(shares > 0, AmmError::ZeroAmount);
    to_u64(shares)
}
/// A share's claim on one reserve: rounded up for what a deposit pays, down for what a withdrawal takes.
pub fn share_amount(shares: u64, reserve: u64, total: u64, round_up: bool) -> Result<u64> {
    if round_up { mul_div_ceil(shares, reserve, total) } else { mul_div(shares, reserve, total) }
}
fn isqrt(n: u128) -> u128 {
    if n < 2 { return n; }
    let mut x = 1u128 << ((128 - n.leading_zeros()).div_ceil(2));
    loop {
        let y = (x + n / x) / 2;
        if y >= x { return x; }
        x = y;
    }
}

/// Lamports per token atom, scaled by 2^64; zero for an empty pool.
pub fn spot(x: u64, y: u64) -> u128 {
    if y == 0 { 0 } else { ((x as u128) << 64) / y as u128 }
}
pub fn impact(before: u128, after: u128) -> i64 {
    if before == 0 { return 0; }
    // A display metric: the ratio is kept in U256 and only the signed value is clamped.
    let magnitude = U256::from(before.abs_diff(after)) * U256::from(BPS) / U256::from(before);
    if after >= before {
        if magnitude > U256::from(i64::MAX as u64) { i64::MAX } else { magnitude.low_u64() as i64 }
    } else if magnitude >= U256::from(1u64 << 63) { i64::MIN } else { -(magnitude.low_u64() as i64) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn isqrt_is_the_floor_root() {
        for n in [0u128, 1, 2, 3, 4, 15, 16, 17, 99, 100, 101, u64::MAX as u128, (u64::MAX as u128) * (u64::MAX as u128)] {
            let r = isqrt(n);
            assert!(r * r <= n && (r + 1).checked_mul(r + 1).map_or(true, |s| s > n), "{n}");
        }
    }
}
