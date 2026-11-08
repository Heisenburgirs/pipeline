//! Token-2022 transfer hook used in tests.
//!
//! `initialize(allowed)` creates the mint's extra-account-meta list. With an empty
//! `allowed` list the hook approves every transfer. Otherwise it also creates a
//! whitelist account (listed as the hook's one extra account) and only approves
//! transfers whose source or destination token account is on it, like a whitelist-gated
//! token's hook.

use solana_program::{
    account_info::AccountInfo,
    entrypoint,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};

/// `sha256("spl-transfer-hook-interface:execute")[..8]`
const EXECUTE: [u8; 8] = [105, 37, 101, 197, 75, 251, 102, 26];
/// This program's own setup instruction.
const INITIALIZE: [u8; 8] = *b"tsthkini";

entrypoint!(process_instruction);

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    match data.get(..8) {
        Some(d) if d == EXECUTE => execute(accounts),
        Some(d) if d == INITIALIZE => initialize(program_id, accounts, &data[8..]),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

/// Accounts: source, mint, destination, authority, validation, [whitelist].
fn execute(accounts: &[AccountInfo]) -> ProgramResult {
    let source = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let destination = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let Some(whitelist) = accounts.get(5) else {
        return Ok(());
    };
    let data = whitelist.try_borrow_data()?;
    let allowed = data
        .chunks_exact(32)
        .any(|key| key == source.key.as_ref() || key == destination.key.as_ref());
    if allowed {
        Ok(())
    } else {
        msg!("transfer rejected: neither source nor destination is whitelisted");
        Err(ProgramError::Custom(0x7e57))
    }
}

/// Accounts: payer (signer), validation PDA, whitelist PDA, mint, system program.
/// Data: N × 32-byte token account addresses allowed by the whitelist.
fn initialize(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let [payer, validation, whitelist, mint, system_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let (validation_key, validation_bump) =
        Pubkey::find_program_address(&[b"extra-account-metas", mint.key.as_ref()], program_id);
    let (whitelist_key, whitelist_bump) =
        Pubkey::find_program_address(&[b"whitelist", mint.key.as_ref()], program_id);
    if *validation.key != validation_key || *whitelist.key != whitelist_key {
        return Err(ProgramError::InvalidSeeds);
    }

    // TLV entry for the Execute instruction: discriminator, length, then a PodSlice of
    // 35-byte ExtraAccountMeta entries (type 0 = fixed address).
    let extra_count: u32 = if data.is_empty() { 0 } else { 1 };
    let mut tlv = EXECUTE.to_vec();
    tlv.extend_from_slice(&(4 + 35 * extra_count).to_le_bytes());
    tlv.extend_from_slice(&extra_count.to_le_bytes());
    if extra_count == 1 {
        tlv.push(0);
        tlv.extend_from_slice(whitelist_key.as_ref());
        tlv.push(0); // not a signer
        tlv.push(0); // not writable
    }

    create_pda(
        payer,
        validation,
        system_program,
        program_id,
        tlv.len(),
        &[
            b"extra-account-metas",
            mint.key.as_ref(),
            &[validation_bump],
        ],
    )?;
    validation.try_borrow_mut_data()?.copy_from_slice(&tlv);
    if extra_count == 1 {
        create_pda(
            payer,
            whitelist,
            system_program,
            program_id,
            data.len(),
            &[b"whitelist", mint.key.as_ref(), &[whitelist_bump]],
        )?;
        whitelist.try_borrow_mut_data()?.copy_from_slice(data);
    }
    Ok(())
}

fn create_pda<'a>(
    payer: &AccountInfo<'a>,
    account: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    owner: &Pubkey,
    space: usize,
    seeds: &[&[u8]],
) -> ProgramResult {
    let lamports = Rent::get()?.minimum_balance(space);
    // System program CreateAccount: u32 tag 0, lamports, space, owner.
    let mut data = 0u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    data.extend_from_slice(&(space as u64).to_le_bytes());
    data.extend_from_slice(owner.as_ref());
    let create_account = Instruction {
        program_id: *system_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new(*account.key, true),
        ],
        data,
    };
    invoke_signed(
        &create_account,
        &[payer.clone(), account.clone(), system_program.clone()],
        &[seeds],
    )
}
