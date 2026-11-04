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

// Token pairs. Accounts are boxed: two mints, two vaults and two token accounts would crowd the SBF stack frame.
#[event_cpi]
#[derive(Accounts)]
pub struct CreatePair<'info> {
    #[account(mut)] pub creator: Signer<'info>,
    #[account(seeds=[b"config"], bump=config.bump)] pub config: Box<Account<'info, Config>>,
    #[account(mint::token_program=token_program_a)] pub mint_a: Box<InterfaceAccount<'info, Mint>>,
    #[account(mint::token_program=token_program_b, constraint = mint_a.key() < mint_b.key() @ AmmError::PairOrder)] pub mint_b: Box<InterfaceAccount<'info, Mint>>,
    #[account(init, payer=creator, space=8+Pair::INIT_SPACE, seeds=[b"pair", mint_a.key().as_ref(), mint_b.key().as_ref()], bump)] pub pair: Box<Account<'info, Pair>>,
    #[account(init, payer=creator, seeds=[b"vault", pair.key().as_ref(), mint_a.key().as_ref()], bump, token::mint=mint_a, token::authority=pair, token::token_program=token_program_a)]
    pub vault_a: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init, payer=creator, seeds=[b"vault", pair.key().as_ref(), mint_b.key().as_ref()], bump, token::mint=mint_b, token::authority=pair, token::token_program=token_program_b)]
    pub vault_b: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint=mint_a, token::authority=creator, token::token_program=token_program_a)] pub creator_token_a: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint=mint_b, token::authority=creator, token::token_program=token_program_b)] pub creator_token_b: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init, payer=creator, space=8+Position::INIT_SPACE, seeds=[b"position", pair.key().as_ref(), creator.key().as_ref()], bump)]
    pub position: Box<Account<'info, Position>>,
    /// CHECK: The launchpad's market address for mint A. A coin the launchpad created has one there and is refused.
    #[account(seeds=[b"market", mint_a.key().as_ref()], bump, seeds::program=LAUNCHPAD_ID)] pub launchpad_market_a: UncheckedAccount<'info>,
    /// CHECK: The launchpad's market address for mint B.
    #[account(seeds=[b"market", mint_b.key().as_ref()], bump, seeds::program=LAUNCHPAD_ID)] pub launchpad_market_b: UncheckedAccount<'info>,
    pub token_program_a: Interface<'info, TokenInterface>,
    pub token_program_b: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}
#[event_cpi]
#[derive(Accounts)]
pub struct AddPairLiquidity<'info> {
    #[account(mut)] pub owner: Signer<'info>,
    #[account(mut, seeds=[b"pair", mint_a.key().as_ref(), mint_b.key().as_ref()], bump=pair.bump, has_one=mint_a, has_one=mint_b, has_one=token_program_a, has_one=token_program_b)] pub pair: Box<Account<'info, Pair>>,
    pub mint_a: Box<InterfaceAccount<'info, Mint>>,
    pub mint_b: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, seeds=[b"vault", pair.key().as_ref(), mint_a.key().as_ref()], bump=pair.vault_a_bump)] pub vault_a: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds=[b"vault", pair.key().as_ref(), mint_b.key().as_ref()], bump=pair.vault_b_bump)] pub vault_b: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint=mint_a, token::authority=owner, token::token_program=token_program_a)] pub owner_token_a: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint=mint_b, token::authority=owner, token::token_program=token_program_b)] pub owner_token_b: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(init_if_needed, payer=owner, space=8+Position::INIT_SPACE, seeds=[b"position", pair.key().as_ref(), owner.key().as_ref()], bump)]
    pub position: Box<Account<'info, Position>>,
    pub token_program_a: Interface<'info, TokenInterface>,
    pub token_program_b: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}
#[event_cpi]
#[derive(Accounts)]
pub struct RemovePairLiquidity<'info> {
    #[account(mut)] pub owner: Signer<'info>,
    #[account(mut, seeds=[b"pair", mint_a.key().as_ref(), mint_b.key().as_ref()], bump=pair.bump, has_one=mint_a, has_one=mint_b, has_one=token_program_a, has_one=token_program_b)] pub pair: Box<Account<'info, Pair>>,
    pub mint_a: Box<InterfaceAccount<'info, Mint>>,
    pub mint_b: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, seeds=[b"vault", pair.key().as_ref(), mint_a.key().as_ref()], bump=pair.vault_a_bump)] pub vault_a: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds=[b"vault", pair.key().as_ref(), mint_b.key().as_ref()], bump=pair.vault_b_bump)] pub vault_b: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint=mint_a, token::authority=owner, token::token_program=token_program_a)] pub owner_token_a: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint=mint_b, token::authority=owner, token::token_program=token_program_b)] pub owner_token_b: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds=[b"position", pair.key().as_ref(), owner.key().as_ref()], bump=position.bump)] pub position: Box<Account<'info, Position>>,
    pub token_program_a: Interface<'info, TokenInterface>,
    pub token_program_b: Interface<'info, TokenInterface>,
}
#[event_cpi]
#[derive(Accounts)]
pub struct PairSwap<'info> {
    pub trader: Signer<'info>,
    #[account(mut, seeds=[b"pair", mint_a.key().as_ref(), mint_b.key().as_ref()], bump=pair.bump, has_one=mint_a, has_one=mint_b, has_one=token_program_a, has_one=token_program_b)] pub pair: Box<Account<'info, Pair>>,
    pub mint_a: Box<InterfaceAccount<'info, Mint>>,
    pub mint_b: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, seeds=[b"vault", pair.key().as_ref(), mint_a.key().as_ref()], bump=pair.vault_a_bump)] pub vault_a: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds=[b"vault", pair.key().as_ref(), mint_b.key().as_ref()], bump=pair.vault_b_bump)] pub vault_b: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint=mint_a, token::authority=trader, token::token_program=token_program_a)] pub trader_token_a: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint=mint_b, token::authority=trader, token::token_program=token_program_b)] pub trader_token_b: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program_a: Interface<'info, TokenInterface>,
    pub token_program_b: Interface<'info, TokenInterface>,
}
