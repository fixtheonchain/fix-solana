use anchor_spl::token_2022::Token2022;
use anchor_spl::token_interface::{Mint, TokenAccount};
use crate::*;

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(mut)] pub admin: Signer<'info>,
    #[account(constraint=program.programdata_address()? == Some(program_data.key()) @ LaunchError::Unauthorized)]
    pub program: Program<'info, crate::program::FixLaunchpad>,
    #[account(constraint=program_data.upgrade_authority_address == Some(admin.key()) @ LaunchError::Unauthorized)]
    pub program_data: Account<'info, ProgramData>,
    #[account(init, payer=admin, space=8+Config::INIT_SPACE, seeds=[b"config"], bump)] pub config: Account<'info, Config>,
    #[account(init, payer=admin, space=8+Protocol::INIT_SPACE, seeds=[b"protocol"], bump)] pub protocol: Account<'info, Protocol>,
    pub system_program: Program<'info, System>,
}
#[derive(Accounts)]
pub struct AdminConfig<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds=[b"config"], bump=config.bump, has_one=admin)] pub config: Account<'info, Config>,
}
#[derive(Accounts)]
pub struct AcceptAdmin<'info> {
    pub pending_admin: Signer<'info>,
    #[account(mut, seeds=[b"config"], bump=config.bump, has_one=pending_admin)] pub config: Account<'info, Config>,
}
#[derive(Accounts)]
pub struct CreateCoin<'info> {
    #[account(mut)] pub creator: Signer<'info>,
    #[account(seeds=[b"config"], bump=config.bump)] pub config: Account<'info, Config>,
    /// CHECK: A new signer account allocated and initialized as Token-2022 in the handler.
    #[account(mut)] pub mint: Signer<'info>,
    #[account(init, payer=creator, space=8+Market::INIT_SPACE, seeds=[b"market",mint.key().as_ref()], bump)] pub market: Account<'info, Market>,
    #[account(init,payer=creator,space=PriceHistory::SPACE,seeds=[b"prices",market.key().as_ref()],bump)] pub price_history: AccountLoader<'info,PriceHistory>,
    /// CHECK: The vault PDA is allocated and initialized by the handler.
    #[account(mut, seeds=[b"vault",market.key().as_ref()], bump)] pub vault: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token2022>,
    pub system_program: Program<'info, System>,
}
#[derive(Accounts)]
pub struct PreparePosition<'info> {
    #[account(mut)] pub payer: Signer<'info>,
    /// CHECK: The beneficiary is an address and need not sign a permissionless preparation.
    pub follower: UncheckedAccount<'info>,
    #[account(seeds=[b"market",market.mint.as_ref()],bump=market.bump)] pub market: Account<'info,Market>,
    #[account(mut,has_one=market)] pub call: Account<'info,Call>,
    #[account(init_if_needed,payer=payer,space=8+CallPosition::INIT_SPACE,seeds=[b"position",call.key().as_ref(),follower.key().as_ref()],bump)] pub position: Account<'info,CallPosition>,
    pub system_program: Program<'info,System>,
}
#[derive(Accounts)]
pub struct Trade<'info> {
    #[account(mut)] pub trader: Signer<'info>,
    #[account(mut,seeds=[b"market",mint.key().as_ref()],bump=market.bump,has_one=mint)] pub market: Account<'info,Market>,
    #[account(mut)] pub mint: InterfaceAccount<'info,Mint>,
    #[account(mut,seeds=[b"vault",market.key().as_ref()],bump=market.vault_bump,token::mint=mint,token::authority=market,token::token_program=token_program)] pub vault: InterfaceAccount<'info,TokenAccount>,
    #[account(mut,token::mint=mint,token::authority=trader,token::token_program=token_program)] pub trader_token: InterfaceAccount<'info,TokenAccount>,
    #[account(mut,seeds=[b"protocol"],bump=protocol.bump)] pub protocol: Account<'info,Protocol>,
    #[account(mut)] pub position: Option<Account<'info,CallPosition>>,
    #[account(mut,seeds=[b"prices",market.key().as_ref()],bump=price_history.load()?.bump,constraint=price_history.load()?.market==market.key() @ LaunchError::InvalidOracleProof)] pub price_history: AccountLoader<'info,PriceHistory>,
    pub token_program: Program<'info,Token2022>,
    pub system_program: Program<'info,System>,
}
#[derive(Accounts)]
pub struct ClaimCreatorFees<'info> {
    #[account(mut)] pub creator: Signer<'info>,
    #[account(mut,seeds=[b"market",market.mint.as_ref()],bump=market.bump,has_one=creator)] pub market: Account<'info,Market>,
}
#[derive(Accounts)]
#[instruction(id:u64)]
pub struct OpenCall<'info> {
    #[account(mut)] pub caller: Signer<'info>,
    #[account(seeds=[b"market",market.mint.as_ref()],bump=market.bump)] pub market: Account<'info,Market>,
    #[account(init_if_needed,payer=caller,space=8+CallerSequence::INIT_SPACE,seeds=[b"call-sequence",market.key().as_ref(),caller.key().as_ref()],bump)] pub caller_sequence: Account<'info,CallerSequence>,
    #[account(init,payer=caller,space=8+Call::INIT_SPACE,seeds=[b"call",market.key().as_ref(),caller.key().as_ref(),&id.to_le_bytes()],bump)] pub call: Account<'info,Call>,
    pub system_program: Program<'info,System>,
}
#[derive(Accounts)]
pub struct SettleCall<'info> {
    pub settler: Signer<'info>,
    #[account(mut,seeds=[b"market",market.mint.as_ref()],bump=market.bump)] pub market: Account<'info,Market>,
    #[account(mut,has_one=market,has_one=caller)] pub call: Account<'info,Call>,
    #[account(seeds=[b"prices",market.key().as_ref()],bump=price_history.load()?.bump,constraint=price_history.load()?.market==market.key() @ LaunchError::InvalidOracleProof)] pub price_history: AccountLoader<'info,PriceHistory>,
    #[account(mut,close=follower,seeds=[b"position",call.key().as_ref(),follower.key().as_ref()],bump=position.bump,has_one=call,has_one=market,has_one=follower,constraint=position.rent_payer==follower.key())] pub position: Account<'info,CallPosition>,
    /// CHECK: Exact caller address enforced by call.has_one.
    #[account(mut)] pub caller: UncheckedAccount<'info>,
    /// CHECK: Exact follower address enforced by position.has_one.
    #[account(mut)] pub follower: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct CheckpointCall<'info> {
    pub settler: Signer<'info>,
    #[account(seeds=[b"market",market.mint.as_ref()],bump=market.bump)] pub market: Account<'info,Market>,
    #[account(mut,has_one=market)] pub call: Account<'info,Call>,
    #[account(seeds=[b"prices",market.key().as_ref()],bump=price_history.load()?.bump,constraint=price_history.load()?.market==market.key() @ LaunchError::InvalidOracleProof)] pub price_history: AccountLoader<'info,PriceHistory>,
}
#[derive(Accounts)]
pub struct CloseCall<'info> {
    #[account(mut)] pub caller: Signer<'info>,
    #[account(mut,close=caller,has_one=caller)] pub call: Account<'info,Call>,
}
#[derive(Accounts)]
pub struct ClosePosition<'info> {
    #[account(mut)] pub follower: Signer<'info>,
    #[account(mut)] pub call: Account<'info,Call>,
    #[account(mut,close=follower,seeds=[b"position",call.key().as_ref(),follower.key().as_ref()],bump=position.bump,has_one=call,has_one=follower,constraint=position.rent_payer==follower.key(),constraint=position.market==call.market)] pub position: Account<'info,CallPosition>,
}
#[derive(Accounts)]
pub struct ClaimCallPayout<'info> {
    pub settler: Signer<'info>,
    #[account(mut,seeds=[b"market",market.mint.as_ref()],bump=market.bump)] pub market: Account<'info,Market>,
    #[account(mut,has_one=market,has_one=caller)] pub call: Account<'info,Call>,
    /// CHECK: The beneficiary is bound to the call; only its existing balance and rent floor are read.
    #[account(mut)] pub caller: UncheckedAccount<'info>,
}
