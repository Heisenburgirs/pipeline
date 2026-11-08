//! Minimal token-account reading and a `transfer_checked` that forwards transfer-hook
//! accounts. Works for both SPL Token and Token-2022: the fields used here have the
//! same layout in both, and Token-2022 extensions come after them.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::invoke_signed;

use crate::PipelineError;

pub const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_PROGRAM_ID: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

pub fn is_token_program(key: &Pubkey) -> bool {
    *key == TOKEN_PROGRAM_ID || *key == TOKEN_2022_PROGRAM_ID
}

pub fn associated_token_address(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0
}

/// The associated token program's `CreateIdempotent` (instruction 1).
pub fn create_associated_token_account_idempotent<'info>(
    associated_token_program: &AccountInfo<'info>,
    payer: &AccountInfo<'info>,
    account: &AccountInfo<'info>,
    owner: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
) -> Result<()> {
    let instruction = Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer.key(), true),
            AccountMeta::new(account.key(), false),
            AccountMeta::new_readonly(owner.key(), false),
            AccountMeta::new_readonly(mint.key(), false),
            AccountMeta::new_readonly(system_program.key(), false),
            AccountMeta::new_readonly(token_program.key(), false),
        ],
        data: vec![1],
    };
    invoke_signed(
        &instruction,
        &[
            payer.clone(),
            account.clone(),
            owner.clone(),
            mint.clone(),
            system_program.clone(),
            token_program.clone(),
            associated_token_program.clone(),
        ],
        &[],
    )?;
    Ok(())
}

/// Base token account layout: mint (0..32), owner (32..64), amount (64..72).
const ACCOUNT_LEN: usize = 165;
/// Token-2022 `AccountType::Account`.
const ACCOUNT_TYPE_ACCOUNT: u8 = 2;
/// Base mint layout: decimals at byte 44.
const MINT_DECIMALS_OFFSET: usize = 44;
const MINT_LEN: usize = 82;
const TRANSFER_CHECKED_TAG: u8 = 12;

/// Balance of a token account, after checking it belongs to `mint` and `token_program`.
pub fn checked_balance(
    account: &AccountInfo,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Result<u64> {
    require_keys_eq!(
        *account.owner,
        *token_program,
        PipelineError::BadTokenAccount
    );
    let data = account.try_borrow_data()?;
    require!(data.len() >= ACCOUNT_LEN, PipelineError::BadTokenAccount);
    // Token-2022 accounts with extensions carry an account-type byte after the base
    // layout; require "account" so a mint can never pass as a token account.
    require!(
        data.len() == ACCOUNT_LEN || data[ACCOUNT_LEN] == ACCOUNT_TYPE_ACCOUNT,
        PipelineError::BadTokenAccount
    );
    require!(
        data[..32] == mint.to_bytes(),
        PipelineError::BadTokenAccount
    );
    Ok(u64::from_le_bytes(data[64..72].try_into().unwrap()))
}

/// Balance of a token account already validated by [`checked_balance`].
pub fn balance(account: &AccountInfo) -> Result<u64> {
    let data = account.try_borrow_data()?;
    require!(data.len() >= ACCOUNT_LEN, PipelineError::BadTokenAccount);
    Ok(u64::from_le_bytes(data[64..72].try_into().unwrap()))
}

pub fn decimals(mint: &AccountInfo) -> Result<u8> {
    let data = mint.try_borrow_data()?;
    require!(data.len() >= MINT_LEN, PipelineError::WrongTokenProgram);
    Ok(data[MINT_DECIMALS_OFFSET])
}

/// `transfer_checked` with the given transfer-hook accounts appended, so Token-2022
/// can resolve and invoke the mint's hook. Extra accounts that the hook does not use
/// are ignored by the token program.
#[allow(clippy::too_many_arguments)]
pub fn transfer_checked<'info>(
    token_program: &AccountInfo<'info>,
    from: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    hook_accounts: &[AccountInfo<'info>],
    amount: u64,
    decimals: u8,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    let mut data = Vec::with_capacity(10);
    data.push(TRANSFER_CHECKED_TAG);
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);

    let mut metas = vec![
        AccountMeta::new(from.key(), false),
        AccountMeta::new_readonly(mint.key(), false),
        AccountMeta::new(to.key(), false),
        AccountMeta::new_readonly(authority.key(), true),
    ];
    let mut infos = vec![from.clone(), mint.clone(), to.clone(), authority.clone()];
    for account in hook_accounts {
        metas.push(if account.is_writable {
            AccountMeta::new(account.key(), false)
        } else {
            AccountMeta::new_readonly(account.key(), false)
        });
        infos.push(account.clone());
    }
    infos.push(token_program.clone());

    invoke_signed(
        &Instruction {
            program_id: token_program.key(),
            accounts: metas,
            data,
        },
        &infos,
        signer_seeds,
    )?;
    Ok(())
}
