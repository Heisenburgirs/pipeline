//! Hostile Token-2022 transfer hook used in tests. On every transfer of its mint it
//! empties a stash (an SPL Token account owned by this program's PDA `["stash"]`) into a
//! target token account. With the target set to a trader's input account, it shows a
//! hook can top up that account during a router swap, which the router's fee
//! measurement must not be fooled by.
//!
//! Extra accounts (fixed addresses): stash (writable), stash authority, target
//! (writable), SPL Token program.
use solana_program::{
    account_info::AccountInfo,
    entrypoint,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};

const EXECUTE: [u8; 8] = [105, 37, 101, 197, 75, 251, 102, 26];
const INITIALIZE: [u8; 8] = *b"refilini";

entrypoint!(process_instruction);

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    match data.get(..8) {
        Some(d) if d == EXECUTE => execute(program_id, accounts),
        Some(d) if d == INITIALIZE => initialize(program_id, accounts, &data[8..]),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn execute(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    if accounts.len() < 9 {
        return Ok(());
    }
    let stash = &accounts[5];
    let authority = &accounts[6];
    let target = &accounts[7];
    let token_program = &accounts[8];
    let amount = {
        let data = stash.try_borrow_data()?;
        u64::from_le_bytes(data[64..72].try_into().unwrap())
    };
    if amount == 0 {
        return Ok(());
    }
    let (_, bump) = Pubkey::find_program_address(&[b"stash"], program_id);
    let mut data = vec![3u8];
    data.extend_from_slice(&amount.to_le_bytes());
    invoke_signed(
        &Instruction {
            program_id: *token_program.key,
            accounts: vec![
                AccountMeta::new(*stash.key, false),
                AccountMeta::new(*target.key, false),
                AccountMeta::new_readonly(*authority.key, true),
            ],
            data,
        },
        &[
            stash.clone(),
            target.clone(),
            authority.clone(),
            token_program.clone(),
        ],
        &[&[b"stash", &[bump]]],
    )
}

/// Accounts: payer (signer, w), validation PDA (w), mint, system program.
/// Data: N x (32-byte address, 1-byte writable).
fn initialize(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let [payer, validation, mint, system_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let (key, bump) =
        Pubkey::find_program_address(&[b"extra-account-metas", mint.key.as_ref()], program_id);
    if key != *validation.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let entries: Vec<&[u8]> = data.chunks_exact(33).collect();
    let mut tlv = EXECUTE.to_vec();
    tlv.extend_from_slice(&(4 + 35 * entries.len() as u32).to_le_bytes());
    tlv.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in &entries {
        tlv.push(0);
        tlv.extend_from_slice(&e[..32]);
        tlv.push(0);
        tlv.push(e[32]);
    }
    let lamports = Rent::get()?.minimum_balance(tlv.len());
    let mut create = 0u32.to_le_bytes().to_vec();
    create.extend_from_slice(&lamports.to_le_bytes());
    create.extend_from_slice(&(tlv.len() as u64).to_le_bytes());
    create.extend_from_slice(program_id.as_ref());
    invoke_signed(
        &Instruction {
            program_id: *system_program.key,
            accounts: vec![
                AccountMeta::new(*payer.key, true),
                AccountMeta::new(*validation.key, true),
            ],
            data: create,
        },
        &[payer.clone(), validation.clone(), system_program.clone()],
        &[&[b"extra-account-metas", mint.key.as_ref(), &[bump]]],
    )?;
    validation.try_borrow_mut_data()?.copy_from_slice(&tlv);
    Ok(())
}
