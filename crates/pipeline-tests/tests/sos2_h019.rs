//! SOS2 H019: `register_existing_market` is first-write-wins, from caller-chosen info
//! accounts, with no correction path.
//!
//! The router forwards every trailing account (read-only, unsigned) to the strategy's
//! `pipeline_market_info` and records the first answer permanently. No prebuilt fixture
//! reads trailing accounts in `market_info` (the evil strategy derives everything from its
//! state account), so "a strategy that trusts trailing accounts" is settled by code
//! reading. What these LiteSVM tests settle:
//!
//! 1. Hostile trailing accounts (signer, writable, router PDAs, the payer's tokens) cannot
//!    escalate or steer a strategy whose info comes from its state.
//! 2. The record is first-write-wins: a second registration fails, a state that later
//!    changes leaves the record stale, and nothing in the router can correct it.
//! 3. The first registrant gains only `created_by` and pays only the Market rent.
//! 4. The router records whatever vault keys the strategy returns without checking that
//!    they exist, so a state registered before it is complete is frozen incomplete.
//!
//! The tests describe a design limitation and pass; they assert the router-side safe
//! behaviour (no caller control over the record, no mutation, no extra privilege).

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Pubkey};
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
const STATE_LEN: usize = 32 * 4 + 2;
const HONEST: u8 = 0;

struct Evil {
    mint_a: Pubkey,
    mint_b: Pubkey,
    state: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
    market: Pubkey,
}

fn registered_evil_env() -> Env {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    env
}

fn evil_accounts(env: &mut Env) -> Evil {
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    Evil {
        mint_a,
        mint_b,
        state,
        vault_a,
        vault_b,
        market: pda::market(&EVIL_ID, &state),
    }
}

/// Writes the evil strategy's state account directly (as if the strategy made it itself).
fn write_state(env: &mut Env, evil: &Evil, vault_a: &Pubkey, vault_b: &Pubkey) {
    let mut data = Vec::with_capacity(STATE_LEN);
    for key in [&evil.mint_a, &evil.mint_b, vault_a, vault_b] {
        data.extend_from_slice(key.as_ref());
    }
    data.push(255);
    data.push(HONEST);
    let lamports = env.svm.minimum_balance_for_rent_exemption(STATE_LEN);
    env.svm
        .set_account(
            evil.state,
            solana_account::Account {
                lamports,
                data,
                owner: EVIL_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
}

fn plant(env: &mut Env) -> Evil {
    let evil = evil_accounts(env);
    write_state(env, &evil, &evil.vault_a, &evil.vault_b);
    evil
}

fn market_count(env: &Env) -> u64 {
    client::decode_strategy(&env.account_data(&pda::strategy(&EVIL_ID)))
        .unwrap()
        .market_count
}

/// Property 1 (test): trailing accounts with signer/writable flags in the caller's
/// instruction, including the router's own config and strategy PDAs and the payer's own
/// token account, change nothing. The router forwards them read-only and unsigned (code
/// reading, `fetch_market_info`), and the record equals the state's own fields.
#[test]
fn h019_hostile_trailing_accounts_do_not_steer_or_escalate() {
    let mut env = registered_evil_env();
    let attacker = env.funded_keypair();
    let evil = plant(&mut env);
    let attacker_tokens = env.create_ata(&attacker.pubkey(), &evil.mint_a, &TOKEN);
    let decoy = Keypair::new();

    let trailing = vec![
        AccountMeta::new(attacker.pubkey(), true),
        AccountMeta::new(attacker_tokens, false),
        AccountMeta::new(pda::config(), false),
        AccountMeta::new(pda::strategy(&EVIL_ID), false),
        AccountMeta::new(evil.vault_a, false),
        AccountMeta::new(evil.state, false),
        AccountMeta::new(decoy.pubkey(), false),
    ];
    let ix = client::register_existing_market(&attacker.pubkey(), &EVIL_ID, &evil.state, trailing);
    let state_before = env.account_data(&evil.state);
    let config_before = env.account_data(&pda::config());
    let attacker_sol = env.lamports(&attacker.pubkey());
    env.send(&[ix], &[&attacker])
        .expect("setup: registration with hostile trailing accounts");

    let market = env.market(&evil.market);
    assert_eq!(
        market.mints[..2],
        [evil.mint_a, evil.mint_b],
        "H019: caller-chosen trailing accounts altered the recorded mints"
    );
    assert_eq!(
        market.vaults[..2],
        [evil.vault_a, evil.vault_b],
        "H019: caller-chosen trailing accounts altered the recorded vaults"
    );
    assert_eq!(env.account_data(&evil.state), state_before);
    assert_eq!(env.account_data(&pda::config()), config_before);
    assert!(env.svm.get_account(&decoy.pubkey()).is_none());
    // Only the Market rent (plus fee-payer signature costs) left the attacker's wallet.
    let rent = env
        .svm
        .minimum_balance_for_rent_exemption(env.account_data(&evil.market).len());
    assert!(attacker_sol - env.lamports(&attacker.pubkey()) <= rent + 10_000);
}

/// Properties 2 and 3 (test): first registration wins and is permanent. The first
/// registrant is `created_by` and nothing else. A second registration fails with "already
/// in use". If the strategy's state changes afterwards the record stays stale, and the
/// router has no instruction that writes a `Market` after creation (code reading: the
/// only `Market` writers are `create_market` and `register_existing_market` via
/// `record_market`, both behind `init`).
#[test]
fn h019_first_registration_is_permanent_and_cannot_be_corrected() {
    let mut env = registered_evil_env();
    let front_runner = env.funded_keypair();
    let honest = env.funded_keypair();
    let evil = plant(&mut env);
    let count = market_count(&env);
    let listing_recipient_before = env.lamports(&env.listing_fee_recipient.clone());

    let ix =
        client::register_existing_market(&front_runner.pubkey(), &EVIL_ID, &evil.state, vec![]);
    env.send(&[ix], &[&front_runner])
        .expect("setup: first registration");
    assert_eq!(market_count(&env), count + 1);
    let recorded = env.account_data(&evil.market);
    assert_eq!(env.market(&evil.market).created_by, front_runner.pubkey());
    // Registration is free of the listing fee.
    assert_eq!(
        env.lamports(&env.listing_fee_recipient.clone()),
        listing_recipient_before
    );

    // The strategy later changes its own state (new vaults); a new honest registration
    // is refused, and so is every other path to rewrite the record.
    let new_vault_a = env.create_ata(&honest.pubkey(), &evil.mint_a, &TOKEN);
    let new_vault_b = env.create_ata(&honest.pubkey(), &evil.mint_b, &TOKEN);
    write_state(&mut env, &evil, &new_vault_a, &new_vault_b);
    let again = client::register_existing_market(&honest.pubkey(), &EVIL_ID, &evil.state, vec![]);
    assert_fails_with(env.send(&[again], &[&honest]), "already in use");
    assert_eq!(env.account_data(&evil.market), recorded);
    assert_eq!(market_count(&env), count + 1);

    // Nothing in the router rewrites it: the other market-touching instructions leave
    // the bytes alone.
    let payer = env.admin.pubkey();
    env.send(
        &[client::init_fee_account(
            &payer,
            &evil.market,
            &evil.mint_a,
            &TOKEN,
        )],
        &[],
    )
    .unwrap();
    assert_eq!(env.account_data(&evil.market), recorded);

    // The stale record still names the original vaults, not the strategy's current state.
    let market = env.market(&evil.market);
    assert_eq!(market.vaults[..2], [evil.vault_a, evil.vault_b]);
    assert_ne!(market.vaults[..2], [new_vault_a, new_vault_b]);
}

/// Property 4 (test): the router validates the shape of `MarketInfo`, not that the vaults
/// exist. A state registered before its vaults are created records those keys permanently.
/// This is the router trusting the strategy (by design), not caller control.
#[test]
fn h019_router_does_not_check_vaults_exist_at_registration() {
    let mut env = registered_evil_env();
    let payer = env.funded_keypair();
    let evil = evil_accounts(&mut env);
    let future_a = Pubkey::new_unique();
    let future_b = Pubkey::new_unique();
    write_state(&mut env, &evil, &future_a, &future_b);
    assert!(env.svm.get_account(&future_a).is_none());

    let ix = client::register_existing_market(&payer.pubkey(), &EVIL_ID, &evil.state, vec![]);
    env.send(&[ix], &[&payer])
        .expect("setup: registration with not-yet-existing vaults");
    let market = env.market(&evil.market);
    assert_eq!(market.vaults[..2], [future_a, future_b]);
    // The state later becomes complete; the record is not refreshed.
    write_state(&mut env, &evil, &evil.vault_a, &evil.vault_b);
    let again = client::register_existing_market(&payer.pubkey(), &EVIL_ID, &evil.state, vec![]);
    assert_fails_with(env.send(&[again], &[&payer]), "already in use");
    assert_eq!(env.market(&evil.market).vaults[..2], [future_a, future_b]);
}
