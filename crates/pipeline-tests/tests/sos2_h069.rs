//! SOS audit #2, H069: `init_fee_account` relies on the associated token program's
//! derivation. Asserted here against the real router and the real ATA program:
//! - a fee account address that is not the derived ATA is refused, whatever account
//!   the caller passes (random address, the other token program's ATA, the treasury ATA);
//! - a fee account for the wrong token program is refused by the router, and no account
//!   appears at the other program's ATA address;
//! - a second `init_fee_account` leaves the existing fee account untouched;
//! - `init_fee_account` followed by a swap in the same transaction works.
//!
//! Pre-funded fee-ATA addresses (SPL and Token-2022) are covered by sos2_h036.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;

/// A CP market with both mints legacy SPL, created and funded, with no fee accounts yet.
fn bare_cp_market(env: &mut Env) -> (CpMarket, Pubkey, Pubkey) {
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    if env
        .svm
        .get_account(&pda::strategy(&CP_PROGRAM_ID))
        .is_none()
    {
        env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    }
    let creator = env.funded_keypair();
    let ix = cp_create_market_ix(&creator.pubkey(), &mint_a, &TOKEN, &mint_b, &TOKEN, 30);
    env.send(&[ix], &[&creator]).expect("create_market");
    let pool = cp_pool(&mint_a, &mint_b, 30);
    let market = CpMarket {
        pool,
        market: pda::market(&CP_PROGRAM_ID, &pool),
        mint_a,
        mint_b,
        token_program_a: TOKEN,
        token_program_b: TOKEN,
        vault_a: pda::associated_token(&pool, &mint_a, &TOKEN),
        vault_b: pda::associated_token(&pool, &mint_b, &TOKEN),
    };
    let lp = env.funded_keypair();
    let lp_a = env.create_ata(&lp.pubkey(), &mint_a, &TOKEN);
    let lp_b = env.create_ata(&lp.pubkey(), &mint_b, &TOKEN);
    env.mint_to(&mint_a, &TOKEN, &lp_a, 2_000_000_000);
    env.mint_to(&mint_b, &TOKEN, &lp_b, 4_000_000_000);
    let deposit = cp_deposit_ix(
        &lp.pubkey(),
        &market,
        &lp_a,
        &lp_b,
        1_000_000_000,
        2_000_000_000,
        vec![],
    );
    env.send(&[deposit], &[&lp]).expect("deposit");
    (market, mint_a, mint_b)
}

#[test]
fn fee_account_must_be_the_derived_ata() {
    let mut env = Env::new();
    let (market, mint_a, _) = bare_cp_market(&mut env);
    let payer = env.admin.pubkey();
    let market_key = market.market;
    let derived = pda::fee_account(&market_key, &mint_a, &TOKEN);
    let other_program_ata = pda::fee_account(&market_key, &mint_a, &TOKEN_2022);
    let treasury_ata = pda::associated_token(&env.treasury_owner, &mint_a, &TOKEN);
    let random = Pubkey::new_unique();

    for wrong in [random, other_program_ata, treasury_ata] {
        let mut ix = client::init_fee_account(&payer, &market_key, &mint_a, &TOKEN);
        ix.accounts[4].pubkey = wrong;
        assert!(
            env.send(&[ix], &[]).is_err(),
            "H069: init_fee_account accepted a fee account that is not the derived ATA"
        );
        assert!(
            env.svm.get_account(&wrong).is_none(),
            "H069: a refused init_fee_account still created an account at {wrong}"
        );
    }
    assert!(
        env.svm.get_account(&derived).is_none(),
        "setup: the refused calls created nothing"
    );

    env.send(
        &[client::init_fee_account(
            &payer,
            &market_key,
            &mint_a,
            &TOKEN,
        )],
        &[],
    )
    .expect("the derived ATA is created");
    let created = env.svm.get_account(&derived).expect("derived ATA exists");
    assert_eq!(
        &created.data[32..64],
        pda::fee_authority(&market_key).as_ref()
    );
}

#[test]
fn fee_account_for_the_wrong_token_program_is_refused() {
    let mut env = Env::new();
    let (market, mint_a, _) = bare_cp_market(&mut env);
    let payer = env.admin.pubkey();
    let market_key = market.market;

    // A legacy mint with the Token-2022 program: the router's mint-owner check refuses it
    // before the ATA program is called.
    let ix = client::init_fee_account(&payer, &market_key, &mint_a, &TOKEN_2022);
    assert!(
        env.send(&[ix], &[]).is_err(),
        "H069: init_fee_account accepted a Token-2022 program for a legacy mint"
    );
    let other = pda::fee_account(&market_key, &mint_a, &TOKEN_2022);
    assert!(
        env.svm.get_account(&other).is_none(),
        "H069: a refused call created the Token-2022 ATA for the legacy mint"
    );
}

#[test]
fn second_init_fee_account_leaves_existing_account_alone() {
    let mut env = Env::new();
    let (market, mint_a, _) = bare_cp_market(&mut env);
    let payer = env.admin.pubkey();
    let market_key = market.market;
    let fee_a = pda::fee_account(&market_key, &mint_a, &TOKEN);

    let init = || client::init_fee_account(&payer, &market_key, &mint_a, &TOKEN);
    env.send(&[init()], &[]).expect("first init");
    let first = env.svm.get_account(&fee_a).expect("fee account");
    env.mint_to(&mint_a, &TOKEN, &fee_a, 123);

    env.send(&[init()], &[])
        .expect("H069: second init must be idempotent");
    let second = env.svm.get_account(&fee_a).expect("fee account");
    assert_eq!(second.owner, first.owner);
    assert_eq!(second.data.len(), first.data.len());
    assert_eq!(
        env.token_balance(&fee_a),
        123,
        "H069: second init changed the balance"
    );
}

#[test]
fn init_fee_account_then_swap_in_one_transaction() {
    let mut env = Env::new();
    let (market, mint_a, mint_b) = bare_cp_market(&mut env);
    let payer = env.admin.pubkey();
    let market_key = market.market;
    let fee_a = pda::fee_account(&market_key, &mint_a, &TOKEN);

    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let amount_in = 1_000_000;
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &mint_a,
        &trader_a,
        &mint_b,
        &trader_b,
    );
    let swap = client::swap(&accounts, &exact_in(amount_in, 1));
    let init = client::init_fee_account(&payer, &market_key, &mint_a, &TOKEN);

    env.send(&[init, swap], &[&trader])
        .expect("H069: init_fee_account then swap in one transaction");
    assert!(
        env.token_balance(&fee_a) > 0,
        "H069: the swap's fee was not collected into the new fee account"
    );
}
