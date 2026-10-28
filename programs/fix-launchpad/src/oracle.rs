//! Exact minute-boundary integrals in one fixed, rent-funded ring.
//!
//! Reserve changes finish the previous constant-price segment before changing
//! the current price. Crossing a long quiet interval costs at most one ring's
//! writes. A completed call integral is immutable; absent evidence refunds.
use anchor_lang::prelude::*;
use crate::{math, Call, CallCheckpointed, LaunchError, Market, MinuteObservation,
    PriceHistory, PriceRecorded, PRICE_HISTORY_CAPACITY, PRICE_MINUTE_SECONDS};

pub fn minute(timestamp:i64)->i64 {
    timestamp.div_euclid(PRICE_MINUTE_SECONDS)*PRICE_MINUTE_SECONDS
}

pub fn slot(boundary:i64)->usize {
    boundary.div_euclid(PRICE_MINUTE_SECONDS).rem_euclid(PRICE_HISTORY_CAPACITY as i64) as usize
}

fn cumulative_at(market:&Market,edge:i64)->Result<math::U256> {
    require!(edge>=market.oracle_timestamp,LaunchError::InvalidOracleProof);
    market.oracle_cumulative().checked_add(math::U256::from(market.oracle_exit_price)
        *math::U256::from((edge as i128-market.oracle_timestamp as i128) as u128))
        .ok_or_else(||error!(LaunchError::MathOverflow))
}

pub fn initialize(history:&mut PriceHistory,market:&mut Market,key:Pubkey,bump:u8,now:i64)->Result<()> {
    require!(now>=0,LaunchError::ClockWentBackwards);
    history.market=key;history.latest_minute=minute(now);history.count=1;history.bump=bump;
    history.observations[slot(history.latest_minute)]=MinuteObservation::new(history.latest_minute,math::U256::zero());
    market.oracle_count=1;market.oracle_timestamp=now;market.oracle_exit_price=market.exit_price()?;
    market.oracle_cumulative_lo=0;market.oracle_cumulative_hi=0;
    emit!(PriceRecorded {market:key,index:0,timestamp:now,cumulative_lo:0,cumulative_hi:0,exit_price:market.oracle_exit_price});Ok(())
}

pub fn record(market:&mut Market,key:Pubkey,history:&mut PriceHistory,now:i64)->Result<()> {
    require!(history.market==key && history.count>0 && history.count<=PRICE_HISTORY_CAPACITY as u64,LaunchError::InvalidOracleProof);
    require!(now>=market.oracle_timestamp && history.latest_minute==minute(market.oracle_timestamp),LaunchError::ClockWentBackwards);
    let cumulative=cumulative_at(market,now)?;
    let latest=minute(now);
    let crossed=(latest-history.latest_minute)/PRICE_MINUTE_SECONDS;
    let writes=crossed.min(PRICE_HISTORY_CAPACITY as i64);
    if writes>0 {
        let mut edge=latest-(writes-1)*PRICE_MINUTE_SECONDS;
        let mut index=slot(edge);
        let mut boundary_cumulative=cumulative_at(market,edge)?;
        let minute_integral=math::U256::from(market.oracle_exit_price)*math::U256::from(PRICE_MINUTE_SECONDS as u64);
        for offset in 0..writes {
            history.observations[index]=MinuteObservation::new(edge,boundary_cumulative);
            if offset+1<writes {
                edge+=PRICE_MINUTE_SECONDS;
                index+=1;if index==PRICE_HISTORY_CAPACITY {index=0;}
                // Error construction must be lazy: SBF's bump allocator does
                // not reclaim eagerly-created Anchor errors on success.
                boundary_cumulative=boundary_cumulative.checked_add(minute_integral).ok_or_else(||error!(LaunchError::MathOverflow))?;
            }
        }
        history.latest_minute=latest;
        history.count=(history.count+writes as u64).min(PRICE_HISTORY_CAPACITY as u64);
        market.oracle_count=market.oracle_count.checked_add(crossed as u64).ok_or_else(||error!(LaunchError::MathOverflow))?;
    }
    market.oracle_timestamp=now;market.oracle_exit_price=market.exit_price()?;
    market.oracle_cumulative_lo=cumulative.low_u128();market.oracle_cumulative_hi=(cumulative>>128).low_u128();
    emit!(PriceRecorded {market:key,index:market.oracle_count-1,timestamp:now,cumulative_lo:market.oracle_cumulative_lo,cumulative_hi:market.oracle_cumulative_hi,exit_price:market.oracle_exit_price});Ok(())
}

/// Return None only when an exact historical edge was overwritten. A quiet
/// tail remains reconstructible indefinitely from the last reserve change.
pub fn at(market:&Market,history:&PriceHistory,edge:i64)->Result<Option<math::U256>> {
    require!(edge>=0 && edge%PRICE_MINUTE_SECONDS==0,LaunchError::InvalidOracleProof);
    require!(history.count>0 && history.count<=PRICE_HISTORY_CAPACITY as u64 && history.latest_minute==minute(market.oracle_timestamp),LaunchError::InvalidOracleProof);
    if edge>=market.oracle_timestamp {return Ok(Some(cumulative_at(market,edge)?));}
    let earliest=history.latest_minute-(history.count as i64-1)*PRICE_MINUTE_SECONDS;
    if edge<earliest || edge>history.latest_minute {return Ok(None);}
    let observation=&history.observations[slot(edge)];
    require!(observation.minute==edge,LaunchError::InvalidOracleProof);
    Ok(Some(observation.cumulative()))
}

pub fn checkpoint(call:&mut Call,key:Pubkey,market:&Market,history:&PriceHistory,now:i64)->Result<()> {
    if call.checkpointed {return Ok(());}
    require!(now>=call.horizon,LaunchError::CallNotMature);
    let start=at(market,history,call.window_start)?;
    let end=at(market,history,call.horizon)?;
    let integral=match (start,end) {
        (Some(start),Some(end))=>end.checked_sub(start).ok_or_else(||error!(LaunchError::InvalidOracleProof))?,
        _=>{call.evidence_missing=true;math::U256::zero()}
    };
    call.price_integral=integral.low_u128();call.price_integral_hi=(integral>>128).low_u128();
    call.checkpointed=true;
    emit!(CallCheckpointed {call:key,window_start:call.window_start,horizon:call.horizon,price_integral:call.price_integral,price_integral_hi:call.price_integral_hi,evidence_missing:call.evidence_missing});Ok(())
}

/// Align the actual deadline, and validate the duration that will be signed.
pub fn horizon(now:i64,requested:i64,min:i64,max:i64)->Result<i64> {
    require!(now>=0 && requested>=min && requested<=max,LaunchError::InvalidHorizon);
    let mut end=minute(now.checked_add(requested).ok_or_else(||error!(LaunchError::MathOverflow))?);
    if end-now<min {end=end.checked_add(PRICE_MINUTE_SECONDS).ok_or_else(||error!(LaunchError::MathOverflow))?;}
    require!(end-now>=min && end-now<=max,LaunchError::InvalidHorizon);Ok(end)
}
