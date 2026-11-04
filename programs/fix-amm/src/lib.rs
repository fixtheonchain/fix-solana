#![allow(unexpected_cfgs)]
use anchor_lang::prelude::*;
use anchor_lang::solana_program::{instruction::{AccountMeta, Instruction}, program::{invoke, invoke_signed}, pubkey, system_instruction};
use anchor_spl::token_interface::{self, TransferChecked};

pub mod math;
pub mod state;
pub mod contexts;
pub mod mint;
pub use state::*;
pub use contexts::*;
use math::{add, sub, Fees};

declare_id!("boi4jCS8KNarnN28BxRekzKd1hXt86rGUGpBhzkHfix");

/// The fix launchpad. A coin it created trades in its own market there for its whole life, so no pool here
/// takes its mint.
pub const LAUNCHPAD_ID: Pubkey = pubkey!("6S1hsbe4XecosNkMDcb6xTreHn2BNmnqr8Sfr1rJfix");

#[program]
pub mod fix_amm {
    use super::*;

    pub fn initialize_config(ctx: Context<InitializeConfig>, terms: Terms) -> Result<()> {
        terms.validate()?;
        ctx.accounts.config.set_inner(Config { revision: 0, admin: ctx.accounts.admin.key(), pending_admin: Pubkey::default(), paused: false, terms, bump: ctx.bumps.config });
        Ok(())
    }
    /// Anyone may open a fee shard; its rent stays with the PDA, which no key signs for.
    pub fn create_protocol_shard(ctx: Context<CreateProtocolShard>, shard: u8) -> Result<()> {
        require!(shard < PROTOCOL_SHARDS, AmmError::InvalidProtocolShard);
        ctx.accounts.protocol.set_inner(Protocol { accrued: 0, shard, bump: ctx.bumps.protocol });
        Ok(())
    }
    /// Admin only. The terms reach pools created from now on; `paused` stops new pools, nothing else.
    pub fn update_config(ctx: Context<AdminConfig>, terms: Terms, paused: bool) -> Result<()> {
        terms.validate()?;
        let config = &mut ctx.accounts.config;
        config.terms = terms;
        config.paused = paused;
        config.revision = add(config.revision, 1)?;
        Ok(())
    }
    pub fn propose_admin(ctx: Context<AdminConfig>, new_admin: Pubkey) -> Result<()> {
        require!(new_admin != Pubkey::default(), AmmError::Unauthorized);
        ctx.accounts.config.pending_admin = new_admin;
        Ok(())
    }
    pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.admin = config.pending_admin;
        config.pending_admin = Pubkey::default();
        Ok(())
    }

    /// Opens the mint's pool with its first liquidity, which sets the opening price. The creator's shares are
    /// the geometric mean of the two amounts. Swaps open in the next slot, the creator's included.
    pub fn create_pool(ctx: Context<CreatePool>, sol: u64, tokens: u64, expected_config_revision: u64) -> Result<()> {
        let config = &ctx.accounts.config;
        require!(!config.paused, AmmError::CreationPaused);
        require!(config.revision == expected_config_revision, AmmError::ConfigChanged);
        mint::check(&ctx.accounts.mint.to_account_info())?;
        require!(ctx.accounts.launchpad_market.owner != &LAUNCHPAD_ID, AmmError::LaunchpadCoin);
        let shares = math::initial_shares(sol, tokens)?;
        let clock = Clock::get()?;
        let events = EventSink::new(&ctx.accounts.event_authority, ctx.bumps.event_authority);
        let (key, creator, mint) = (ctx.accounts.pool.key(), ctx.accounts.creator.key(), ctx.accounts.mint.key());
        let pool = &mut ctx.accounts.pool;
        pool.set_inner(Pool {
            mint, token_program: ctx.accounts.token_program.key(), creator, terms: config.terms,
            sol_reserve: 0, token_reserve: 0, shares: 0, created_slot: clock.slot, seeded_slot: clock.slot,
            receipt_sequence: 0, bump: ctx.bumps.pool, vault_bump: ctx.bumps.vault,
        });
        ctx.accounts.position.set_inner(Position { pool: key, owner: creator, shares, bump: ctx.bumps.position });
        transfer_in(&ctx.accounts.creator.to_account_info(), &pool.to_account_info(), &ctx.accounts.system_program.to_account_info(), sol)?;
        token_in(&ctx.accounts.token_program, &ctx.accounts.creator_token, &ctx.accounts.mint, &ctx.accounts.vault, &ctx.accounts.creator.to_account_info(), tokens)?;
        pool.sol_reserve = sol;
        pool.token_reserve = tokens;
        pool.shares = shares;
        events.emit(&PoolCreated { pool: key, mint, token_program: pool.token_program, creator, terms: pool.terms, decimals: ctx.accounts.mint.decimals, created_slot: clock.slot })?;
        let price = pool.price();
        receipt(&events, pool, key, creator, CREATE, sol, Fees::default(), sol, tokens, shares, 0, price, clock.unix_timestamp)
    }

    /// Adds liquidity at the pool's ratio for exactly `shares`, paying each reserve's share rounded up, at
    /// most `max_sol` and `max_tokens`. Into an empty pool the two maximums go in whole and set the price,
    /// minting their geometric mean, at least `shares`; swaps then open in the next slot.
    pub fn add_liquidity(ctx: Context<AddLiquidity>, shares: u64, max_sol: u64, max_tokens: u64) -> Result<()> {
        let clock = Clock::get()?;
        let events = EventSink::new(&ctx.accounts.event_authority, ctx.bumps.event_authority);
        let (key, owner) = (ctx.accounts.pool.key(), ctx.accounts.owner.key());
        let pool = &mut ctx.accounts.pool;
        let before = pool.price();
        let (sol, tokens, minted) = if pool.shares == 0 {
            let minted = math::initial_shares(max_sol, max_tokens)?;
            require!(minted >= shares, AmmError::SlippageExceeded);
            pool.seeded_slot = clock.slot;
            (max_sol, max_tokens, minted)
        } else {
            require!(shares > 0, AmmError::ZeroAmount);
            let sol = math::share_amount(shares, pool.sol_reserve, pool.shares, true)?;
            let tokens = math::share_amount(shares, pool.token_reserve, pool.shares, true)?;
            require!(sol <= max_sol && tokens <= max_tokens, AmmError::SlippageExceeded);
            (sol, tokens, shares)
        };
        let position = &mut ctx.accounts.position;
        if position.owner == Pubkey::default() {
            position.set_inner(Position { pool: key, owner, shares: 0, bump: ctx.bumps.position });
        }
        transfer_in(&ctx.accounts.owner.to_account_info(), &pool.to_account_info(), &ctx.accounts.system_program.to_account_info(), sol)?;
        token_in(&ctx.accounts.token_program, &ctx.accounts.owner_token, &ctx.accounts.mint, &ctx.accounts.vault, &ctx.accounts.owner.to_account_info(), tokens)?;
        pool.sol_reserve = add(pool.sol_reserve, sol)?;
        pool.token_reserve = add(pool.token_reserve, tokens)?;
        pool.shares = add(pool.shares, minted)?;
        position.shares = add(position.shares, minted)?;
        let after = pool.price();
        receipt(&events, pool, key, owner, ADD, sol, Fees::default(), sol, tokens, minted, before, after, clock.unix_timestamp)
    }

    /// Burns `shares` for each reserve's share rounded down, at least `min_sol` and `min_tokens`. The last
    /// shares take everything left in the pool.
    pub fn remove_liquidity(ctx: Context<RemoveLiquidity>, shares: u64, min_sol: u64, min_tokens: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let events = EventSink::new(&ctx.accounts.event_authority, ctx.bumps.event_authority);
        let (key, owner) = (ctx.accounts.pool.key(), ctx.accounts.owner.key());
        let position = &mut ctx.accounts.position;
        require!(shares > 0, AmmError::ZeroAmount);
        require!(shares <= position.shares, AmmError::InsufficientShares);
        let pool = &mut ctx.accounts.pool;
        let before = pool.price();
        let sol = math::share_amount(shares, pool.sol_reserve, pool.shares, false)?;
        let tokens = math::share_amount(shares, pool.token_reserve, pool.shares, false)?;
        require!(sol > 0 || tokens > 0, AmmError::ZeroAmount);
        require!(sol >= min_sol && tokens >= min_tokens, AmmError::SlippageExceeded);
        pool.sol_reserve = sub(pool.sol_reserve, sol)?;
        pool.token_reserve = sub(pool.token_reserve, tokens)?;
        pool.shares = sub(pool.shares, shares)?;
        position.shares = sub(position.shares, shares)?;
        token_out(&ctx.accounts.token_program, pool, &ctx.accounts.vault, &ctx.accounts.mint, &ctx.accounts.owner_token, tokens)?;
        move_lamports(&pool.to_account_info(), &ctx.accounts.owner.to_account_info(), sol)?;
        let after = pool.price();
        receipt(&events, pool, key, owner, REMOVE, sol, Fees::default(), sol, tokens, shares, before, after, now)
    }

    pub fn buy_exact_in(ctx: Context<Swap>, gross: u64, min_tokens: u64) -> Result<()> { execute_buy(ctx, gross, None, min_tokens) }
    pub fn buy_exact_out(ctx: Context<Swap>, tokens: u64, max_gross: u64) -> Result<()> { execute_buy(ctx, max_gross, Some(tokens), tokens) }
    pub fn sell_exact_in(ctx: Context<Swap>, tokens: u64, min_sol: u64) -> Result<()> { execute_sell(ctx, tokens, None, min_sol) }
    pub fn sell_exact_out(ctx: Context<Swap>, sol: u64, max_tokens: u64) -> Result<()> { execute_sell(ctx, max_tokens, Some(sol), sol) }
}

/// A buy pays `gross` SOL; the fee comes off it, the protocol's part goes to the shard and the liquidity
/// providers' part stays in the reserves with the SOL that bought the tokens.
fn execute_buy(ctx: Context<Swap>, budget: u64, exact_out: Option<u64>, min_tokens: u64) -> Result<()> {
    let clock = Clock::get()?;
    let events = EventSink::new(&ctx.accounts.event_authority, ctx.bumps.event_authority);
    let (key, trader) = (ctx.accounts.pool.key(), ctx.accounts.trader.key());
    let pool = &mut ctx.accounts.pool;
    require!(pool.shares > 0, AmmError::InsufficientLiquidity);
    require!(clock.slot > pool.seeded_slot, AmmError::SwapNotOpen);
    require!(budget > 0, AmmError::ZeroAmount);
    let before = pool.price();
    let (gross, out) = match exact_out {
        Some(out) => {
            require!(out > 0, AmmError::ZeroAmount);
            let gross = math::gross_for_net(math::buy_net(pool.sol_reserve, pool.token_reserve, out)?, pool.terms.fee_bps)?;
            require!(gross <= budget, AmmError::SlippageExceeded);
            (gross, out)
        }
        None => (budget, math::buy_out(pool.sol_reserve, pool.token_reserve, budget - math::fees(budget, &pool.terms)?.total)?),
    };
    require!(out > 0 && out >= min_tokens, AmmError::SlippageExceeded);
    let fees = math::fees(gross, &pool.terms)?;
    transfer_in(&ctx.accounts.trader.to_account_info(), &pool.to_account_info(), &ctx.accounts.system_program.to_account_info(), gross)?;
    pool.sol_reserve = add(pool.sol_reserve, gross - fees.protocol)?;
    pool.token_reserve = sub(pool.token_reserve, out)?;
    token_out(&ctx.accounts.token_program, pool, &ctx.accounts.vault, &ctx.accounts.mint, &ctx.accounts.trader_token, out)?;
    credit_protocol(pool, &mut ctx.accounts.protocol, fees.protocol)?;
    let after = pool.price();
    receipt(&events, pool, key, trader, BUY, gross, fees, gross - fees.total, out, 0, before, after, clock.unix_timestamp)
}
/// A sell's tokens take `gross` SOL out of the pool's price; the fee comes off it and the trader receives
/// `net`. The liquidity providers' part never leaves the reserves.
fn execute_sell(ctx: Context<Swap>, budget: u64, exact_out: Option<u64>, min_sol: u64) -> Result<()> {
    let clock = Clock::get()?;
    let events = EventSink::new(&ctx.accounts.event_authority, ctx.bumps.event_authority);
    let (key, trader) = (ctx.accounts.pool.key(), ctx.accounts.trader.key());
    let pool = &mut ctx.accounts.pool;
    require!(pool.shares > 0, AmmError::InsufficientLiquidity);
    require!(clock.slot > pool.seeded_slot, AmmError::SwapNotOpen);
    require!(budget > 0, AmmError::ZeroAmount);
    let before = pool.price();
    let (input, gross, net) = match exact_out {
        Some(wanted) => {
            require!(wanted > 0, AmmError::ZeroAmount);
            let gross = math::gross_for_net(wanted, pool.terms.fee_bps)?;
            let input = math::sell_in(pool.sol_reserve, pool.token_reserve, gross)?;
            require!(input <= budget, AmmError::SlippageExceeded);
            (input, gross, wanted)
        }
        None => {
            let gross = math::sell_out(pool.sol_reserve, pool.token_reserve, budget)?;
            (budget, gross, gross - math::fees(gross, &pool.terms)?.total)
        }
    };
    require!(net > 0 && net >= min_sol, AmmError::SlippageExceeded);
    let fees = math::fees(gross, &pool.terms)?;
    token_in(&ctx.accounts.token_program, &ctx.accounts.trader_token, &ctx.accounts.mint, &ctx.accounts.vault, &ctx.accounts.trader.to_account_info(), input)?;
    pool.token_reserve = add(pool.token_reserve, input)?;
    pool.sol_reserve = sub(pool.sol_reserve, add(net, fees.protocol)?)?;
    move_lamports(&pool.to_account_info(), &ctx.accounts.trader.to_account_info(), net)?;
    credit_protocol(pool, &mut ctx.accounts.protocol, fees.protocol)?;
    let after = pool.price();
    receipt(&events, pool, key, trader, SELL, gross, fees, net, input, 0, before, after, clock.unix_timestamp)
}

/// Events travel as self-CPI instruction data (Anchor `#[event_cpi]`), which the runtime records in full,
/// so the 10,000-byte log budget can never drop a receipt.
struct EventSink<'info> { authority: AccountInfo<'info>, bump: u8 }
impl<'info> EventSink<'info> {
    fn new(authority: &AccountInfo<'info>, bump: u8) -> Self { Self { authority: authority.clone(), bump } }
    fn emit<E: anchor_lang::Event>(&self, event: &E) -> Result<()> {
        let mut data = anchor_lang::event::EVENT_IX_TAG_LE.to_vec();
        data.extend(event.data());
        let instruction = Instruction::new_with_bytes(crate::ID, &data, vec![AccountMeta::new_readonly(self.authority.key(), true)]);
        invoke_signed(&instruction, &[self.authority.clone()], &[&[b"__event_authority", &[self.bump]]]).map_err(Into::into)
    }
}

fn transfer_in<'info>(from: &AccountInfo<'info>, to: &AccountInfo<'info>, system: &AccountInfo<'info>, amount: u64) -> Result<()> {
    if amount > 0 { invoke(&system_instruction::transfer(from.key, to.key, amount), &[from.clone(), to.clone(), system.clone()])?; }
    Ok(())
}
fn token_in<'info>(program: &Interface<'info, token_interface::TokenInterface>, from: &InterfaceAccount<'info, token_interface::TokenAccount>, mint: &InterfaceAccount<'info, token_interface::Mint>, vault: &InterfaceAccount<'info, token_interface::TokenAccount>, authority: &AccountInfo<'info>, amount: u64) -> Result<()> {
    if amount == 0 { return Ok(()); }
    token_interface::transfer_checked(CpiContext::new(program.to_account_info(), TransferChecked { from: from.to_account_info(), mint: mint.to_account_info(), to: vault.to_account_info(), authority: authority.clone() }), amount, mint.decimals)
}
fn token_out<'info>(program: &Interface<'info, token_interface::TokenInterface>, pool: &Account<'info, Pool>, vault: &InterfaceAccount<'info, token_interface::TokenAccount>, mint: &InterfaceAccount<'info, token_interface::Mint>, to: &InterfaceAccount<'info, token_interface::TokenAccount>, amount: u64) -> Result<()> {
    if amount == 0 { return Ok(()); }
    let bump = [pool.bump];
    let seeds: &[&[u8]] = &[b"pool", pool.mint.as_ref(), &bump];
    token_interface::transfer_checked(CpiContext::new_with_signer(program.to_account_info(), TransferChecked { from: vault.to_account_info(), mint: mint.to_account_info(), to: to.to_account_info(), authority: pool.to_account_info() }, &[seeds]), amount, mint.decimals)
}
/// Moves lamports out of a program-owned account, never below its rent floor.
fn move_lamports(from: &AccountInfo, to: &AccountInfo, amount: u64) -> Result<()> {
    if amount == 0 { return Ok(()); }
    let floor = Rent::get()?.minimum_balance(from.data_len());
    let remaining = from.lamports().checked_sub(amount).ok_or(error!(AmmError::InsufficientLiquidity))?;
    require!(remaining >= floor, AmmError::InsufficientLiquidity);
    let destination = to.lamports().checked_add(amount).ok_or(error!(AmmError::MathOverflow))?;
    **from.try_borrow_mut_lamports()? = remaining;
    **to.try_borrow_mut_lamports()? = destination;
    Ok(())
}
/// The protocol's part, credited in the instruction that charges it. No instruction withdraws from a shard.
fn credit_protocol(pool: &Account<Pool>, protocol: &mut Account<Protocol>, amount: u64) -> Result<()> {
    move_lamports(&pool.to_account_info(), &protocol.to_account_info(), amount)?;
    protocol.accrued = add(protocol.accrued, amount)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn receipt(events: &EventSink, pool: &mut Pool, key: Pubkey, actor: Pubkey, action: u8, gross: u64, fees: Fees, net: u64, tokens: u64, shares: u64, before: u128, after: u128, now: i64) -> Result<()> {
    pool.receipt_sequence = add(pool.receipt_sequence, 1)?;
    events.emit(&Receipt {
        pool: key, actor, action, sequence: pool.receipt_sequence, timestamp: now,
        gross, protocol_fee: fees.protocol, lp_fee: fees.lp, net, tokens, shares,
        total_shares: pool.shares, sol_reserve: pool.sol_reserve, token_reserve: pool.token_reserve,
        price_before: before, price_after: after, price_impact_bps: math::impact(before, after),
    })
}

#[error_code]
pub enum AmmError {
    #[msg("Invalid fee terms")] InvalidTerms,
    #[msg("Arithmetic exceeds supported integer bounds")] MathOverflow,
    #[msg("Pool creation is paused")] CreationPaused,
    #[msg("Configuration changed since the creation preview")] ConfigChanged,
    #[msg("Amount must be positive")] ZeroAmount,
    #[msg("Slippage bound would be exceeded")] SlippageExceeded,
    #[msg("Insufficient pool liquidity")] InsufficientLiquidity,
    #[msg("Authority is not authorized")] Unauthorized,
    #[msg("Swaps open in the slot after liquidity goes into an empty pool")] SwapNotOpen,
    #[msg("Protocol fee shard is out of range")] InvalidProtocolShard,
    #[msg("This mint's transfers or vault could change outside the pool")] UnsupportedMint,
    #[msg("A coin created on the fix launchpad trades in its own market")] LaunchpadCoin,
    #[msg("The position holds fewer shares")] InsufficientShares,
}
