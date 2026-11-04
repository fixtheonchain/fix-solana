use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use crate::*;

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(mut)] pub admin: Signer<'info>,
    #[account(constraint=program.programdata_address()? == Some(program_data.key()) @ AmmError::Unauthorized)]
    pub program: Program<'info, crate::program::FixAmm>,
    #[account(constraint=program_data.upgrade_authority_address == Some(admin.key()) @ AmmError::Unauthorized)]
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
    #[account(mut, seeds=[b"config"], bump=config.bump, has_one=admin @ AmmError::Unauthorized)] pub config: Account<'info, Config>,
}
#[derive(Accounts)]
pub struct AcceptAdmin<'info> {
    pub pending_admin: Signer<'info>,
    #[account(mut, seeds=[b"config"], bump=config.bump, has_one=pending_admin @ AmmError::Unauthorized)] pub config: Account<'info, Config>,
}
#[event_cpi]
#[derive(Accounts)]
pub struct CreatePool<'info> {
    #[account(mut)] pub creator: Signer<'info>,
    #[account(seeds=[b"config"], bump=config.bump)] pub config: Account<'info, Config>,
    #[account(mint::token_program=token_program)] pub mint: InterfaceAccount<'info, Mint>,
    #[account(init, payer=creator, space=8+Pool::INIT_SPACE, seeds=[b"pool", mint.key().as_ref()], bump)] pub pool: Account<'info, Pool>,
    #[account(init, payer=creator, seeds=[b"vault", pool.key().as_ref()], bump, token::mint=mint, token::authority=pool, token::token_program=token_program)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint=mint, token::authority=creator, token::token_program=token_program)] pub creator_token: InterfaceAccount<'info, TokenAccount>,
    #[account(init, payer=creator, space=8+Position::INIT_SPACE, seeds=[b"position", pool.key().as_ref(), creator.key().as_ref()], bump)]
    pub position: Account<'info, Position>,
    /// CHECK: The launchpad's market address for this mint. A coin the launchpad created has one there and is refused.
    #[account(seeds=[b"market", mint.key().as_ref()], bump, seeds::program=LAUNCHPAD_ID)] pub launchpad_market: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}
#[event_cpi]
#[derive(Accounts)]
pub struct AddLiquidity<'info> {
    #[account(mut)] pub owner: Signer<'info>,
    #[account(mut, seeds=[b"pool", mint.key().as_ref()], bump=pool.bump, has_one=mint, has_one=token_program)] pub pool: Account<'info, Pool>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, seeds=[b"vault", pool.key().as_ref()], bump=pool.vault_bump)] pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint=mint, token::authority=owner, token::token_program=token_program)] pub owner_token: InterfaceAccount<'info, TokenAccount>,
    #[account(init_if_needed, payer=owner, space=8+Position::INIT_SPACE, seeds=[b"position", pool.key().as_ref(), owner.key().as_ref()], bump)]
    pub position: Account<'info, Position>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}
#[event_cpi]
#[derive(Accounts)]
pub struct RemoveLiquidity<'info> {
    #[account(mut)] pub owner: Signer<'info>,
    #[account(mut, seeds=[b"pool", mint.key().as_ref()], bump=pool.bump, has_one=mint, has_one=token_program)] pub pool: Account<'info, Pool>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, seeds=[b"vault", pool.key().as_ref()], bump=pool.vault_bump)] pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint=mint, token::authority=owner, token::token_program=token_program)] pub owner_token: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, seeds=[b"position", pool.key().as_ref(), owner.key().as_ref()], bump=position.bump)] pub position: Account<'info, Position>,
    pub token_program: Interface<'info, TokenInterface>,
}
#[event_cpi]
#[derive(Accounts)]
pub struct Swap<'info> {
    #[account(mut)] pub trader: Signer<'info>,
    #[account(mut, seeds=[b"pool", mint.key().as_ref()], bump=pool.bump, has_one=mint, has_one=token_program)] pub pool: Account<'info, Pool>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, seeds=[b"vault", pool.key().as_ref()], bump=pool.vault_bump)] pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint=mint, token::authority=trader, token::token_program=token_program)] pub trader_token: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, seeds=[b"protocol", protocol.shard.to_le_bytes().as_ref()], bump=protocol.bump)] pub protocol: Account<'info, Protocol>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}
