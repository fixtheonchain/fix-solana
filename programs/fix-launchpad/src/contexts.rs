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
    pub system_program: Program<'info, System>,
}
#[derive(Accounts)]
#[instruction(shard: u8)]
pub struct CreateProtocolShard<'info> {
    #[account(mut)] pub payer: Signer<'info>,
    #[account(init, payer=payer, space=8+Protocol::INIT_SPACE, seeds=[b"protocol", shard.to_le_bytes().as_ref()], bump)] pub protocol: Account<'info, Protocol>,
    pub system_program: Program<'info, System>,
}
#[derive(Accounts)]
pub struct AdminConfig<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds=[b"config"], bump=config.bump, has_one=admin)] pub config: Account<'info, Config>,
}
#[derive(Accounts)]
#[instruction(program_id: Pubkey)]
pub struct RegisterExtension<'info> {
    #[account(mut)] pub admin: Signer<'info>,
    #[account(seeds=[b"config"], bump=config.bump, has_one=admin @ LaunchError::Unauthorized)] pub config: Account<'info, Config>,
    #[account(init, payer=admin, space=8+Extension::INIT_SPACE, seeds=[b"extension", program_id.as_ref()], bump)] pub extension: Account<'info, Extension>,
    pub system_program: Program<'info, System>,
}
#[derive(Accounts)]
pub struct AdminExtension<'info> {
    pub admin: Signer<'info>,
    #[account(seeds=[b"config"], bump=config.bump, has_one=admin @ LaunchError::Unauthorized)] pub config: Account<'info, Config>,
    #[account(mut, seeds=[b"extension", extension.program.as_ref()], bump=extension.bump)] pub extension: Account<'info, Extension>,
}
#[derive(Accounts)]
pub struct RemoveExtension<'info> {
    #[account(mut)] pub admin: Signer<'info>,
    #[account(seeds=[b"config"], bump=config.bump, has_one=admin @ LaunchError::Unauthorized)] pub config: Account<'info, Config>,
    #[account(mut, close=admin, seeds=[b"extension", extension.program.as_ref()], bump=extension.bump)] pub extension: Account<'info, Extension>,
}
#[derive(Accounts)]
pub struct AcceptAdmin<'info> {
    pub pending_admin: Signer<'info>,
    #[account(mut, seeds=[b"config"], bump=config.bump, has_one=pending_admin)] pub config: Account<'info, Config>,
}
#[event_cpi]
#[derive(Accounts)]
pub struct CreateCoin<'info> {
    #[account(mut)] pub creator: Signer<'info>,
    #[account(seeds=[b"config"], bump=config.bump)] pub config: Account<'info, Config>,
    /// CHECK: A new signer account allocated and initialized as Token-2022 in the handler.
    #[account(mut)] pub mint: Signer<'info>,
    #[account(init, payer=creator, space=8+Market::INIT_SPACE, seeds=[b"market",mint.key().as_ref()], bump)] pub market: Account<'info, Market>,
    /// CHECK: The vault PDA is allocated and initialized by the handler.
    #[account(mut, seeds=[b"vault",market.key().as_ref()], bump)] pub vault: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token2022>,
    pub system_program: Program<'info, System>,
}
#[event_cpi]
#[derive(Accounts)]
pub struct Trade<'info> {
    #[account(mut)] pub trader: Signer<'info>,
    #[account(mut,seeds=[b"market",mint.key().as_ref()],bump=market.bump,has_one=mint)] pub market: Account<'info,Market>,
    #[account(mut)] pub mint: InterfaceAccount<'info,Mint>,
    #[account(mut,seeds=[b"vault",market.key().as_ref()],bump=market.vault_bump,token::mint=mint,token::authority=market,token::token_program=token_program)] pub vault: InterfaceAccount<'info,TokenAccount>,
    #[account(mut,token::mint=mint,token::authority=trader,token::token_program=token_program)] pub trader_token: InterfaceAccount<'info,TokenAccount>,
    #[account(mut,seeds=[b"protocol",protocol.shard.to_le_bytes().as_ref()],bump=protocol.bump)] pub protocol: Account<'info,Protocol>,
    pub token_program: Program<'info,Token2022>,
    pub system_program: Program<'info,System>,
}
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimCreatorFees<'info> {
    #[account(mut)] pub creator: Signer<'info>,
    #[account(mut,seeds=[b"market",market.mint.as_ref()],bump=market.bump,has_one=creator)] pub market: Account<'info,Market>,
}
