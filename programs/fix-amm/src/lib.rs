#![allow(unexpected_cfgs)]
use anchor_lang::prelude::*;
use anchor_lang::solana_program::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, instruction::{AccountMeta, Instruction}, program::{invoke, invoke_signed}, pubkey, system_instruction};
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

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
        let a = &mut *ctx.accounts; let bumps = (ctx.bumps.pool, ctx.bumps.vault, ctx.bumps.position);
        let events = EventSink::new(&a.event_authority, ctx.bumps.event_authority);
        open_pool(&a.config, &a.creator, &a.mint, &mut a.pool, &a.vault, &a.creator_token, &mut a.position, &a.launchpad_market, &a.token_program, &a.system_program, &events, bumps, sol, tokens, expected_config_revision)
    }
    /// Opens a pool with a hook, at `["pool", mint, hook]` beside the mint's plain pool. The hook must be a program
    /// that can no longer change, and its account for this pool must exist. The AMM calls it on every swap and add.
    pub fn create_hooked_pool(ctx: Context<CreateHookedPool>, sol: u64, tokens: u64, expected_config_revision: u64) -> Result<()> {
        check_hook(&ctx.accounts.hook, &ctx.accounts.hook_program_data, &ctx.accounts.hook_state, &ctx.accounts.pool.key())?;
        write_hook(&ctx.accounts.pool.to_account_info(), POOL_BASE_LEN, &ctx.accounts.hook.key())?;
        let a = &mut *ctx.accounts; let bumps = (ctx.bumps.pool, ctx.bumps.vault, ctx.bumps.position);
        let events = EventSink::new(&a.event_authority, ctx.bumps.event_authority);
        open_pool(&a.config, &a.creator, &a.mint, &mut a.pool, &a.vault, &a.creator_token, &mut a.position, &a.launchpad_market, &a.token_program, &a.system_program, &events, bumps, sol, tokens, expected_config_revision)
    }

    /// Adds liquidity at the pool's ratio for exactly `shares`, paying each reserve's share rounded up, at
    /// most `max_sol` and `max_tokens`. Into an empty pool the two maximums go in whole and set the price,
    /// minting their geometric mean, at least `shares`; swaps then open in the next slot.
    pub fn add_liquidity<'info>(ctx: Context<'_, '_, 'info, 'info, AddLiquidity<'info>>, shares: u64, max_sol: u64, max_tokens: u64, min_sol: u64, min_tokens: u64) -> Result<()> {
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
            // Both sides bounded both ways: a pool emptied and seeded again since the quote cannot move the price.
            require!(sol <= max_sol && tokens <= max_tokens && sol >= min_sol && tokens >= min_tokens, AmmError::SlippageExceeded);
            (sol, tokens, shares)
        };
        call_hook(ctx.remaining_accounts, &pool.to_account_info(), POOL_BASE_LEN, &ctx.accounts.owner.to_account_info(), HookCall {
            venue: key, actor: owner, action: ADD, slot: clock.slot, amount_a: sol, amount_b: tokens,
            reserve_a_before: pool.sol_reserve, reserve_b_before: pool.token_reserve, reserve_a_after: add(pool.sol_reserve, sol)?, reserve_b_after: add(pool.token_reserve, tokens)?,
        })?;
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

    pub fn buy_exact_in<'info>(ctx: Context<'_, '_, 'info, 'info, Swap<'info>>, gross: u64, min_tokens: u64) -> Result<()> { execute_buy(ctx, gross, None, min_tokens) }
    pub fn buy_exact_out<'info>(ctx: Context<'_, '_, 'info, 'info, Swap<'info>>, tokens: u64, max_gross: u64) -> Result<()> { execute_buy(ctx, max_gross, Some(tokens), tokens) }
    pub fn sell_exact_in<'info>(ctx: Context<'_, '_, 'info, 'info, Swap<'info>>, tokens: u64, min_sol: u64) -> Result<()> { execute_sell(ctx, tokens, None, min_sol) }
    pub fn sell_exact_out<'info>(ctx: Context<'_, '_, 'info, 'info, Swap<'info>>, sol: u64, max_tokens: u64) -> Result<()> { execute_sell(ctx, max_tokens, Some(sol), sol) }

    /// Opens the pair of two token mints, `mint_a` the smaller address, with its first liquidity, which sets its
    /// price. The creator's shares are the geometric mean of the two amounts. Swaps open in the next slot.
    pub fn create_pair(ctx: Context<CreatePair>, amount_a: u64, amount_b: u64, expected_config_revision: u64) -> Result<()> {
        let a = &mut *ctx.accounts; let bumps = (ctx.bumps.pair, ctx.bumps.vault_a, ctx.bumps.vault_b, ctx.bumps.position);
        let events = EventSink::new(&a.event_authority, ctx.bumps.event_authority);
        open_pair(&a.config, &a.creator, (&a.mint_a, &a.mint_b), &mut a.pair, (&a.vault_a, &a.vault_b), (&a.creator_token_a, &a.creator_token_b), &mut a.position, (&a.launchpad_market_a, &a.launchpad_market_b), (&a.token_program_a, &a.token_program_b), &events, bumps, amount_a, amount_b, expected_config_revision)
    }
    /// Opens a pair with a hook, at `["pair", mint_a, mint_b, hook]` beside the mints' plain pair, under the same
    /// rules as a hooked pool.
    pub fn create_hooked_pair(ctx: Context<CreateHookedPair>, amount_a: u64, amount_b: u64, expected_config_revision: u64) -> Result<()> {
        check_hook(&ctx.accounts.hook, &ctx.accounts.hook_program_data, &ctx.accounts.hook_state, &ctx.accounts.pair.key())?;
        write_hook(&ctx.accounts.pair.to_account_info(), PAIR_BASE_LEN, &ctx.accounts.hook.key())?;
        let a = &mut *ctx.accounts; let bumps = (ctx.bumps.pair, ctx.bumps.vault_a, ctx.bumps.vault_b, ctx.bumps.position);
        let events = EventSink::new(&a.event_authority, ctx.bumps.event_authority);
        open_pair(&a.config, &a.creator, (&a.mint_a, &a.mint_b), &mut a.pair, (&a.vault_a, &a.vault_b), (&a.creator_token_a, &a.creator_token_b), &mut a.position, (&a.launchpad_market_a, &a.launchpad_market_b), (&a.token_program_a, &a.token_program_b), &events, bumps, amount_a, amount_b, expected_config_revision)
    }

    /// Exactly `shares` at the pair's ratio, each side's part rounded up, at most `max_a` and `max_b`. Into an empty
    /// pair both maximums go in whole and set the price, minting their geometric mean, at least `shares`.
    pub fn add_pair_liquidity<'info>(ctx: Context<'_, '_, 'info, 'info, AddPairLiquidity<'info>>, shares: u64, max_a: u64, max_b: u64, min_a: u64, min_b: u64) -> Result<()> {
        let clock = Clock::get()?;
        let events = EventSink::new(&ctx.accounts.event_authority, ctx.bumps.event_authority);
        let (key, owner) = (ctx.accounts.pair.key(), ctx.accounts.owner.key());
        let pair = &mut ctx.accounts.pair;
        let before = pair.price();
        let (amount_a, amount_b, minted) = if pair.shares == 0 {
            let minted = math::initial_shares(max_a, max_b)?;
            require!(minted >= shares, AmmError::SlippageExceeded);
            pair.seeded_slot = clock.slot;
            (max_a, max_b, minted)
        } else {
            require!(shares > 0, AmmError::ZeroAmount);
            let a = math::share_amount(shares, pair.reserve_a, pair.shares, true)?;
            let b = math::share_amount(shares, pair.reserve_b, pair.shares, true)?;
            require!(a <= max_a && b <= max_b && a >= min_a && b >= min_b, AmmError::SlippageExceeded);
            (a, b, shares)
        };
        call_hook(ctx.remaining_accounts, &pair.to_account_info(), PAIR_BASE_LEN, &ctx.accounts.owner.to_account_info(), HookCall {
            venue: key, actor: owner, action: ADD, slot: clock.slot, amount_a, amount_b,
            reserve_a_before: pair.reserve_a, reserve_b_before: pair.reserve_b, reserve_a_after: add(pair.reserve_a, amount_a)?, reserve_b_after: add(pair.reserve_b, amount_b)?,
        })?;
        let position = &mut ctx.accounts.position;
        if position.owner == Pubkey::default() {
            position.set_inner(Position { pool: key, owner, shares: 0, bump: ctx.bumps.position });
        }
        token_in(&ctx.accounts.token_program_a, &ctx.accounts.owner_token_a, &ctx.accounts.mint_a, &ctx.accounts.vault_a, &ctx.accounts.owner.to_account_info(), amount_a)?;
        token_in(&ctx.accounts.token_program_b, &ctx.accounts.owner_token_b, &ctx.accounts.mint_b, &ctx.accounts.vault_b, &ctx.accounts.owner.to_account_info(), amount_b)?;
        pair.reserve_a = add(pair.reserve_a, amount_a)?;
        pair.reserve_b = add(pair.reserve_b, amount_b)?;
        pair.shares = add(pair.shares, minted)?;
        position.shares = add(position.shares, minted)?;
        let after = pair.price();
        pair_receipt(&events, pair, key, owner, ADD, amount_a, amount_b, Fees::default(), minted, before, after, clock.unix_timestamp)
    }

    /// Burns `shares` for each side's part rounded down, at least `min_a` and `min_b`. The last shares take everything.
    pub fn remove_pair_liquidity(ctx: Context<RemovePairLiquidity>, shares: u64, min_a: u64, min_b: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let events = EventSink::new(&ctx.accounts.event_authority, ctx.bumps.event_authority);
        let (key, owner) = (ctx.accounts.pair.key(), ctx.accounts.owner.key());
        let position = &mut ctx.accounts.position;
        require!(shares > 0, AmmError::ZeroAmount);
        require!(shares <= position.shares, AmmError::InsufficientShares);
        let pair = &mut ctx.accounts.pair;
        let before = pair.price();
        let amount_a = math::share_amount(shares, pair.reserve_a, pair.shares, false)?;
        let amount_b = math::share_amount(shares, pair.reserve_b, pair.shares, false)?;
        require!(amount_a > 0 || amount_b > 0, AmmError::ZeroAmount);
        require!(amount_a >= min_a && amount_b >= min_b, AmmError::SlippageExceeded);
        pair.reserve_a = sub(pair.reserve_a, amount_a)?;
        pair.reserve_b = sub(pair.reserve_b, amount_b)?;
        pair.shares = sub(pair.shares, shares)?;
        position.shares = sub(position.shares, shares)?;
        pair_out(&ctx.accounts.token_program_a, pair, &ctx.accounts.vault_a, &ctx.accounts.mint_a, &ctx.accounts.owner_token_a, amount_a)?;
        pair_out(&ctx.accounts.token_program_b, pair, &ctx.accounts.vault_b, &ctx.accounts.mint_b, &ctx.accounts.owner_token_b, amount_b)?;
        let after = pair.price();
        pair_receipt(&events, pair, key, owner, REMOVE, amount_a, amount_b, Fees::default(), shares, before, after, now)
    }

    /// Pays exactly `amount_in` of the input token (A when `a_to_b`); the fee comes off it in that token.
    pub fn swap_pair_exact_in<'info>(ctx: Context<'_, '_, 'info, 'info, PairSwap<'info>>, a_to_b: bool, amount_in: u64, min_out: u64) -> Result<()> { execute_pair_swap(ctx, a_to_b, amount_in, None, min_out) }
    /// Receives exactly `amount_out` of the output token; the input paid is the smallest that covers it after the fee.
    pub fn swap_pair_exact_out<'info>(ctx: Context<'_, '_, 'info, 'info, PairSwap<'info>>, a_to_b: bool, amount_out: u64, max_in: u64) -> Result<()> { execute_pair_swap(ctx, a_to_b, max_in, Some(amount_out), amount_out) }
}

/// A pair swap in either direction. The input reserve gains what the trader paid less the protocol's part, which
/// is booked to the protocol in the input token; the liquidity providers' part stays in the reserve.
fn execute_pair_swap<'info>(ctx: Context<'_, '_, 'info, 'info, PairSwap<'info>>, a_to_b: bool, budget: u64, exact_out: Option<u64>, min_out: u64) -> Result<()> {
    let clock = Clock::get()?;
    let events = EventSink::new(&ctx.accounts.event_authority, ctx.bumps.event_authority);
    let (key, trader) = (ctx.accounts.pair.key(), ctx.accounts.trader.key());
    let pair = &mut ctx.accounts.pair;
    require!(pair.shares > 0, AmmError::InsufficientLiquidity);
    require!(clock.slot > pair.seeded_slot, AmmError::SwapNotOpen);
    require!(budget > 0, AmmError::ZeroAmount);
    let before = pair.price();
    let (x, y) = if a_to_b { (pair.reserve_a, pair.reserve_b) } else { (pair.reserve_b, pair.reserve_a) };
    let (gross, out) = match exact_out {
        Some(out) => {
            require!(out > 0, AmmError::ZeroAmount);
            let gross = math::gross_for_net(math::buy_net(x, y, out)?, pair.terms.fee_bps)?;
            require!(gross <= budget, AmmError::SlippageExceeded);
            (gross, out)
        }
        None => (budget, math::buy_out(x, y, budget - math::fees(budget, &pair.terms)?.total)?),
    };
    require!(out > 0 && out >= min_out, AmmError::SlippageExceeded);
    let fees = math::fees(gross, &pair.terms)?;
    let (reserve_in, reserve_out) = (add(x, gross - fees.protocol)?, sub(y, out)?);
    let ((amount_a, amount_b), (after_a, after_b)) = if a_to_b { ((gross, out), (reserve_in, reserve_out)) } else { ((out, gross), (reserve_out, reserve_in)) };
    call_hook(ctx.remaining_accounts, &pair.to_account_info(), PAIR_BASE_LEN, &ctx.accounts.trader.to_account_info(), HookCall {
        venue: key, actor: trader, action: if a_to_b { SWAP_A_TO_B } else { SWAP_B_TO_A }, slot: clock.slot, amount_a, amount_b,
        reserve_a_before: pair.reserve_a, reserve_b_before: pair.reserve_b, reserve_a_after: after_a, reserve_b_after: after_b,
    })?;
    if a_to_b {
        pair.reserve_a = reserve_in; pair.reserve_b = reserve_out; pair.protocol_a = add(pair.protocol_a, fees.protocol)?;
        token_in(&ctx.accounts.token_program_a, &ctx.accounts.trader_token_a, &ctx.accounts.mint_a, &ctx.accounts.vault_a, &ctx.accounts.trader.to_account_info(), gross)?;
        pair_out(&ctx.accounts.token_program_b, pair, &ctx.accounts.vault_b, &ctx.accounts.mint_b, &ctx.accounts.trader_token_b, out)?;
    } else {
        pair.reserve_b = reserve_in; pair.reserve_a = reserve_out; pair.protocol_b = add(pair.protocol_b, fees.protocol)?;
        token_in(&ctx.accounts.token_program_b, &ctx.accounts.trader_token_b, &ctx.accounts.mint_b, &ctx.accounts.vault_b, &ctx.accounts.trader.to_account_info(), gross)?;
        pair_out(&ctx.accounts.token_program_a, pair, &ctx.accounts.vault_a, &ctx.accounts.mint_a, &ctx.accounts.trader_token_a, out)?;
    }
    let after = pair.price();
    pair_receipt(&events, pair, key, trader, if a_to_b { SWAP_A_TO_B } else { SWAP_B_TO_A }, amount_a, amount_b, fees, 0, before, after, clock.unix_timestamp)
}
fn pair_out<'info>(program: &Interface<'info, token_interface::TokenInterface>, pair: &Account<'info, Pair>, vault: &InterfaceAccount<'info, token_interface::TokenAccount>, mint: &InterfaceAccount<'info, token_interface::Mint>, to: &InterfaceAccount<'info, token_interface::TokenAccount>, amount: u64) -> Result<()> {
    if amount == 0 { return Ok(()); }
    let bump = [pair.bump]; let hook = hook_of(&pair.to_account_info(), PAIR_BASE_LEN)?.unwrap_or_default();
    let plain: [&[u8]; 4] = [b"pair", pair.mint_a.as_ref(), pair.mint_b.as_ref(), &bump];
    let hooked: [&[u8]; 5] = [b"pair", pair.mint_a.as_ref(), pair.mint_b.as_ref(), hook.as_ref(), &bump];
    let seeds: &[&[u8]] = if hook == Pubkey::default() { &plain } else { &hooked };
    token_interface::transfer_checked(CpiContext::new_with_signer(program.to_account_info(), TransferChecked { from: vault.to_account_info(), mint: mint.to_account_info(), to: to.to_account_info(), authority: pair.to_account_info() }, &[seeds]), amount, mint.decimals)
}
#[allow(clippy::too_many_arguments)]
fn pair_receipt(events: &EventSink, pair: &mut Pair, key: Pubkey, actor: Pubkey, action: u8, amount_a: u64, amount_b: u64, fees: Fees, shares: u64, before: u128, after: u128, now: i64) -> Result<()> {
    pair.receipt_sequence = add(pair.receipt_sequence, 1)?;
    events.emit(&PairReceipt {
        pair: key, actor, action, sequence: pair.receipt_sequence, timestamp: now, amount_a, amount_b,
        protocol_fee: fees.protocol, lp_fee: fees.lp, shares, total_shares: pair.shares, reserve_a: pair.reserve_a, reserve_b: pair.reserve_b,
        price_before: before, price_after: after, price_impact_bps: math::impact(before, after),
    })
}

/// A buy pays `gross` SOL; the fee comes off it, the protocol's part goes to the shard and the liquidity
/// providers' part stays in the reserves with the SOL that bought the tokens.
fn execute_buy<'info>(ctx: Context<'_, '_, 'info, 'info, Swap<'info>>, budget: u64, exact_out: Option<u64>, min_tokens: u64) -> Result<()> {
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
    call_hook(ctx.remaining_accounts, &pool.to_account_info(), POOL_BASE_LEN, &ctx.accounts.trader.to_account_info(), HookCall {
        venue: key, actor: trader, action: BUY, slot: clock.slot, amount_a: gross, amount_b: out,
        reserve_a_before: pool.sol_reserve, reserve_b_before: pool.token_reserve, reserve_a_after: add(pool.sol_reserve, gross - fees.protocol)?, reserve_b_after: sub(pool.token_reserve, out)?,
    })?;
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
fn execute_sell<'info>(ctx: Context<'_, '_, 'info, 'info, Swap<'info>>, budget: u64, exact_out: Option<u64>, min_sol: u64) -> Result<()> {
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
    call_hook(ctx.remaining_accounts, &pool.to_account_info(), POOL_BASE_LEN, &ctx.accounts.trader.to_account_info(), HookCall {
        venue: key, actor: trader, action: SELL, slot: clock.slot, amount_a: net, amount_b: input,
        reserve_a_before: pool.sol_reserve, reserve_b_before: pool.token_reserve, reserve_a_after: sub(pool.sol_reserve, add(net, fees.protocol)?)?, reserve_b_after: add(pool.token_reserve, input)?,
    })?;
    token_in(&ctx.accounts.token_program, &ctx.accounts.trader_token, &ctx.accounts.mint, &ctx.accounts.vault, &ctx.accounts.trader.to_account_info(), input)?;
    pool.token_reserve = add(pool.token_reserve, input)?;
    pool.sol_reserve = sub(pool.sol_reserve, add(net, fees.protocol)?)?;
    move_lamports(&pool.to_account_info(), &ctx.accounts.trader.to_account_info(), net)?;
    credit_protocol(pool, &mut ctx.accounts.protocol, fees.protocol)?;
    let after = pool.price();
    receipt(&events, pool, key, trader, SELL, gross, fees, net, input, 0, before, after, clock.unix_timestamp)
}

/// Opens a pool with its first liquidity, which sets the opening price. The creator's shares are the geometric mean
/// of the two amounts. Swaps open in the next slot, the creator's included.
#[allow(clippy::too_many_arguments)]
fn open_pool<'info>(config: &Account<'info, Config>, creator: &Signer<'info>, mint: &InterfaceAccount<'info, Mint>, pool: &mut Account<'info, Pool>, vault: &InterfaceAccount<'info, TokenAccount>, creator_token: &InterfaceAccount<'info, TokenAccount>, position: &mut Account<'info, Position>, launchpad_market: &UncheckedAccount<'info>, token_program: &Interface<'info, TokenInterface>, system_program: &Program<'info, System>, events: &EventSink<'info>, bumps: (u8, u8, u8), sol: u64, tokens: u64, expected_config_revision: u64) -> Result<()> {
    require!(!config.paused, AmmError::CreationPaused);
    require!(config.revision == expected_config_revision, AmmError::ConfigChanged);
    mint::check(&mint.to_account_info())?;
    require!(launchpad_market.owner != &LAUNCHPAD_ID, AmmError::LaunchpadCoin);
    let shares = math::initial_shares(sol, tokens)?;
    let clock = Clock::get()?;
    let (key, creator_key, mint_key) = (pool.key(), creator.key(), mint.key());
    pool.set_inner(Pool {
        mint: mint_key, token_program: token_program.key(), creator: creator_key, terms: config.terms,
        sol_reserve: 0, token_reserve: 0, shares: 0, created_slot: clock.slot, seeded_slot: clock.slot,
        receipt_sequence: 0, bump: bumps.0, vault_bump: bumps.1,
    });
    position.set_inner(Position { pool: key, owner: creator_key, shares, bump: bumps.2 });
    transfer_in(&creator.to_account_info(), &pool.to_account_info(), &system_program.to_account_info(), sol)?;
    token_in(token_program, creator_token, mint, vault, &creator.to_account_info(), tokens)?;
    pool.sol_reserve = sol;
    pool.token_reserve = tokens;
    pool.shares = shares;
    events.emit(&PoolCreated { pool: key, mint: mint_key, token_program: pool.token_program, creator: creator_key, terms: pool.terms, decimals: mint.decimals, created_slot: clock.slot })?;
    let price = pool.price();
    receipt(events, pool, key, creator_key, CREATE, sol, Fees::default(), sol, tokens, shares, 0, price, clock.unix_timestamp)
}
type Two<'a, T> = (&'a T, &'a T);
/// Opens a pair of two token mints, `mint_a` the smaller address, with its first liquidity, which sets its price.
#[allow(clippy::too_many_arguments)]
fn open_pair<'info>(config: &Account<'info, Config>, creator: &Signer<'info>, mints: Two<Box<InterfaceAccount<'info, Mint>>>, pair: &mut Account<'info, Pair>, vaults: Two<Box<InterfaceAccount<'info, TokenAccount>>>, tokens: Two<Box<InterfaceAccount<'info, TokenAccount>>>, position: &mut Account<'info, Position>, markets: Two<UncheckedAccount<'info>>, programs: Two<Interface<'info, TokenInterface>>, events: &EventSink<'info>, bumps: (u8, u8, u8, u8), amount_a: u64, amount_b: u64, expected_config_revision: u64) -> Result<()> {
    require!(!config.paused, AmmError::CreationPaused);
    require!(config.revision == expected_config_revision, AmmError::ConfigChanged);
    mint::check(&mints.0.to_account_info())?;
    mint::check(&mints.1.to_account_info())?;
    require!(markets.0.owner != &LAUNCHPAD_ID && markets.1.owner != &LAUNCHPAD_ID, AmmError::LaunchpadCoin);
    let shares = math::initial_shares(amount_a, amount_b)?;
    let clock = Clock::get()?;
    let (key, creator_key) = (pair.key(), creator.key());
    let (mint_a, mint_b) = (mints.0.key(), mints.1.key());
    let (token_program_a, token_program_b) = (programs.0.key(), programs.1.key());
    pair.set_inner(Pair {
        mint_a, mint_b, token_program_a, token_program_b, creator: creator_key, terms: config.terms,
        reserve_a: 0, reserve_b: 0, protocol_a: 0, protocol_b: 0, shares: 0, created_slot: clock.slot, seeded_slot: clock.slot,
        receipt_sequence: 0, bump: bumps.0, vault_a_bump: bumps.1, vault_b_bump: bumps.2,
    });
    position.set_inner(Position { pool: key, owner: creator_key, shares, bump: bumps.3 });
    token_in(programs.0, tokens.0, mints.0, vaults.0, &creator.to_account_info(), amount_a)?;
    token_in(programs.1, tokens.1, mints.1, vaults.1, &creator.to_account_info(), amount_b)?;
    pair.reserve_a = amount_a;
    pair.reserve_b = amount_b;
    pair.shares = shares;
    events.emit(&PairCreated { pair: key, mint_a, mint_b, token_program_a, token_program_b, creator: creator_key, terms: pair.terms, decimals_a: mints.0.decimals, decimals_b: mints.1.decimals, created_slot: clock.slot })?;
    let price = pair.price();
    pair_receipt(events, pair, key, creator_key, CREATE, amount_a, amount_b, Fees::default(), shares, 0, price, clock.unix_timestamp)
}

/// A hook must be a program that can no longer change: under the upgradeable loader with no upgrade authority, or
/// under a loader whose programs are final. Its account for the venue must already exist, owned by it.
fn check_hook(hook: &AccountInfo, program_data: &AccountInfo, state: &AccountInfo, venue: &Pubkey) -> Result<()> {
    require!(hook.executable && hook.key() != crate::ID, AmmError::HookNotFinal);
    if hook.owner == &bpf_loader_upgradeable::ID {
        let program = hook.try_borrow_data()?;
        require!(program.len() >= 36 && program[0..4] == [2, 0, 0, 0] && program[4..36] == program_data.key().to_bytes(), AmmError::HookNotFinal);
        let data = program_data.try_borrow_data()?;
        // ProgramData: a u32 tag (3), the deploy slot, then the upgrade authority's option tag.
        require!(program_data.owner == &bpf_loader_upgradeable::ID && data.len() >= 13 && data[0..4] == [3, 0, 0, 0] && data[12] == 0, AmmError::HookNotFinal);
    } else {
        require!(hook.owner == &bpf_loader::ID || hook.owner == &bpf_loader_deprecated::ID, AmmError::HookNotFinal);
    }
    let expected = Pubkey::find_program_address(&[HOOK_STATE_SEED, venue.as_ref()], hook.key).0;
    require!(state.key() == expected && state.owner == hook.key, AmmError::HookAccounts);
    Ok(())
}
/// Tells a hooked venue's hook about a swap or add before any funds move. Every account goes to it without a
/// signature, so it can read and write only its own account; it can refuse the action by failing, and nothing else.
fn call_hook<'info>(remaining: &[AccountInfo<'info>], venue: &AccountInfo<'info>, base: usize, actor: &AccountInfo<'info>, call: HookCall) -> Result<()> {
    let Some(hook) = hook_of(venue, base)? else { return Ok(()) };
    let [program, state] = remaining else { return err!(AmmError::HookAccounts) };
    let expected = Pubkey::find_program_address(&[HOOK_STATE_SEED, venue.key.as_ref()], &hook).0;
    require!(program.key() == hook && state.key() == expected && state.is_writable, AmmError::HookAccounts);
    let mut data = HOOK_CALL_TAG.to_vec(); data.extend(call.try_to_vec()?);
    let instruction = Instruction { program_id: hook, accounts: vec![AccountMeta::new(state.key(), false), AccountMeta::new_readonly(venue.key(), false), AccountMeta::new_readonly(actor.key(), false)], data };
    invoke(&instruction, &[state.clone(), venue.clone(), actor.clone(), program.clone()]).map_err(Into::into)
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
    let bump = [pool.bump]; let hook = hook_of(&pool.to_account_info(), POOL_BASE_LEN)?.unwrap_or_default();
    let plain: [&[u8]; 3] = [b"pool", pool.mint.as_ref(), &bump];
    let hooked: [&[u8]; 4] = [b"pool", pool.mint.as_ref(), hook.as_ref(), &bump];
    let seeds: &[&[u8]] = if hook == Pubkey::default() { &plain } else { &hooked };
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
    #[msg("A pair names two different mints, the smaller address first")] PairOrder,
    #[msg("A hook must be a program that can no longer be upgraded")] HookNotFinal,
    #[msg("A hooked venue's swap or add passes its hook and the hook's account for it")] HookAccounts,
    #[msg("This account is not a venue at its own address")] InvalidVenue,
}
