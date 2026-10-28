#![allow(unexpected_cfgs)]
use anchor_lang::prelude::*;
use anchor_lang::solana_program::{program::{invoke, invoke_signed}, system_instruction};
use anchor_spl::token_interface::{self, TransferChecked, Burn};
use spl_token_2022::{extension::ExtensionType, state::{Mint as SplMint, Account as SplAccount}};
use spl_token_metadata_interface::state::TokenMetadata;

pub mod math;
pub mod state;
pub mod contexts;
pub mod oracle;
pub use state::*;
pub use contexts::*;
use math::{add, sub, Fees};

declare_id!("Fg6PaFpoGXkYsidMpWxTWqkZq6W2BeZ7FEfcYkgMQHG");

#[program]
pub mod fix_launchpad {
    use super::*;

    pub fn initialize_config(ctx: Context<InitializeConfig>, terms: Terms) -> Result<()> {
        terms.validate()?;
        ctx.accounts.config.set_inner(Config { revision:0, admin: ctx.accounts.admin.key(), pending_admin: Pubkey::default(), paused:false, terms, bump:ctx.bumps.config });
        ctx.accounts.protocol.set_inner(Protocol { accrued:0, bump:ctx.bumps.protocol });
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
    pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
        ctx.accounts.config.admin=ctx.accounts.pending_admin.key();
        ctx.accounts.config.pending_admin=Pubkey::default();
        Ok(())
    }
    pub fn create_coin(ctx: Context<CreateCoin>, name: String, symbol: String, uri: String, expected_config_revision: u64) -> Result<()> {
        require!(!ctx.accounts.config.paused, LaunchError::CreationPaused);
        require!(ctx.accounts.config.revision==expected_config_revision,LaunchError::ConfigChanged);
        require!(!name.is_empty() && name.len()<=64 && !symbol.is_empty() && symbol.len()<=16 && !uri.is_empty() && uri.len()<=256, LaunchError::InvalidMetadata);
        let clock=Clock::get()?;
        let now=clock.unix_timestamp;
        let mint_key=ctx.accounts.mint.key();
        let market_key=ctx.accounts.market.key();
        let creator=ctx.accounts.creator.key();
        let terms=ctx.accounts.config.terms;
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
        emit!(CoinCreated { market:market_key,mint:mint_key,creator,terms,name,symbol,uri });
        oracle::initialize(&mut *ctx.accounts.price_history.load_init()?,market,market_key,ctx.bumps.price_history,now)?;
        let price=market.price()?;
        receipt(market,market_key,creator,0,0,Fees::default(),0,0,0,0,price,Pubkey::default(),now)?;
        Ok(())
    }
    pub fn prepare_position(ctx: Context<PreparePosition>) -> Result<()> {
        require_keys_eq!(ctx.accounts.payer.key(),ctx.accounts.follower.key(),LaunchError::PositionRentPayerMismatch);
        require!(Clock::get()?.unix_timestamp<ctx.accounts.call.horizon,LaunchError::CallExpired);
        let position=&mut ctx.accounts.position;
        if position.call == Pubkey::default() {
            require!(ctx.accounts.follower.key()!=ctx.accounts.call.caller,LaunchError::SelfFollow);
            position.call=ctx.accounts.call.key(); position.market=ctx.accounts.market.key();position.follower=ctx.accounts.follower.key();position.bump=ctx.bumps.position;
            position.caller=ctx.accounts.call.caller;position.horizon=ctx.accounts.call.horizon;position.rent_payer=ctx.accounts.payer.key();
            ctx.accounts.call.position_count=add(ctx.accounts.call.position_count,1)?;
        }
        Ok(())
    }
    pub fn buy_exact_in(ctx: Context<Trade>, gross: u64, min_tokens: u64) -> Result<()> { execute_buy(ctx,gross,None,min_tokens) }
    pub fn buy_exact_out(ctx: Context<Trade>, tokens: u64, max_gross: u64) -> Result<()> { execute_buy(ctx,max_gross,Some(tokens),tokens) }
    pub fn sell_exact_in(ctx: Context<Trade>, tokens: u64, min_sol: u64) -> Result<()> { execute_sell(ctx,tokens,None,min_sol) }
    pub fn sell_exact_out(ctx: Context<Trade>, sol: u64, max_tokens: u64) -> Result<()> { execute_sell(ctx,max_tokens,Some(sol),sol) }
    pub fn claim_creator_fees(ctx: Context<ClaimCreatorFees>) -> Result<()> {
        let key=ctx.accounts.market.key();let market=&mut ctx.accounts.market;let amount=market.creator_fees;
        market.creator_fees=0;move_lamports(&market.to_account_info(),&ctx.accounts.creator.to_account_info(),amount)?;
        let price=market.price()?;
        receipt(market,key,ctx.accounts.creator.key(),3,amount,Fees::default(),amount,0,0,price,price,Pubkey::default(),Clock::get()?.unix_timestamp)?;
        Ok(())
    }
    pub fn open_call(ctx: Context<OpenCall>, id: u64, horizon_seconds: i64) -> Result<()> {
        let now=Clock::get()?.unix_timestamp;let key=ctx.accounts.market.key();let market=&ctx.accounts.market;
        let terms=market.terms;
        let end=oracle::horizon(now,horizon_seconds,terms.min_horizon,terms.max_horizon)?;
        let start=end.checked_sub(terms.scoring_window).ok_or(error!(LaunchError::MathOverflow))?;
        let sequence=&mut ctx.accounts.caller_sequence;
        if sequence.market==Pubkey::default() {
            sequence.market=key;sequence.caller=ctx.accounts.caller.key();sequence.bump=ctx.bumps.caller_sequence;
        }
        require!(sequence.market==key && sequence.caller==ctx.accounts.caller.key() && sequence.bump==ctx.bumps.caller_sequence,LaunchError::InvalidCall);
        require!(id==sequence.next_id,LaunchError::CallSequenceChanged);
        sequence.next_id=add(sequence.next_id,1)?;
        ctx.accounts.call.set_inner(Call {market:key,caller:ctx.accounts.caller.key(),id,opened_at:now,window_start:start,horizon:end,price_integral:0,price_integral_hi:0,checkpointed:false,evidence_missing:false,position_count:0,follower_count:0,settled_positions:0,earned:0,refunded:0,claimable_payout:0,bump:ctx.bumps.call});
        emit!(CallOpened {call:ctx.accounts.call.key(),market:key,caller:ctx.accounts.caller.key(),window_start:start,horizon:end});
        Ok(())
    }
    pub fn settle_call(ctx: Context<SettleCall>) -> Result<()> {
        let now=Clock::get()?.unix_timestamp;let call=&mut ctx.accounts.call;let pos=&mut ctx.accounts.position;
        if pos.settled {return Ok(());}
        require!(now>=call.horizon,LaunchError::CallNotMature);
        require!(ctx.remaining_accounts.is_empty(),LaunchError::InvalidOracleProof);
        let call_key=call.key();
        oracle::checkpoint(call,call_key,&ctx.accounts.market,&*ctx.accounts.price_history.load()?,now)?;
        let value=call.value(pos.tokens,ctx.accounts.market.terms.scoring_window)?;
        let profitable=!call.evidence_missing && pos.cost>0 && call.is_profitable(pos.tokens,pos.cost,ctx.accounts.market.terms.scoring_window)?;
        let payout=pos.escrow;
        let payout_deferred=profitable && payout>0 && !can_receive_lamports(&ctx.accounts.caller.to_account_info(),payout)?;
        let payee=if payout_deferred {call.claimable_payout=add(call.claimable_payout,payout)?;call.to_account_info()}else if profitable {ctx.accounts.caller.to_account_info()}else{ctx.accounts.follower.to_account_info()};
        move_lamports(&pos.to_account_info(),&payee,payout)?;
        pos.escrow=0;pos.settled=true;pos.profitable=profitable;pos.exit_value=value;
        call.settled_positions=add(call.settled_positions,1)?;
        if pos.has_activity {call.follower_count=add(call.follower_count,1)?;}
        if profitable {call.earned=add(call.earned,payout)?;}else{call.refunded=add(call.refunded,payout)?;}
        emit!(CallSettled {call:call.key(),follower:pos.follower,caller:call.caller,profitable,cost:pos.cost,tokens:pos.tokens,exit_value:value,payout,price_integral:call.price_integral,price_integral_hi:call.price_integral_hi,payout_deferred,evidence_missing:call.evidence_missing});
        let average=call.average(ctx.accounts.market.terms.scoring_window)?;
        let market_key=ctx.accounts.market.key();
        receipt(&mut ctx.accounts.market,market_key,pos.follower,5,payout,Fees::default(),payout,pos.tokens,if profitable {0}else{payout},average,average,call.key(),now)?;
        Ok(())
    }
    pub fn checkpoint_call(ctx:Context<CheckpointCall>)->Result<()> {
        require!(ctx.remaining_accounts.is_empty(),LaunchError::InvalidOracleProof);
        let key=ctx.accounts.call.key();
        oracle::checkpoint(&mut ctx.accounts.call,key,&ctx.accounts.market,&*ctx.accounts.price_history.load()?,Clock::get()?.unix_timestamp)
    }
    pub fn close_call(ctx:Context<CloseCall>)->Result<()> {
        let call=&ctx.accounts.call;require!(Clock::get()?.unix_timestamp>=call.horizon,LaunchError::CallNotMature);
        require!(call.position_count==call.settled_positions,LaunchError::UnsettledCallPositions);
        emit!(CallClosed {call:call.key(),caller:call.caller,position_count:call.position_count,earned:call.earned,refunded:call.refunded,payout:call.claimable_payout});Ok(())
    }
    pub fn close_position(ctx:Context<ClosePosition>)->Result<()> {
        let position=&ctx.accounts.position;
        require!(position.cost==0 && position.tokens==0 && position.escrow==0 && !position.has_activity,LaunchError::PositionStillActive);
        ctx.accounts.call.settled_positions=add(ctx.accounts.call.settled_positions,1)?;
        emit!(PositionClosed {call:position.call,follower:position.follower,rent_payer:position.rent_payer});Ok(())
    }
    pub fn claim_call_payout(ctx:Context<ClaimCallPayout>)->Result<()> {
        let call=&mut ctx.accounts.call;let payout=call.claimable_payout;
        if payout==0 {return Ok(());}
        require!(can_receive_lamports(&ctx.accounts.caller.to_account_info(),payout)?,LaunchError::RecipientNeedsRent);
        move_lamports(&call.to_account_info(),&ctx.accounts.caller.to_account_info(),payout)?;
        call.claimable_payout=0;
        emit!(CallPayoutClaimed {call:call.key(),caller:call.caller,payout});
        let key=ctx.accounts.market.key();let price=ctx.accounts.market.price()?;
        receipt(&mut ctx.accounts.market,key,call.caller,6,payout,Fees::default(),payout,0,0,price,price,call.key(),Clock::get()?.unix_timestamp)?;Ok(())
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
fn can_receive_lamports(destination:&AccountInfo,amount:u64)->Result<bool> {
    let after=destination.lamports().checked_add(amount).ok_or(error!(LaunchError::MathOverflow))?;
    Ok(after>=Rent::get()?.minimum_balance(destination.data_len()))
}
fn credit_protocol(market:&Account<Market>,protocol:&mut Account<Protocol>,amount:u64)->Result<()> {
    move_lamports(&market.to_account_info(),&protocol.to_account_info(),amount)?;
    protocol.accrued=add(protocol.accrued,amount)?;Ok(())
}
fn check_tag(pos:&Option<Account<CallPosition>>,market:Pubkey,trader:Pubkey,now:i64)->Result<Pubkey> {
    let Some(pos)=pos else{return Ok(Pubkey::default());};
    require!(pos.market==market && pos.follower==trader && pos.rent_payer==trader,LaunchError::InvalidCall);
    let expected=Pubkey::find_program_address(&[b"position",pos.call.as_ref(),trader.as_ref()],&crate::ID).0;
    require_keys_eq!(pos.key(),expected,LaunchError::InvalidCall);
    require!(pos.caller!=trader,LaunchError::SelfFollow);require!(now<pos.horizon,LaunchError::CallExpired);require!(!pos.settled,LaunchError::PositionSettled);Ok(pos.call)
}

fn quote_buy(market:&Market,budget:u64,exact_out:Option<u64>,rate:u16,tagged:bool)->Result<(u64,u64,Fees)> {
    if budget==0 && exact_out.is_none() {return Ok((0,0,Fees::default()));}
    let cap=if market.phase==POOL {market.pool_tokens.saturating_sub(1)}else{market.curve_remaining};
    if let Some(out)=exact_out {
        require!(out>0 && out<=cap,LaunchError::InsufficientLiquidity);
        let required=if market.phase==POOL {math::pool_buy_net(market.real_sol,market.pool_token_num,market.pool_token_den,out)?}else{math::curve_buy_net(market.virtual_sol,market.virtual_token,out)?};
        let gross=math::gross_for_net(required,rate)?;
        require!(gross<=budget,LaunchError::SlippageExceeded);
        return Ok((gross,out,math::fees(gross,rate,&market.terms,tagged)?));
    }
    if market.phase!=POOL {
        let maximum_net=math::curve_buy_net(market.virtual_sol,market.virtual_token,cap)?;
        if let Ok(maximum_gross)=math::gross_for_net(maximum_net,rate) {
            if budget>=maximum_gross {return Ok((maximum_gross,cap,math::fees(maximum_gross,rate,&market.terms,tagged)?));}
        }
    }
    let mut gross=budget;
    let mut fees=math::fees(gross,rate,&market.terms,tagged)?;
    let net=gross-fees.total;
    let mut out=if market.phase==POOL {math::pool_buy_out(market.real_sol,market.pool_token_num,market.pool_token_den,net)?}else{math::curve_buy_out(market.virtual_sol,market.virtual_token,net)?};
    out=out.min(cap);
    if out==0 {return Ok((0,0,Fees::default()));}
    if market.phase!=POOL && out==cap {
        let required=if market.phase==POOL {math::pool_buy_net(market.real_sol,market.pool_token_num,market.pool_token_den,out)?}else{math::curve_buy_net(market.virtual_sol,market.virtual_token,out)?};
        gross=math::gross_for_net(required,rate)?;
        require!(gross<=budget,LaunchError::SlippageExceeded);
        fees=math::fees(gross,rate,&market.terms,tagged)?;
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
fn execute_buy(ctx:Context<Trade>,budget:u64,exact_out:Option<u64>,min_tokens:u64)->Result<()> {
    let clock=Clock::get()?;let now=clock.unix_timestamp;let key=ctx.accounts.market.key();let trader=ctx.accounts.trader.key();let market=&mut ctx.accounts.market;
    require!(clock.slot>market.created_slot,LaunchError::BuyNotOpen);
    require!(budget>0,LaunchError::ZeroAmount);
    let tag=check_tag(&ctx.accounts.position,key,trader,now)?;
    let before=market.price()?;
    let (gross,out,fees)=quote_buy(market,budget,exact_out,market.rate(),tag!=Pubkey::default())?;
    require!(out>0 && out>=min_tokens,LaunchError::SlippageExceeded);
    transfer_in(&ctx.accounts.trader.to_account_info(),&market.to_account_info(),&ctx.accounts.system_program.to_account_info(),gross)?;
    apply_buy(market,gross,out,fees)?;
    market.creator_fees=add(market.creator_fees,fees.creator)?;
    if let Some(pos)=ctx.accounts.position.as_mut() { pos.has_activity=true;pos.cost=add(pos.cost,gross)?;pos.tokens=add(pos.tokens,out)?;pos.escrow=add(pos.escrow,fees.call)?; }
    let bump=[market.bump];let mint=market.mint;let seeds:&[&[u8]]=&[b"market",mint.as_ref(),&bump];
    token_interface::transfer_checked(CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(),TransferChecked {from:ctx.accounts.vault.to_account_info(),mint:ctx.accounts.mint.to_account_info(),to:ctx.accounts.trader_token.to_account_info(),authority:market.to_account_info()},&[seeds]),out,market.terms.decimals)?;
    if market.phase==CURVE && market.curve_remaining==0 {graduate(market,key,&ctx.accounts.mint.to_account_info(),&ctx.accounts.vault.to_account_info(),&ctx.accounts.token_program.to_account_info(),now)?;}
    credit_protocol(market,&mut ctx.accounts.protocol,fees.protocol)?;
    if let Some(pos)=ctx.accounts.position.as_ref() {move_lamports(&market.to_account_info(),&pos.to_account_info(),fees.call)?;}
    require!(ctx.remaining_accounts.is_empty(),LaunchError::InvalidOracleProof);
    oracle::record(market,key,&mut *ctx.accounts.price_history.load_mut()?,now)?;
    let after=market.price()?;let refund=if exact_out.is_none(){budget-gross}else{0};
    receipt(market,key,trader,1,gross,fees,gross-fees.total,out,refund,before,after,tag,now)?;Ok(())
}
fn execute_sell(ctx:Context<Trade>,budget:u64,exact_out:Option<u64>,min_sol:u64)->Result<()> {
    let now=Clock::get()?.unix_timestamp;let key=ctx.accounts.market.key();let trader=ctx.accounts.trader.key();let market=&mut ctx.accounts.market;
    require!(budget>0,LaunchError::ZeroAmount);
    require!(ctx.accounts.position.is_none(),LaunchError::InvalidCall);
    let before=market.price()?;let rate=market.rate();let mut input=budget;
    let gross;
    let net=if let Some(wanted)=exact_out {
        require!(wanted>0,LaunchError::ZeroAmount);gross=math::gross_for_net(wanted,rate)?;
        input=if market.phase==POOL {math::pool_sell_in(market.real_sol,market.pool_token_num,market.pool_token_den,gross)?}else{math::curve_sell_in(market.virtual_sol,market.virtual_token,gross)?};
        require!(input<=budget,LaunchError::SlippageExceeded);wanted
    }else{
        gross=if market.phase==POOL {math::pool_sell_out(market.real_sol,market.pool_token_num,market.pool_token_den,input)?}else{math::curve_sell_out(market.virtual_sol,market.virtual_token,input)?};
        gross-math::fees(gross,rate,&market.terms,false)?.total
    };
    require!(net>=min_sol && net>0,LaunchError::SlippageExceeded);
    let fees=math::fees(gross,rate,&market.terms,false)?;
    require!(gross<=market.real_sol,LaunchError::InsufficientLiquidity);
    token_interface::transfer_checked(CpiContext::new(ctx.accounts.token_program.to_account_info(),TransferChecked {from:ctx.accounts.trader_token.to_account_info(),mint:ctx.accounts.mint.to_account_info(),to:ctx.accounts.vault.to_account_info(),authority:ctx.accounts.trader.to_account_info()}),input,market.terms.decimals)?;
    let reserve_out=gross-fees.pool;
    market.real_sol=sub(market.real_sol,reserve_out)?;
    if market.phase==POOL {market.pool_token_num=market.pool_token_num.checked_add((input as u128)*market.pool_token_den as u128).ok_or(error!(LaunchError::MathOverflow))?;market.pool_tokens=add(market.pool_tokens,input)?;}
    else{market.virtual_sol=sub(market.virtual_sol,reserve_out)?;market.virtual_token=add(market.virtual_token,input)?;market.curve_remaining=add(market.curve_remaining,input)?;require!(market.curve_remaining<=market.terms.curve_tokens,LaunchError::InsufficientLiquidity);}
    market.creator_fees=add(market.creator_fees,fees.creator)?;credit_protocol(market,&mut ctx.accounts.protocol,fees.protocol)?;
    move_lamports(&market.to_account_info(),&ctx.accounts.trader.to_account_info(),net)?;
    require!(ctx.remaining_accounts.is_empty(),LaunchError::InvalidOracleProof);
    oracle::record(market,key,&mut *ctx.accounts.price_history.load_mut()?,now)?;
    let after=market.price()?;
    receipt(market,key,trader,2,gross,fees,net,input,0,before,after,Pubkey::default(),now)?;Ok(())
}

fn graduate<'info>(market:&mut Account<'info,Market>,key:Pubkey,mint:&AccountInfo<'info>,vault:&AccountInfo<'info>,token_program:&AccountInfo<'info>,now:i64)->Result<()> {
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
    emit!(Graduated {market:key,real_sol:market.real_sol,token_reserve:reserve,token_numerator:numerator,token_denominator:denominator,burned:burn,last_curve_price:before,pool_price:after});
    receipt(market,key,Pubkey::default(),4,0,Fees::default(),0,burn,0,before,after,Pubkey::default(),now)?;Ok(())
}

#[allow(clippy::too_many_arguments)]
fn receipt(market:&mut Market,key:Pubkey,actor:Pubkey,action:u8,gross:u64,fees:Fees,net:u64,tokens:u64,refund:u64,before:u128,after:u128,call:Pubkey,now:i64)->Result<()> {
    market.receipt_sequence=add(market.receipt_sequence,1)?;
    emit!(Receipt {market:key,actor,action,phase:market.phase,sequence:market.receipt_sequence,timestamp:now,is_creator:actor==market.creator,gross,protocol_fee:fees.protocol,creator_fee:fees.creator,call_fee:fees.call,pool_fee:fees.pool,net,tokens,refund,price_before:before,price_after:after,price_impact_bps:math::impact(before,after)?,call});Ok(())
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
    #[msg("A caller cannot tag their own trade")] SelfFollow,
    #[msg("Invalid call or follower position")] InvalidCall,
    #[msg("Call horizon has elapsed")] CallExpired,
    #[msg("Call position already settled")] PositionSettled,
    #[msg("Call horizon is outside the market's locked bounds")] InvalidHorizon,
    #[msg("Call horizon has not elapsed")] CallNotMature,
    #[msg("Clock moved backwards")] ClockWentBackwards,
    #[msg("Authority is not authorized")] Unauthorized,
    #[msg("Configuration changed since the creation preview")] ConfigChanged,
    #[msg("Price-history proof does not bracket the exact window edges")] InvalidOracleProof,
    #[msg("Every registered call position must settle before closing")] UnsettledCallPositions,
    #[msg("The follower must pay their own position account rent")] PositionRentPayerMismatch,
    #[msg("This caller's call sequence changed; rebuild with their next call ID")] CallSequenceChanged,
    #[msg("An active position must settle at its horizon before closing")] PositionStillActive,
    #[msg("The payout recipient must be rent-exempt; fund it or close the completed call to receive its rent and payout together")] RecipientNeedsRent,
    #[msg("Buying opens in the slot after coin creation")] BuyNotOpen,
}
