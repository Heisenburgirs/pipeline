//! The instructions a wallet's transaction needs around a swap: creating token
//! accounts, wrapping native SOL before and unwrapping it after.

use crate::{
    init_fee_account, pda, swap, AccountMeta, Instruction, Pubkey, SwapAccounts, SwapArgs,
    ASSOCIATED_TOKEN_PROGRAM_ID, SYSTEM_PROGRAM_ID, TOKEN_PROGRAM_ID,
};
use pipeline_strategy_interface::fees;

/// Wrapped SOL (SPL Token).
pub const NATIVE_MINT: Pubkey = crate::pubkey!("So11111111111111111111111111111111111111112");

/// Whether `mint` under `token_program` is wrapped SOL.
pub fn is_native(mint: &Pubkey, token_program: &Pubkey) -> bool {
    *mint == NATIVE_MINT && *token_program == TOKEN_PROGRAM_ID
}

/// Associated Token Account program `CreateIdempotent`.
pub fn create_associated_token_idempotent(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction::new_with_bytes(
        ASSOCIATED_TOKEN_PROGRAM_ID,
        &[1],
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(pda::associated_token(owner, mint, token_program), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
    )
}

/// System program `Transfer`.
pub fn system_transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction::new_with_bytes(
        SYSTEM_PROGRAM_ID,
        &data,
        vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
    )
}

/// SPL Token `SyncNative`: credits lamports sent to a wrapped-SOL account as tokens.
pub fn sync_native(account: &Pubkey) -> Instruction {
    Instruction::new_with_bytes(
        TOKEN_PROGRAM_ID,
        &[17],
        vec![AccountMeta::new(*account, false)],
    )
}

/// SPL Token `CloseAccount`. For wrapped SOL this returns the whole balance as SOL.
pub fn close_token_account(
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction::new_with_bytes(
        *token_program,
        &[9],
        vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*owner, true),
        ],
    )
}

/// Everything a trader's transaction needs for one swap, in order:
///
/// 1. create the trader's output token account if it doesn't exist;
/// 2. if `wrap_sol` and the input is wrapped SOL: create the trader's wrapped-SOL account
///    if needed and wrap enough SOL for the most the swap can spend, including fees;
/// 3. create the market's fee account for the input mint if it doesn't exist;
/// 4. the swap;
/// 5. if `wrap_sol`: close the trader's wrapped-SOL input and/or output account, so any
///    unspent input and all SOL output arrive as native SOL.
///
/// The trader pays for everything and is the only signer. With `wrap_sol`, any wrapped
/// SOL the trader already held in that account is unwrapped too.
pub fn swap_with_setup(
    accounts: &SwapAccounts,
    args: &SwapArgs,
    wrap_sol: bool,
) -> Vec<Instruction> {
    let trader = accounts.trader;
    let market = pda::market(&accounts.strategy_program, &accounts.market_state);
    let wrap_input = wrap_sol && is_native(&accounts.input_mint, &accounts.input_token_program);
    let unwrap_output =
        wrap_sol && is_native(&accounts.output_mint, &accounts.output_token_program);

    let mut instructions = vec![create_associated_token_idempotent(
        &trader,
        &trader,
        &accounts.output_mint,
        &accounts.output_token_program,
    )];
    if wrap_input {
        let most = fees::max_total_spend(args.amount_in, args.integrator_fee_bps)
            .expect("fees on a u64 amount fit in a u64");
        instructions.push(create_associated_token_idempotent(
            &trader,
            &trader,
            &NATIVE_MINT,
            &TOKEN_PROGRAM_ID,
        ));
        instructions.push(system_transfer(&trader, &accounts.trader_input, most));
        instructions.push(sync_native(&accounts.trader_input));
    }
    instructions.push(init_fee_account(
        &trader,
        &market,
        &accounts.input_mint,
        &accounts.input_token_program,
    ));
    instructions.push(swap(accounts, args));
    if wrap_input {
        instructions.push(close_token_account(
            &accounts.trader_input,
            &trader,
            &trader,
            &TOKEN_PROGRAM_ID,
        ));
    }
    if unwrap_output {
        instructions.push(close_token_account(
            &accounts.trader_output,
            &trader,
            &trader,
            &TOKEN_PROGRAM_ID,
        ));
    }
    instructions
}
