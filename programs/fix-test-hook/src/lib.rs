//! TEST ONLY. A hook for the AMM's LiteSVM suite and the local chain. It keeps one account per venue,
//! `["fix-hook", venue]`, counts the calls it gets, keeps the last one, and refuses by its mode: a call whose
//! `amount_a` passes `limit`, or one from `blocked`. Its other modes try what a hook must not be able to do: move
//! the actor's lamports, or call back into the AMM. Never deployed to mainnet.
#![allow(unexpected_cfgs)]
use solana_program::{account_info::AccountInfo, entrypoint::ProgramResult, instruction::Instruction, program::{invoke, invoke_signed}, program_error::ProgramError, pubkey::Pubkey, rent::Rent, system_instruction, sysvar::Sysvar};

pub const STATE_SEED: &[u8] = b"fix-hook";
/// `sha256("global:on_fix_action")[..8]`, the AMM's call tag.
pub const CALL_TAG: [u8; 8] = [171, 39, 133, 218, 105, 247, 235, 179];
/// The AMM's `HookCall`: venue, actor, action, slot, two amounts and four reserves.
pub const CALL_LEN: usize = 32 + 32 + 1 + 8 * 7;
/// mode, limit, blocked, calls, the last call.
pub const STATE_LEN: usize = 1 + 8 + 32 + 8 + CALL_LEN;
pub const MODE_RECORD: u8 = 0;
pub const MODE_TAKE_LAMPORTS: u8 = 1;
pub const MODE_REENTER: u8 = 2;
/// The custom error a refusal returns.
pub const REFUSED: u32 = 7_001;

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process);

/// `[0, venue, mode, limit, blocked]` with payer, state and the system program creates the venue's account;
/// `[1, mode, limit, blocked]` with the state changes it; the AMM's call is the tag and its `HookCall`.
pub fn process(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    if let Some(call) = data.strip_prefix(&CALL_TAG) { return on_call(program_id, accounts, call); }
    match data.split_first() {
        Some((0, rest)) if rest.len() == 32 + 1 + 8 + 32 => {
            let [payer, state, system] = accounts else { return Err(ProgramError::NotEnoughAccountKeys) };
            let venue = &rest[..32];
            let (address, bump) = Pubkey::find_program_address(&[STATE_SEED, venue], program_id);
            if *state.key != address { return Err(ProgramError::InvalidSeeds); }
            let rent = Rent::get()?.minimum_balance(STATE_LEN);
            invoke_signed(&system_instruction::create_account(payer.key, state.key, rent, STATE_LEN as u64, program_id), &[payer.clone(), state.clone(), system.clone()], &[&[STATE_SEED, venue, &[bump]]])?;
            state.try_borrow_mut_data()?[..41].copy_from_slice(&rest[32..]);
            Ok(())
        }
        Some((1, rest)) if rest.len() == 1 + 8 + 32 => {
            let [state] = accounts else { return Err(ProgramError::NotEnoughAccountKeys) };
            if state.owner != program_id { return Err(ProgramError::IllegalOwner); }
            state.try_borrow_mut_data()?[..41].copy_from_slice(rest);
            Ok(())
        }
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn on_call(program_id: &Pubkey, accounts: &[AccountInfo], call: &[u8]) -> ProgramResult {
    let [state, venue, actor, rest @ ..] = accounts else { return Err(ProgramError::NotEnoughAccountKeys) };
    if state.owner != program_id || call.len() != CALL_LEN { return Err(ProgramError::InvalidArgument); }
    if Pubkey::find_program_address(&[STATE_SEED, venue.key.as_ref()], program_id).0 != *state.key { return Err(ProgramError::InvalidSeeds); }
    let mut data = state.try_borrow_mut_data()?;
    let (mode, limit) = (data[0], u64::from_le_bytes(data[1..9].try_into().unwrap()));
    let blocked = Pubkey::new_from_array(data[9..41].try_into().unwrap());
    let calls = u64::from_le_bytes(data[41..49].try_into().unwrap()) + 1;
    data[41..49].copy_from_slice(&calls.to_le_bytes());
    data[49..49 + CALL_LEN].copy_from_slice(call);
    drop(data);
    let amount_a = u64::from_le_bytes(call[73..81].try_into().unwrap());
    match mode {
        MODE_TAKE_LAMPORTS => invoke(&system_instruction::transfer(actor.key, state.key, 1), &[actor.clone(), state.clone()]),
        // Calls the venue's owner, the AMM, back: the runtime refuses it.
        MODE_REENTER => invoke(&Instruction { program_id: *venue.owner, accounts: vec![], data: vec![0; 8] }, rest),
        _ if (limit > 0 && amount_a > limit) || *actor.key == blocked => Err(ProgramError::Custom(REFUSED)),
        _ => Ok(()),
    }
}
