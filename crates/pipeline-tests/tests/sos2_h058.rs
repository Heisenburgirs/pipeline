//! SOS audit #2, H058: `MarketInfo` structural validation gaps.
//!
//! `fetch_market_info` checks only the count (2-4 mints, one vault per mint) and that the
//! mints are distinct. It does not check that a declared "mint" is a mint, or that a
//! vault is a token account of the matching mint. This file shows what a hostile strategy
//! can do with a token account declared as a mint:
//! - registration succeeds, so the bad declaration is recorded;
//! - every swap that touches the bogus mint is refused by the router, before any token
//!   moves, because no trader account can carry that mint;
//! - a healthy market in the same world still swaps and pays Pipeline's fee.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Pubkey};
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
/// The evil strategy's market state length (`STATE_LEN`).
const STATE_LEN: usize = 32 * 4 + 2;
const RESERVE: u64 = 1_000_000_000;
const AMOUNT: u64 = 1_000;

/// Writes an evil-owned state whose `mint_a` is `mint_a`, with `info_mode` 0 (two mints,
/// two vaults). It stands in for a strategy that registered a bogus `MarketInfo`.
fn plant_evil_state(
    env: &mut Env,
    mint_a: Pubkey,
    mint_b: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
) -> Pubkey {
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let mut data = Vec::with_capacity(STATE_LEN);
    for key in [mint_a, mint_b, vault_a, vault_b] {
        data.extend_from_slice(key.as_ref());
    }
    data.push(255);
    data.push(0);
    let lamports = env.svm.minimum_balance_for_rent_exemption(STATE_LEN);
    env.svm
        .set_account(
            state,
            solana_account::Account {
                lamports,
                data,
                owner: EVIL_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    state
}

#[test]
fn h058_token_account_declared_as_mint_registers_but_cannot_swap_and_neighbour_is_unaffected() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let healthy = setup_cp_market(&mut env, TOKEN, TOKEN, RESERVE, RESERVE, 30);

    // A real token account whose address the bogus market declares as its mint.
    let other_mint = env.create_mint(&TOKEN, 6, None, None);
    let bogus_mint = env.create_ata(&Pubkey::new_unique(), &other_mint, &TOKEN);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state_owner = Pubkey::new_unique();
    let vault_a = env.create_ata(&state_owner, &other_mint, &TOKEN);
    let vault_b = env.create_ata(&state_owner, &mint_b, &TOKEN);
    let state = plant_evil_state(&mut env, bogus_mint, mint_b, vault_a, vault_b);
    env.mint_to(&mint_b, &TOKEN, &vault_b, RESERVE);
    let market = pda::market(&EVIL_ID, &state);

    // Registration accepts it: two distinct mints and two vaults pass the structural checks.
    let payer = env.funded_keypair();
    let ix = client::register_existing_market(&payer.pubkey(), &EVIL_ID, &state, vec![]);
    env.send(&[ix], &[&payer])
        .expect("H058: the bogus declaration is accepted");
    let recorded = env.market(&market);
    assert_eq!(recorded.mint_count, 2);
    assert_eq!(recorded.mints[0], bogus_mint);

    // A swap that uses the bogus mint as input is refused by the router: no trader
    // account can have mint == bogus_mint, so the trader-input check fails first.
    let (trader, trader_healthy_a) = env.trader_with(&healthy.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let trader_bogus = env.create_ata(&trader.pubkey(), &other_mint, &TOKEN);
    env.mint_to(&other_mint, &TOKEN, &trader_bogus, 10_000_000);
    let vault_a_before = env.token_balance(&vault_a);
    let vault_b_before = env.token_balance(&vault_b);
    let bogus_swap = client::SwapAccounts {
        trader: trader.pubkey(),
        strategy_program: EVIL_ID,
        market_state: state,
        trader_input: trader_bogus,
        trader_output: trader_b,
        input_vault: vault_a,
        input_mint: bogus_mint,
        output_mint: mint_b,
        input_token_program: TOKEN,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts: vec![AccountMeta::new(vault_b, false)],
        input_hook_accounts: vec![],
    };
    let failed = env
        .send(
            &[client::swap(&bogus_swap, &exact_in(AMOUNT, 0))],
            &[&trader],
        )
        .expect_err("H058: a swap with a non-mint as input must not succeed");
    let logs = failed.meta.logs.join("\n");
    assert!(
        logs.contains("AnchorError"),
        "H058: the router refused the swap, not the strategy:\n{logs}"
    );
    assert_eq!(
        env.token_balance(&vault_a),
        vault_a_before,
        "no input moved"
    );
    assert_eq!(
        env.token_balance(&vault_b),
        vault_b_before,
        "no output moved"
    );

    // The healthy market in the same world still swaps and pays Pipeline's fee.
    let trader_healthy_b = env.create_ata(&trader.pubkey(), &healthy.mint_b, &TOKEN);
    let healthy_fee = pda::fee_account(&healthy.market, &healthy.mint_a, &TOKEN);
    env.send(
        &[client::init_fee_account(
            &trader.pubkey(),
            &healthy.market,
            &healthy.mint_a,
            &TOKEN,
        )],
        &[&trader],
    )
    .expect("init the healthy market's fee account");
    let fee_before = env.token_balance(&healthy_fee);
    let healthy_accounts = cp_swap_accounts(
        &healthy,
        &trader.pubkey(),
        &healthy.mint_a,
        &trader_healthy_a,
        &healthy.mint_b,
        &trader_healthy_b,
    );
    env.send(
        &[client::swap(&healthy_accounts, &exact_in(AMOUNT, 0))],
        &[&trader],
    )
    .expect("H058: the neighbouring healthy market still swaps");
    assert!(env.token_balance(&trader_healthy_b) > 0);
    let fee_after = env.token_balance(&healthy_fee);
    assert!(
        fee_after > fee_before,
        "H058: Pipeline's fee still lands on the healthy market"
    );
}
