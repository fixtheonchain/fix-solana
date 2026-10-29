//! Exact integer swap math. The SDK mirrors these routines, including rounding.
use anchor_lang::prelude::*;
use crate::{LaunchError, Terms};
mod wide { uint::construct_uint! { pub struct U256(4); } }
pub use wide::U256;

pub const BPS: u64 = 10_000;
pub const PRICE_SCALE: u128 = 1u128 << 64;

pub fn add(a: u64, b: u64) -> Result<u64> { a.checked_add(b).ok_or(error!(LaunchError::MathOverflow)) }
pub fn sub(a: u64, b: u64) -> Result<u64> { a.checked_sub(b).ok_or(error!(LaunchError::InsufficientLiquidity)) }
pub fn to_u64(v: u128) -> Result<u64> { u64::try_from(v).map_err(|_| error!(LaunchError::MathOverflow)) }
pub fn mul_div(a: u64, b: u64, d: u64) -> Result<u64> {
    require!(d > 0, LaunchError::MathOverflow);
    to_u64((a as u128) * (b as u128) / d as u128)
}
pub fn wide_div(a: U256, d: U256, ceil: bool) -> Result<u128> {
    require!(!d.is_zero(), LaunchError::MathOverflow);
    let q = a / d;
    let q = if ceil && a % d != U256::zero() { q.checked_add(U256::one()).ok_or(error!(LaunchError::MathOverflow))? } else { q };
    require!(q <= U256::from(u128::MAX), LaunchError::MathOverflow);
    Ok(q.low_u128())
}
pub fn ceil_div(n: u128, d: u128) -> Result<u128> {
    require!(d != 0, LaunchError::MathOverflow);
    Ok(n / d + u128::from(n % d != 0))
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, Debug, PartialEq, Eq, InitSpace)]
pub struct Fees { pub total: u64, pub protocol: u64, pub creator: u64, pub pool: u64 }

pub fn fees(gross: u64, rate: u16, terms: &Terms) -> Result<Fees> {
    let total = mul_div(gross, rate as u64, BPS)?;
    let protocol = mul_div(total, terms.protocol_share_bps as u64, BPS)?;
    let pool = mul_div(total, terms.pool_share_bps as u64, BPS)?;
    let creator = sub(sub(total, protocol)?, pool)?;
    Ok(Fees { total, protocol, creator, pool })
}

/// Minimal gross whose fee-net value covers `net`. Binary search avoids a fee-rounding off-by-one.
pub fn gross_for_net(net: u64, rate: u16) -> Result<u64> {
    require!(rate < BPS as u16, LaunchError::InvalidTerms);
    if net == 0 { return Ok(0); }
    let mut lo = net;
    require!(u64::MAX - mul_div(u64::MAX, rate as u64, BPS)? >= net, LaunchError::MathOverflow);
    let mut hi = ceil_div(net as u128 * BPS as u128, (BPS - rate as u64) as u128)?.min(u64::MAX as u128) as u64;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let fee = mul_div(mid, rate as u64, BPS)?;
        if mid - fee >= net { hi = mid; } else { lo = mid + 1; }
    }
    Ok(lo)
}

pub fn curve_buy_out(x: u64, y: u64, net: u64) -> Result<u64> {
    to_u64((y as u128) * (net as u128) / add(x, net)? as u128)
}
pub fn curve_buy_net(x: u64, y: u64, out: u64) -> Result<u64> {
    to_u64(ceil_div((x as u128) * out as u128, sub(y, out)? as u128)?)
}
pub fn curve_sell_out(x: u64, y: u64, input: u64) -> Result<u64> {
    to_u64((x as u128) * input as u128 / add(y, input)? as u128)
}
pub fn curve_sell_in(x: u64, y: u64, gross_out: u64) -> Result<u64> {
    to_u64(ceil_div((y as u128) * gross_out as u128, sub(x, gross_out)? as u128)?)
}
pub fn pool_buy_out(x: u64, y_num: u128, den: u64, net: u64) -> Result<u64> {
    to_u64(wide_div(U256::from(y_num) * U256::from(net), U256::from(add(x, net)?) * U256::from(den), false)?)
}
pub fn pool_buy_net(x: u64, y_num: u128, den: u64, out: u64) -> Result<u64> {
    let out_num = (out as u128) * den as u128;
    require!(y_num > out_num, LaunchError::InsufficientLiquidity);
    to_u64(wide_div(U256::from(x) * U256::from(out_num), U256::from(y_num - out_num), true)?)
}
pub fn pool_sell_out(x: u64, y_num: u128, den: u64, input: u64) -> Result<u64> {
    let input_num = (input as u128) * den as u128;
    let divisor = y_num.checked_add(input_num).ok_or(error!(LaunchError::MathOverflow))?;
    to_u64(wide_div(U256::from(x) * U256::from(input_num), U256::from(divisor), false)?)
}
pub fn pool_sell_in(x: u64, y_num: u128, den: u64, gross_out: u64) -> Result<u64> {
    to_u64(wide_div(U256::from(y_num) * U256::from(gross_out), U256::from(sub(x, gross_out)?) * U256::from(den), true)?)
}

pub fn spot(x: u64, y_num: u128, den: u64) -> Result<u128> {
    wide_div(U256::from(x) * U256::from(den) * U256::from(PRICE_SCALE), U256::from(y_num), false)
}
pub fn impact(before: u128, after: u128) -> Result<i64> {
    if before == 0 { return Ok(0); }
    // Receipt impact is a display metric. Keep its full intermediate ratio in
    // U256 and clamp only the signed display value, never a money amount.
    let magnitude = U256::from(before.abs_diff(after)) * U256::from(BPS) / U256::from(before);
    if after>=before {
        Ok(if magnitude>U256::from(i64::MAX as u64) {i64::MAX}else{magnitude.low_u64() as i64})
    }else{
        let minimum_magnitude=1u64<<63;
        Ok(if magnitude>=U256::from(minimum_magnitude) {i64::MIN}else{-(magnitude.low_u64() as i64)})
    }
}
