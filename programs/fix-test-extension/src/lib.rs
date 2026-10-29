//! TEST ONLY. A minimal extension for the core's LiteSVM suite: it forwards one core trade by CPI and
//! signs with its `["fix-extension"]` PDA. It holds no state, takes no fee itself and is never deployed.
#![allow(unexpected_cfgs)]
use solana_program::{account_info::AccountInfo, entrypoint::ProgramResult, instruction::{AccountMeta, Instruction}, program::{invoke, invoke_signed}, program_error::ProgramError, pubkey::Pubkey};

pub const AUTHORITY_SEED: &[u8] = b"fix-extension";

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process);

/// Data: one mode byte, then the core instruction data. Mode 0 signs as the authority; mode 1 passes the
/// authority unsigned, which the core must refuse. Accounts: the core program, then the core instruction's
/// accounts in order (for a routed trade ending with registry entry, authority and recipient).
pub fn process(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let (mode, core_data) = data.split_first().ok_or(ProgramError::InvalidInstructionData)?;
    let (core, forwarded) = accounts.split_first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let (authority, bump) = Pubkey::find_program_address(&[AUTHORITY_SEED], program_id);
    let sign = *mode == 0;
    let metas = forwarded.iter().map(|account| AccountMeta {
        pubkey: *account.key,
        is_signer: account.is_signer || (sign && *account.key == authority),
        is_writable: account.is_writable,
    }).collect();
    let instruction = Instruction { program_id: *core.key, accounts: metas, data: core_data.to_vec() };
    let mut infos = forwarded.to_vec();
    infos.push(core.clone());
    if sign { invoke_signed(&instruction, &infos, &[&[AUTHORITY_SEED, &[bump]]]) } else { invoke(&instruction, &infos) }
}
