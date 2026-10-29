#![allow(unexpected_cfgs)]
use anchor_lang::prelude::*;
use anchor_lang::solana_program::{instruction::{AccountMeta, Instruction}, program::{invoke, invoke_signed}, system_instruction};
use anchor_spl::token_interface::{self, TransferChecked, Burn};
use spl_token_2022::{extension::ExtensionType, state::{Mint as SplMint, Account as SplAccount}};
use spl_token_metadata_interface::state::TokenMetadata;

pub mod math;
pub mod state;
pub mod contexts;
pub mod vanity;
pub use state::*;
pub use contexts::*;
use math::{add, sub, Fees};

declare_id!("14pdiXAQsKiLDr29KxV4FNRLe14JjUVwQwTCz7BZSfix");

#[program]
pub mod fix_launchpad {
    use super::*;

    pub fn initialize_config(ctx: Context<InitializeConfig>, terms: Terms) -> Result<()> {
        terms.validate()?;
        ctx.accounts.config.set_inner(Config { revision:0, admin: ctx.accounts.admin.key(), pending_admin: Pubkey::default(), paused:false, terms, bump:ctx.bumps.config });
        Ok(())
    }
    /// Anyone may open a fee shard; its rent stays with the PDA, which no key signs for.
    pub fn create_protocol_shard(ctx: Context<CreateProtocolShard>, shard: u8) -> Result<()> {
        require!(shard < PROTOCOL_SHARDS, LaunchError::InvalidProtocolShard);
        ctx.accounts.protocol.set_inner(Protocol { accrued:0, shard, bump:ctx.bumps.protocol });
        Ok(())
    }
    pub fn update_config(ctx: Context<AdminConfig>, terms: Terms, paused: bool) -> Result<()> {
        terms.validate()?;
        ctx.accounts.config.terms=terms;
        ctx.accounts.config.paused=paused;
        ctx.accounts.config.revision=add(ctx.accounts.config.revision,1)?;
        Ok(())
    }
    pub fn propose_admin(ctx: Context<AdminConfig>, new_admin: Pubkey) -> Result<()> {
        require!(new_admin != Pubkey::default(), LaunchError::Unauthorized);
        ctx.accounts.config.pending_admin=new_admin;
        Ok(())
    }
    /// Admin only. Routed trades through `program_id` pay `recipient` `share_bps` of the protocol part of their fee.
    pub fn register_extension(ctx: Context<RegisterExtension>, program_id: Pubkey, share_bps: u16, recipient: Pubkey) -> Result<()> {
        require!(share_bps as u64 <= math::BPS, LaunchError::InvalidExtensionShare);
        require!(program_id != Pubkey::default() && program_id != crate::ID && recipient != Pubkey::default(), LaunchError::InvalidExtensionAccounts);
        let authority = Pubkey::find_program_address(&[EXTENSION_AUTHORITY_SEED], &program_id).0;
        ctx.accounts.extension.set_inner(Extension { program: program_id, authority, recipient, share_bps, paused: false, bump: ctx.bumps.extension });
        Ok(())
    }
    /// Admin only. A paused extension's routed trades are refused; plain trades never read the registry.
    pub fn set_extension_paused(ctx: Context<AdminExtension>, paused: bool) -> Result<()> {
        ctx.accounts.extension.paused = paused;
        Ok(())
    }
    /// Admin only. Closes the entry; its rent returns to the admin. Coins, markets and funds are untouched.
    pub fn remove_extension(_ctx: Context<RemoveExtension>) -> Result<()> { Ok(()) }
    pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
        ctx.accounts.config.admin=ctx.accounts.pending_admin.key();
        ctx.accounts.config.pending_admin=Pubkey::default();
        Ok(())
    }
    pub fn create_coin(ctx: Context<CreateCoin>, name: String, symbol: String, uri: String, expected_config_revision: u64) -> Result<()> {
        #[cfg(feature="measure-mint-suffix")]
        let probe_start=anchor_lang::solana_program::compute_units::sol_remaining_compute_units();
        #[cfg(feature="measure-mint-suffix")]
        let probe_end=anchor_lang::solana_program::compute_units::sol_remaining_compute_units();
        #[cfg(feature="measure-mint-suffix")]
        let suffix_start=anchor_lang::solana_program::compute_units::sol_remaining_compute_units();
        require!(vanity::has_fix_suffix(ctx.accounts.mint.key().as_ref()),LaunchError::InvalidMintSuffix);
        #[cfg(feature="measure-mint-suffix")]
        {
            let suffix_end=anchor_lang::solana_program::compute_units::sol_remaining_compute_units();
            msg!("MintSuffixCompute: checked={} probe={}",suffix_start-suffix_end,probe_start-probe_end);
        }
        require!(!ctx.accounts.config.paused, LaunchError::CreationPaused);
        require!(ctx.accounts.config.revision==expected_config_revision,LaunchError::ConfigChanged);
        require!(!name.is_empty() && name.len()<=64 && !symbol.is_empty() && symbol.len()<=16 && !uri.is_empty() && uri.len()<=256, LaunchError::InvalidMetadata);
        let clock=Clock::get()?;
        let now=clock.unix_timestamp;
        let mint_key=ctx.accounts.mint.key();
        let market_key=ctx.accounts.market.key();
        let creator=ctx.accounts.creator.key();
        let terms=ctx.accounts.config.terms;
        let events=EventSink::new(&ctx.accounts.event_authority,ctx.bumps.event_authority);
        let market=&mut ctx.accounts.market;
        market.mint=mint_key;
        market.creator=creator;
        market.terms=terms;
        market.phase=CURVE;
        market.virtual_sol=terms.virtual_sol;
        market.virtual_token=terms.virtual_token;
        market.curve_remaining=terms.curve_tokens;
        market.bump=ctx.bumps.market;
        market.vault_bump=ctx.bumps.vault;
        market.created_slot=clock.slot;
        let bump=[market.bump];
        let signer:&[&[u8]]=&[b"market", mint_key.as_ref(), &bump];
        let metadata=TokenMetadata { update_authority:Default::default(), mint:mint_key, name:name.clone(), symbol:symbol.clone(), uri:uri.clone(), additional_metadata:vec![] };
        let base_len=ExtensionType::try_calculate_account_len::<SplMint>(&[ExtensionType::MetadataPointer])?;
        let mint_len=base_len.checked_add(metadata.tlv_size_of()?).ok_or(error!(LaunchError::MathOverflow))?;
        let rent=Rent::get()?;
        // Initialize the fixed extension at its base length. Metadata's variable
        // extension grows the account; fund its final rent floor in advance.
        allocate_token_account(&ctx.accounts.creator.to_account_info(),&ctx.accounts.mint.to_account_info(),&ctx.accounts.system_program.to_account_info(),rent.minimum_balance(mint_len),base_len,&[])?;
        invoke(&spl_token_2022::extension::metadata_pointer::instruction::initialize(&spl_token_2022::id(),&mint_key,None,Some(mint_key))?,&[ctx.accounts.mint.to_account_info(),ctx.accounts.token_program.to_account_info()])?;
        invoke(&spl_token_2022::instruction::initialize_mint2(&spl_token_2022::id(),&mint_key,&market_key,None,terms.decimals)?,&[ctx.accounts.mint.to_account_info(),ctx.accounts.token_program.to_account_info()])?;
        invoke_signed(&spl_token_metadata_interface::instruction::initialize(&spl_token_2022::id(),&mint_key,&market_key,&mint_key,&market_key,name.clone(),symbol.clone(),uri.clone()),&[ctx.accounts.mint.to_account_info(),market.to_account_info(),ctx.accounts.token_program.to_account_info()],&[signer])?;
        invoke_signed(&spl_token_metadata_interface::instruction::update_authority(&spl_token_2022::id(),&mint_key,&market_key,Default::default()),&[ctx.accounts.mint.to_account_info(),market.to_account_info(),ctx.accounts.token_program.to_account_info()],&[signer])?;
        let vault_bump=[market.vault_bump];
        let vault_signer:&[&[u8]]=&[b"vault",market_key.as_ref(),&vault_bump];
        let vault_len=ExtensionType::try_calculate_account_len::<SplAccount>(&[])?;
        allocate_token_account(&ctx.accounts.creator.to_account_info(),&ctx.accounts.vault.to_account_info(),&ctx.accounts.system_program.to_account_info(),rent.minimum_balance(vault_len),vault_len,vault_signer)?;
        invoke(&spl_token_2022::instruction::initialize_account3(&spl_token_2022::id(),&ctx.accounts.vault.key(),&mint_key,&market_key)?,&[ctx.accounts.vault.to_account_info(),ctx.accounts.mint.to_account_info(),ctx.accounts.token_program.to_account_info()])?;
        invoke_signed(&spl_token_2022::instruction::mint_to_checked(&spl_token_2022::id(),&mint_key,&ctx.accounts.vault.key(),&market_key,&[],terms.total_supply,terms.decimals)?,&[ctx.accounts.mint.to_account_info(),ctx.accounts.vault.to_account_info(),market.to_account_info(),ctx.accounts.token_program.to_account_info()],&[signer])?;
        invoke_signed(&spl_token_2022::instruction::set_authority(&spl_token_2022::id(),&mint_key,None,spl_token_2022::instruction::AuthorityType::MintTokens,&market_key,&[])?,&[ctx.accounts.mint.to_account_info(),market.to_account_info(),ctx.accounts.token_program.to_account_info()],&[signer])?;
        events.emit(&CoinCreated { market:market_key,mint:mint_key,creator,terms,name,symbol,uri })?;
        let price=market.price()?;
        receipt(&events,market,market_key,creator,0,0,Fees::default(),Split::none(),0,0,0,0,price,now)?;
        Ok(())
    }
    // A trade with no remaining accounts is plain. An extension routes one by CPI with three remaining
    // accounts: its registry entry, its `["fix-extension"]` authority as signer, and its share recipient.
    pub fn buy_exact_in<'info>(ctx: Context<'_, '_, 'info, 'info, Trade<'info>>, gross: u64, min_tokens: u64) -> Result<()> { execute_buy(ctx,gross,None,min_tokens) }
    pub fn buy_exact_out<'info>(ctx: Context<'_, '_, 'info, 'info, Trade<'info>>, tokens: u64, max_gross: u64) -> Result<()> { execute_buy(ctx,max_gross,Some(tokens),tokens) }
    pub fn sell_exact_in<'info>(ctx: Context<'_, '_, 'info, 'info, Trade<'info>>, tokens: u64, min_sol: u64) -> Result<()> { execute_sell(ctx,tokens,None,min_sol) }
    pub fn sell_exact_out<'info>(ctx: Context<'_, '_, 'info, 'info, Trade<'info>>, sol: u64, max_tokens: u64) -> Result<()> { execute_sell(ctx,max_tokens,Some(sol),sol) }
    pub fn claim_creator_fees(ctx: Context<ClaimCreatorFees>) -> Result<()> {
        let events=EventSink::new(&ctx.accounts.event_authority,ctx.bumps.event_authority);
        let key=ctx.accounts.market.key();let market=&mut ctx.accounts.market;let amount=market.creator_fees;
        market.creator_fees=0;move_lamports(&market.to_account_info(),&ctx.accounts.creator.to_account_info(),amount)?;
        let price=market.price()?;
        receipt(&events,market,key,ctx.accounts.creator.key(),3,amount,Fees::default(),Split::none(),amount,0,0,price,price,Clock::get()?.unix_timestamp)?;
        Ok(())
    }

}

/// Events travel as self-CPI instruction data (Anchor `#[event_cpi]`), which the
/// runtime records in full. The 10,000-byte log budget can no longer drop them.
struct EventSink<'info> { authority: AccountInfo<'info>, bump: u8 }
impl<'info> EventSink<'info> {
    fn new(authority:&AccountInfo<'info>,bump:u8)->Self { Self { authority:authority.clone(), bump } }
    fn emit<E:anchor_lang::Event>(&self,event:&E)->Result<()> {
        let mut data=anchor_lang::event::EVENT_IX_TAG_LE.to_vec();data.extend(event.data());
        let instruction=Instruction::new_with_bytes(crate::ID,&data,vec![AccountMeta::new_readonly(self.authority.key(),true)]);
        invoke_signed(&instruction,&[self.authority.clone()],&[&[b"__event_authority",&[self.bump]]]).map_err(Into::into)
    }
}

/// A public address can receive SOL before launch. Allocate an empty system
/// account in place so pre-funding cannot prevent mint or vault creation.
fn allocate_token_account<'info>(payer:&AccountInfo<'info>,account:&AccountInfo<'info>,system:&AccountInfo<'info>,rent_floor:u64,space:usize,signer_seeds:&[&[u8]])->Result<()> {
    let signer_group=[signer_seeds];
    let signers=if signer_seeds.is_empty(){&[][..]}else{&signer_group[..]};
    if account.lamports()==0 {
        invoke_signed(&system_instruction::create_account(payer.key,account.key,rent_floor,space as u64,&spl_token_2022::id()),&[payer.clone(),account.clone(),system.clone()],signers)?;
    }else{
        transfer_in(payer,account,system,rent_floor.saturating_sub(account.lamports()))?;
        invoke_signed(&system_instruction::allocate(account.key,space as u64),&[account.clone(),system.clone()],signers)?;
        invoke_signed(&system_instruction::assign(account.key,&spl_token_2022::id()),&[account.clone(),system.clone()],signers)?;
    }
    Ok(())
}

fn transfer_in<'info>(from:&AccountInfo<'info>,to:&AccountInfo<'info>,system:&AccountInfo<'info>,amount:u64)->Result<()> {
    if amount>0 { invoke(&system_instruction::transfer(from.key,to.key,amount),&[from.clone(),to.clone(),system.clone()])?; } Ok(())
}
fn move_lamports(from:&AccountInfo,to:&AccountInfo,amount:u64)->Result<()> {
    if amount==0 {return Ok(());}
    let floor=Rent::get()?.minimum_balance(from.data_len());
    let remaining=from.lamports().checked_sub(amount).ok_or(error!(LaunchError::InsufficientLiquidity))?;
    require!(remaining>=floor,LaunchError::InsufficientLiquidity);
    let destination=to.lamports().checked_add(amount).ok_or(error!(LaunchError::MathOverflow))?;
    **from.try_borrow_mut_lamports()?=remaining;**to.try_borrow_mut_lamports()?=destination;Ok(())
}
fn credit_protocol(market:&Account<Market>,protocol:&mut Account<Protocol>,amount:u64)->Result<()> {
    move_lamports(&market.to_account_info(),&protocol.to_account_info(),amount)?;
    protocol.accrued=add(protocol.accrued,amount)?;Ok(())
}

/// The extension routing a trade, proven by its three remaining accounts. Anything else in that
/// position is refused by name; a plain trade passes none.
struct Route<'info> { program: Pubkey, share_bps: u16, recipient: AccountInfo<'info> }
fn route<'info>(remaining:&'info [AccountInfo<'info>])->Result<Option<Route<'info>>> {
    let (entry,authority,recipient)=match remaining { [] => return Ok(None), [entry,authority,recipient] => (entry,authority,recipient), _ => return err!(LaunchError::InvalidExtensionAccounts) };
    // Only register_extension creates Extension accounts, at their PDA; a removed entry is closed.
    if entry.owner!=&crate::ID {return err!(LaunchError::ExtensionNotRegistered);}
    let extension=Account::<Extension>::try_from(entry).map_err(|_|error!(LaunchError::ExtensionNotRegistered))?;
    require!(!extension.paused,LaunchError::ExtensionPaused);
    // Only the extension program can sign as a PDA derived under it, so the call came from that program.
    require!(authority.is_signer && authority.key()==extension.authority,LaunchError::ExtensionAuthorityInvalid);
    require!(recipient.is_writable && recipient.key()==extension.recipient,LaunchError::ExtensionRecipientMismatch);
    Ok(Some(Route { program:extension.program, share_bps:extension.share_bps, recipient:recipient.clone() }))
}
/// The protocol part of a fee as booked: the protocol's line and the routing extension's line.
struct Split { protocol: u64, extension: Pubkey, extension_fee: u64 }
impl Split { fn none() -> Self { Self { protocol:0, extension:Pubkey::default(), extension_fee:0 } } }
/// Pays the protocol part in the instruction that charges it: the extension's share to its recipient,
/// the rest to the protocol shard. The trader, the creator and the pool see the same amounts either way.
fn pay_protocol<'info>(market:&Account<'info,Market>,protocol:&mut Account<'info,Protocol>,route:&Option<Route<'info>>,fees:Fees)->Result<Split> {
    let (extension,extension_fee)=match route { Some(r) => (r.program,math::mul_div(fees.protocol,r.share_bps as u64,math::BPS)?), None => (Pubkey::default(),0) };
    let kept=sub(fees.protocol,extension_fee)?;
    credit_protocol(market,protocol,kept)?;
    if let Some(r)=route { move_lamports(&market.to_account_info(),&r.recipient,extension_fee)?; }
    Ok(Split { protocol:kept, extension, extension_fee })
}
fn quote_buy(market:&Market,budget:u64,exact_out:Option<u64>,rate:u16)->Result<(u64,u64,Fees)> {
    if budget==0 && exact_out.is_none() {return Ok((0,0,Fees::default()));}
    let cap=if market.phase==POOL {market.pool_tokens.saturating_sub(1)}else{market.curve_remaining};
    if let Some(out)=exact_out {
        require!(out>0 && out<=cap,LaunchError::InsufficientLiquidity);
        let required=if market.phase==POOL {math::pool_buy_net(market.real_sol,market.pool_token_num,market.pool_token_den,out)?}else{math::curve_buy_net(market.virtual_sol,market.virtual_token,out)?};
        let gross=math::gross_for_net(required,rate)?;
        require!(gross<=budget,LaunchError::SlippageExceeded);
        return Ok((gross,out,math::fees(gross,rate,&market.terms)?));
    }
    if market.phase!=POOL {
        let maximum_net=math::curve_buy_net(market.virtual_sol,market.virtual_token,cap)?;
        if let Ok(maximum_gross)=math::gross_for_net(maximum_net,rate) {
            if budget>=maximum_gross {return Ok((maximum_gross,cap,math::fees(maximum_gross,rate,&market.terms)?));}
        }
    }
    let mut gross=budget;
    let mut fees=math::fees(gross,rate,&market.terms)?;
    let net=gross-fees.total;
    let mut out=if market.phase==POOL {math::pool_buy_out(market.real_sol,market.pool_token_num,market.pool_token_den,net)?}else{math::curve_buy_out(market.virtual_sol,market.virtual_token,net)?};
    out=out.min(cap);
    if out==0 {return Ok((0,0,Fees::default()));}
    if market.phase!=POOL && out==cap {
        let required=if market.phase==POOL {math::pool_buy_net(market.real_sol,market.pool_token_num,market.pool_token_den,out)?}else{math::curve_buy_net(market.virtual_sol,market.virtual_token,out)?};
        gross=math::gross_for_net(required,rate)?;
        require!(gross<=budget,LaunchError::SlippageExceeded);
        fees=math::fees(gross,rate,&market.terms)?;
    }
    Ok((gross,out,fees))
}
fn apply_buy(market:&mut Market,gross:u64,out:u64,fees:Fees)->Result<()> {
    let reserve_in=add(gross-fees.total,fees.pool)?;
    market.real_sol=add(market.real_sol,reserve_in)?;
    if market.phase==POOL {
        market.pool_token_num=market.pool_token_num.checked_sub((out as u128)*market.pool_token_den as u128).ok_or(error!(LaunchError::InsufficientLiquidity))?;
        market.pool_tokens=sub(market.pool_tokens,out)?;
    }else{market.virtual_sol=add(market.virtual_sol,reserve_in)?;market.virtual_token=sub(market.virtual_token,out)?;market.curve_remaining=sub(market.curve_remaining,out)?;}
    Ok(())
}
fn execute_buy<'info>(ctx:Context<'_,'_,'info,'info,Trade<'info>>,budget:u64,exact_out:Option<u64>,min_tokens:u64)->Result<()> {
    let events=EventSink::new(&ctx.accounts.event_authority,ctx.bumps.event_authority);
    let routed=route(ctx.remaining_accounts)?;let via=routed.as_ref().map_or(Pubkey::default(),|r|r.program);
    let clock=Clock::get()?;let now=clock.unix_timestamp;let key=ctx.accounts.market.key();let trader=ctx.accounts.trader.key();let market=&mut ctx.accounts.market;
    require!(clock.slot>market.created_slot,LaunchError::BuyNotOpen);
    require!(budget>0,LaunchError::ZeroAmount);
    let before=market.price()?;
    let (gross,out,fees)=quote_buy(market,budget,exact_out,market.rate())?;
    require!(out>0 && out>=min_tokens,LaunchError::SlippageExceeded);
    transfer_in(&ctx.accounts.trader.to_account_info(),&market.to_account_info(),&ctx.accounts.system_program.to_account_info(),gross)?;
    apply_buy(market,gross,out,fees)?;
    market.creator_fees=add(market.creator_fees,fees.creator)?;
    let bump=[market.bump];let mint=market.mint;let seeds:&[&[u8]]=&[b"market",mint.as_ref(),&bump];
    token_interface::transfer_checked(CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(),TransferChecked {from:ctx.accounts.vault.to_account_info(),mint:ctx.accounts.mint.to_account_info(),to:ctx.accounts.trader_token.to_account_info(),authority:market.to_account_info()},&[seeds]),out,market.terms.decimals)?;
    if market.phase==CURVE && market.curve_remaining==0 {graduate(&events,via,market,key,&ctx.accounts.mint.to_account_info(),&ctx.accounts.vault.to_account_info(),&ctx.accounts.token_program.to_account_info(),now)?;}
    let split=pay_protocol(market,&mut ctx.accounts.protocol,&routed,fees)?;
    let after=market.price()?;let refund=if exact_out.is_none(){budget-gross}else{0};
    receipt(&events,market,key,trader,1,gross,fees,split,gross-fees.total,out,refund,before,after,now)?;Ok(())
}
fn execute_sell<'info>(ctx:Context<'_,'_,'info,'info,Trade<'info>>,budget:u64,exact_out:Option<u64>,min_sol:u64)->Result<()> {
    let events=EventSink::new(&ctx.accounts.event_authority,ctx.bumps.event_authority);
    let routed=route(ctx.remaining_accounts)?;
    let now=Clock::get()?.unix_timestamp;let key=ctx.accounts.market.key();let trader=ctx.accounts.trader.key();let market=&mut ctx.accounts.market;
    require!(budget>0,LaunchError::ZeroAmount);
    let before=market.price()?;let rate=market.rate();let mut input=budget;
    let gross;
    let net=if let Some(wanted)=exact_out {
        require!(wanted>0,LaunchError::ZeroAmount);gross=math::gross_for_net(wanted,rate)?;
        input=if market.phase==POOL {math::pool_sell_in(market.real_sol,market.pool_token_num,market.pool_token_den,gross)?}else{math::curve_sell_in(market.virtual_sol,market.virtual_token,gross)?};
        require!(input<=budget,LaunchError::SlippageExceeded);wanted
    }else{
        gross=if market.phase==POOL {math::pool_sell_out(market.real_sol,market.pool_token_num,market.pool_token_den,input)?}else{math::curve_sell_out(market.virtual_sol,market.virtual_token,input)?};
        gross-math::fees(gross,rate,&market.terms)?.total
    };
    require!(net>=min_sol && net>0,LaunchError::SlippageExceeded);
    let fees=math::fees(gross,rate,&market.terms)?;
    require!(gross<=market.real_sol,LaunchError::InsufficientLiquidity);
    token_interface::transfer_checked(CpiContext::new(ctx.accounts.token_program.to_account_info(),TransferChecked {from:ctx.accounts.trader_token.to_account_info(),mint:ctx.accounts.mint.to_account_info(),to:ctx.accounts.vault.to_account_info(),authority:ctx.accounts.trader.to_account_info()}),input,market.terms.decimals)?;
    let reserve_out=gross-fees.pool;
    market.real_sol=sub(market.real_sol,reserve_out)?;
    if market.phase==POOL {market.pool_token_num=market.pool_token_num.checked_add((input as u128)*market.pool_token_den as u128).ok_or(error!(LaunchError::MathOverflow))?;market.pool_tokens=add(market.pool_tokens,input)?;}
    else{market.virtual_sol=sub(market.virtual_sol,reserve_out)?;market.virtual_token=add(market.virtual_token,input)?;market.curve_remaining=add(market.curve_remaining,input)?;require!(market.curve_remaining<=market.terms.curve_tokens,LaunchError::InsufficientLiquidity);}
    market.creator_fees=add(market.creator_fees,fees.creator)?;let split=pay_protocol(market,&mut ctx.accounts.protocol,&routed,fees)?;
    move_lamports(&market.to_account_info(),&ctx.accounts.trader.to_account_info(),net)?;
    let after=market.price()?;
    receipt(&events,market,key,trader,2,gross,fees,split,net,input,0,before,after,now)?;Ok(())
}

fn graduate<'info>(events:&EventSink<'info>,via:Pubkey,market:&mut Account<'info,Market>,key:Pubkey,mint:&AccountInfo<'info>,vault:&AccountInfo<'info>,token_program:&AccountInfo<'info>,now:i64)->Result<()> {
    let before=market.price()?;
    require!(market.real_sol>0 && market.virtual_token>0,LaunchError::InsufficientLiquidity);
    let numerator=(market.real_sol as u128)*(market.virtual_token as u128);
    let denominator=market.virtual_sol;
    let reserve=math::to_u64(math::ceil_div(numerator,denominator as u128)?)?;
    let circulating=market.terms.curve_tokens;
    let available=sub(market.terms.total_supply,circulating)?;
    require!(reserve<=available,LaunchError::InsufficientLiquidity);
    let burn=available-reserve;
    if burn>0 {
        let bump=[market.bump];let mint_key=market.mint;let seeds:&[&[u8]]=&[b"market",mint_key.as_ref(),&bump];
        token_interface::burn(CpiContext::new_with_signer(token_program.clone(),Burn {mint:mint.clone(),from:vault.clone(),authority:market.to_account_info()},&[seeds]),burn)?;
    }
    market.pool_token_num=numerator;market.pool_token_den=denominator;market.pool_tokens=reserve;market.burned_tokens=burn;market.phase=POOL;
    let after=market.price()?;
    events.emit(&Graduated {market:key,real_sol:market.real_sol,token_reserve:reserve,token_numerator:numerator,token_denominator:denominator,burned:burn,last_curve_price:before,pool_price:after})?;
    // The graduation receipt names the route of the buy that caused it; it carries no fee.
    receipt(events,market,key,Pubkey::default(),4,0,Fees::default(),Split { extension:via, ..Split::none() },0,burn,0,before,after,now)?;Ok(())
}

#[allow(clippy::too_many_arguments)]
fn receipt(events:&EventSink,market:&mut Market,key:Pubkey,actor:Pubkey,action:u8,gross:u64,fees:Fees,split:Split,net:u64,tokens:u64,refund:u64,before:u128,after:u128,now:i64)->Result<()> {
    market.receipt_sequence=add(market.receipt_sequence,1)?;
    events.emit(&Receipt {market:key,actor,action,phase:market.phase,sequence:market.receipt_sequence,timestamp:now,is_creator:actor==market.creator,gross,protocol_fee:split.protocol,creator_fee:fees.creator,pool_fee:fees.pool,net,tokens,refund,price_before:before,price_after:after,price_impact_bps:math::impact(before,after)?,extension:split.extension,extension_fee:split.extension_fee})
}

#[error_code]
pub enum LaunchError {
    #[msg("Invalid economic terms")] InvalidTerms,
    #[msg("Arithmetic exceeds supported integer bounds")] MathOverflow,
    #[msg("Creation is paused")] CreationPaused,
    #[msg("Invalid token metadata")] InvalidMetadata,
    #[msg("Amount must be positive")] ZeroAmount,
    #[msg("Slippage bound would be exceeded")] SlippageExceeded,
    #[msg("Insufficient backed liquidity")] InsufficientLiquidity,
    #[msg("Authority is not authorized")] Unauthorized,
    #[msg("Configuration changed since the creation preview")] ConfigChanged,
    #[msg("Buying opens in the slot after coin creation")] BuyNotOpen,
    #[msg("Mint address must end in lowercase fix")] InvalidMintSuffix,
    #[msg("Protocol fee shard is out of range")] InvalidProtocolShard,
    #[msg("A routed trade takes exactly three extension accounts")] InvalidExtensionAccounts,
    #[msg("The extension is not registered")] ExtensionNotRegistered,
    #[msg("The extension is paused")] ExtensionPaused,
    #[msg("The extension authority is not its signing PDA")] ExtensionAuthorityInvalid,
    #[msg("The share account is not the extension's recipient")] ExtensionRecipientMismatch,
    #[msg("An extension share is at most 10,000 bps of the protocol part")] InvalidExtensionShare,
}
