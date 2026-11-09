//! Shared fixtures for the BOK `swap` runtime tests (`bok_swap*.rs`).
//!
//! Builds on `common/mod.rs`: an evil-strategy market with configurable vaults, the
//! hostile refill hook from `programs/test-hook-refill`, balance snapshots, and helpers
//! that read the swap's return data and event.

#![allow(dead_code, clippy::result_large_err)]

use crate::common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_events::SwapEvent;
use pipeline_strategy_interface as psi;
use solana_keypair::Keypair;
use solana_signer::Signer;

pub const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
pub const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
pub const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
pub const REFILL_HOOK_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ4");

pub const HONEST: u8 = 0;

/// Byte offset of `SwapArgs::min_amount_out` in the instruction data.
pub const MIN_OUT_OFFSET: usize = 8 + 1 + 8 + 8;
/// Byte offset of `SwapArgs::input_hook_accounts` in the instruction data.
pub const HOOK_COUNT_OFFSET: usize = 8 + 1 + 8 + 8 + 8 + 2;

/// Positions of the router's named `swap` accounts in `client::swap`'s instruction.
pub mod ix {
    pub const TRADER_INPUT: usize = 5;
    pub const TRADER_OUTPUT: usize = 6;
    pub const INPUT_VAULT: usize = 7;
    pub const FEE_ACCOUNT: usize = 13;
    pub const INTEGRATOR: usize = 14;
}

pub fn compute(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction::new_with_bytes(
        client::pubkey!("ComputeBudget111111111111111111111111111111"),
        &data,
        vec![],
    )
}

pub fn balances(env: &Env, accounts: &[Pubkey]) -> Vec<u64> {
    accounts.iter().map(|a| env.token_balance(a)).collect()
}

/// The swap's return data, after checking it came from the router.
pub fn swap_result(meta: &litesvm::types::TransactionMetadata) -> client::SwapResult {
    assert_eq!(meta.return_data.program_id, client::PROGRAM_ID);
    borsh::from_slice(&meta.return_data.data).expect("SwapResult return data")
}

/// The last swap's return data and its single event, after asserting that the four
/// reported amounts are identical in both (INV-SWAP-12, first clause).
pub fn reported(meta: &litesvm::types::TransactionMetadata) -> (client::SwapResult, SwapEvent) {
    let result = swap_result(meta);
    let events = swap_events(meta);
    assert_eq!(events.len(), 1, "one swap event per swap instruction");
    let event = events[0].clone();
    assert_eq!(
        (
            event.net_sent,
            event.received,
            event.pipeline_fee,
            event.integrator_fee
        ),
        (
            result.net_sent,
            result.received,
            result.pipeline_fee,
            result.integrator_fee
        ),
        "event and return data disagree"
    );
    (result, event)
}

/// Token-2022 transfer fee: `ceil(amount × bps / 10,000)`, capped at `max`.
pub fn transfer_fee(amount: u64, fee: Option<(u16, u64)>) -> u64 {
    match fee {
        Some((bps, max)) => psi::fees::ceil_bps(amount, bps as u64).unwrap().min(max),
        None => 0,
    }
}

pub fn exact_out(max_in: u64, amount_out: u64, min_amount_out: u64) -> client::SwapArgs {
    client::SwapArgs {
        mode: psi::TradeMode::ExactOut,
        amount_in: max_in,
        amount_out,
        min_amount_out,
        integrator_fee_bps: 0,
        input_hook_accounts: 0,
        extra: vec![],
    }
}

// ---------------------------------------------------------------------------- evil

pub struct EvilMarket {
    pub state: Pubkey,
    pub market: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub tp_a: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
}

pub fn evil_state(mint_a: &Pubkey, mint_b: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0
}

pub fn ensure_evil(env: &mut Env) {
    if env.svm.get_account(&pda::strategy(&EVIL_ID)).is_none() {
        env.register(&EVIL_ID, "test_strategy_evil");
    }
}

/// An evil market between `mint_a` (in `tp_a`) and an SPL `mint_b`. `vault_a` is
/// whatever address the strategy declares (`None`: the state PDA's ATA, created here);
/// vault B is always the state PDA's ATA. Both fee accounts are created, and
/// `vault_b_amount` of B is minted into vault B.
pub fn evil_market(
    env: &mut Env,
    mint_a: Pubkey,
    tp_a: Pubkey,
    mint_b: Pubkey,
    vault_a: Option<Pubkey>,
    vault_b_amount: u64,
) -> EvilMarket {
    ensure_evil(env);
    let state = evil_state(&mint_a, &mint_b);
    let vault_a = match vault_a {
        Some(address) => address,
        None => env.create_ata(&state, &mint_a, &tp_a),
    };
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    let creator = env.funded_keypair();
    let ix = client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
        strategy_program: EVIL_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![0],
        max_creator_lamports: MAX_SETUP_LAMPORTS,
        strategy_accounts: vec![
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
        ],
    });
    env.send(&[ix], &[&creator]).expect("create evil market");
    let market = pda::market(&EVIL_ID, &state);
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &market, &mint_a, &tp_a),
            client::init_fee_account(&payer, &market, &mint_b, &TOKEN),
        ],
        &[],
    )
    .expect("init fee accounts");
    if vault_b_amount > 0 {
        env.mint_to(&mint_b, &TOKEN, &vault_b, vault_b_amount);
    }
    EvilMarket {
        state,
        market,
        mint_a,
        mint_b,
        tp_a,
        vault_a,
        vault_b,
    }
}

/// A → B swap accounts on an evil market (the output vault is the only strategy account).
pub fn evil_swap_accounts(
    m: &EvilMarket,
    trader: &Pubkey,
    trader_a: &Pubkey,
    trader_b: &Pubkey,
) -> client::SwapAccounts {
    client::SwapAccounts {
        trader: *trader,
        strategy_program: EVIL_ID,
        market_state: m.state,
        trader_input: *trader_a,
        trader_output: *trader_b,
        input_vault: m.vault_a,
        input_mint: m.mint_a,
        output_mint: m.mint_b,
        input_token_program: m.tp_a,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts: vec![AccountMeta::new(m.vault_b, false)],
        input_hook_accounts: vec![],
    }
}

pub fn evil_args(behaviour: u8, amount: u64, integrator_bps: u16) -> client::SwapArgs {
    let mut args = exact_in(amount, 0);
    args.extra = vec![behaviour];
    args.integrator_fee_bps = integrator_bps;
    args
}

// --------------------------------------------------------------------- refill hook

/// A Token-2022 mint whose hook (`test_hook_refill`) empties an SPL `stash_mint` stash
/// into `target_of(mint)` on every transfer of the mint.
pub struct RefillHook {
    pub mint: Pubkey,
    pub stash: Pubkey,
    pub accounts: Vec<AccountMeta>,
}

pub fn refill_hook(
    env: &mut Env,
    stash_mint: &Pubkey,
    target_of: impl FnOnce(&Pubkey) -> Pubkey,
) -> RefillHook {
    if env.svm.get_account(&REFILL_HOOK_ID).is_none() {
        env.svm
            .add_program_from_file(REFILL_HOOK_ID, sbf_dir().join("test_hook_refill.so"))
            .unwrap();
    }
    let mint = env.create_mint(&TOKEN_2022, 6, None, Some(REFILL_HOOK_ID));
    let target = target_of(&mint);
    let validation =
        Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], &REFILL_HOOK_ID).0;
    let stash_authority = Pubkey::find_program_address(&[b"stash"], &REFILL_HOOK_ID).0;
    let stash = env.create_ata(&stash_authority, stash_mint, &TOKEN);
    let mut init_data = b"refilini".to_vec();
    for (key, writable) in [(stash, 1u8), (stash_authority, 0), (target, 1), (TOKEN, 0)] {
        init_data.extend_from_slice(key.as_ref());
        init_data.push(writable);
    }
    let init = Instruction::new_with_bytes(
        REFILL_HOOK_ID,
        &init_data,
        vec![
            AccountMeta::new(env.admin.pubkey(), true),
            AccountMeta::new(validation, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    );
    env.send(&[init], &[]).expect("refill hook init");
    RefillHook {
        mint,
        stash,
        accounts: vec![
            AccountMeta::new_readonly(REFILL_HOOK_ID, false),
            AccountMeta::new_readonly(validation, false),
            AccountMeta::new(stash, false),
            AccountMeta::new_readonly(stash_authority, false),
            AccountMeta::new(target, false),
            AccountMeta::new_readonly(TOKEN, false),
        ],
    }
}

/// Sends with a generous compute limit and returns the result.
pub fn send_swap(env: &mut Env, ix: Instruction, trader: &Keypair) -> TxResult {
    env.send(&[compute(1_000_000), ix], &[trader])
}
