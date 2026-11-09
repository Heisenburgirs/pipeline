//! SOS audit #2, H022: the setup-payer funnel. The creator's spend in `create_market` is
//! bounded by Market rent + `max_creator_lamports`, whatever the strategy does with the
//! setup PDA's signature, and the setup PDA ends system-owned, empty and at 0 lamports.
//!
//! Assign-and-return (the strategy assigns the PDA to itself, debits it as owner, zeroes
//! the data and assigns it back to System) needs a fixture that does not exist, so that
//! case is settled by code reading (see .audit/findings/H022.md). The tests here cover
//! partial spends that leave the PDA in the rent-paying band, pre-funding (real and
//! crafted), tiny allowances and lamports sent to the setup PDA after the market exists.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_account::Account;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

const HONEST: u8 = 0;
const INIT_DRAINS_PAYER: u8 = 4;
/// `8 + Market::INIT_SPACE`.
const MARKET_SPACE: usize = 8 + 32 + 32 + 1 + 4 * 32 + 4 * 32 + 1 + 32 + 8 + 1 + 1;
/// The evil strategy's market state (`STATE_LEN`), which its setup pays rent for.
const STATE_LEN: usize = 32 * 4 + 2;

struct Evil {
    mint_a: Pubkey,
    mint_b: Pubkey,
    state: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
    market: Pubkey,
}

impl Evil {
    fn setup_payer(&self) -> Pubkey {
        pda::setup_payer(&self.market)
    }
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

fn create_ix(creator: &Pubkey, evil: &Evil, mode: u8, max_creator_lamports: u64) -> Instruction {
    let strategy_accounts = vec![
        AccountMeta::new_readonly(evil.mint_a, false),
        AccountMeta::new_readonly(evil.mint_b, false),
        AccountMeta::new(evil.vault_a, false),
        AccountMeta::new(evil.vault_b, false),
    ];
    client::create_market(&client::CreateMarket {
        creator: *creator,
        strategy_program: EVIL_ID,
        market_state: evil.state,
        market_state_signs: false,
        init_data: vec![mode],
        max_creator_lamports,
        strategy_accounts,
    })
}

fn rent(env: &Env, len: usize) -> u64 {
    env.svm.minimum_balance_for_rent_exemption(len)
}

/// Gone, or empty, system-owned and holding nothing.
fn assert_setup_payer_empty(env: &Env, evil: &Evil, context: &str) {
    if let Some(account) = env.svm.get_account(&evil.setup_payer()) {
        assert_eq!(
            account.lamports, 0,
            "H022: setup payer keeps lamports ({context})"
        );
        assert_eq!(
            account.owner,
            client::SYSTEM_PROGRAM_ID,
            "H022: setup payer reassigned ({context})"
        );
        assert!(
            account.data.is_empty(),
            "H022: setup payer allocated ({context})"
        );
    }
}

/// Runs one `create_market` and checks the funnel's safety properties, whether it
/// succeeds or fails. `spent` is what the strategy takes from the setup PDA.
/// Returns true if the transaction succeeded.
fn run_and_check(
    env: &mut Env,
    mode: u8,
    allowance: u64,
    prefund: u64,
    spent: u64,
    context: &str,
) -> bool {
    let market_rent = rent(env, MARKET_SPACE);
    let creator = env.funded_keypair();
    let evil = evil_accounts(env);
    if prefund > 0 {
        env.svm.airdrop(&evil.setup_payer(), prefund).unwrap();
    }
    let c0 = env.lamports(&creator.pubkey());
    let ix = create_ix(&creator.pubkey(), &evil, mode, allowance);
    let result = env.send(&[ix], &[&creator]);
    let c1 = env.lamports(&creator.pubkey());
    match result {
        Ok(_) => {
            let debit = c0 as i128 - c1 as i128;
            // Creator debit = Market rent + allowance + prefund-independent spend:
            // spent by the strategy minus what a third party pre-funded.
            assert_eq!(
                debit,
                market_rent as i128 + spent as i128 - prefund as i128,
                "{context}: creator debit"
            );
            assert!(
                debit <= market_rent as i128 + allowance as i128,
                "H022: creator spend exceeds Market rent + max_creator_lamports ({context})"
            );
            assert_setup_payer_empty(env, &evil, context);
            true
        }
        Err(_) => {
            assert_eq!(c1, c0, "{context}: failed tx moved the creator's lamports");
            assert!(env.svm.get_account(&evil.market).is_none(), "{context}");
            // Prefund stays where it was; nothing was taken from the setup PDA.
            assert_eq!(env.lamports(&evil.setup_payer()), prefund, "{context}");
            false
        }
    }
}

/// Partial spends that leave the setup PDA with 1..rent-exempt-minimum lamports at the
/// end of the strategy's instruction (rent-paying for a 0-byte account) are refunded to
/// the creator and the transaction either succeeds exactly or fails cleanly.
#[test]
fn h022_partial_spend_leaving_rent_paying_leftover() {
    let mut env = registered_evil_env();
    let state_rent = rent(&env, STATE_LEN);
    let min0 = rent(&env, 0);
    println!("rent(0)={min0} state_rent={state_rent}");
    let needed_drain = state_rent + SOL;
    for leftover in [0, 1, 5_000, min0 - 1, min0, min0 + 1, 10 * min0] {
        let ok = run_and_check(
            &mut env,
            HONEST,
            state_rent + leftover,
            0,
            state_rent,
            &format!("honest leftover {leftover}"),
        );
        println!("honest leftover {leftover}: success={ok}");
        let ok = run_and_check(
            &mut env,
            INIT_DRAINS_PAYER,
            needed_drain + leftover,
            0,
            needed_drain,
            &format!("drain leftover {leftover}"),
        );
        println!("drain leftover {leftover}: success={ok}");
    }
}

/// A fresh setup PDA cannot receive an allowance below the 0-byte rent-exempt minimum
/// (0 -> rent-paying), so a tiny allowance fails cleanly and moves nothing.
#[test]
fn h022_tiny_allowances_fail_cleanly() {
    let mut env = registered_evil_env();
    let min0 = rent(&env, 0);
    for allowance in [1, 5_000, min0 - 1] {
        let ok = run_and_check(
            &mut env,
            HONEST,
            allowance,
            0,
            0,
            &format!("tiny allowance {allowance}"),
        );
        assert!(
            !ok,
            "allowance {allowance} cannot fund the strategy's state"
        );
    }
}

/// Real pre-funding (a third party sends the setup PDA at least the rent-exempt minimum)
/// only reduces the creator's cost; the bound still holds with a hostile spend.
#[test]
fn h022_prefunded_setup_payer_stays_within_bound() {
    let mut env = registered_evil_env();
    let state_rent = rent(&env, STATE_LEN);
    let min0 = rent(&env, 0);
    let needed_drain = state_rent + SOL;
    for prefund in [min0, state_rent, SOL, 3 * SOL] {
        for allowance in [0, 1, SOL / 10, 2 * SOL] {
            // Honest spend.
            let ok = run_and_check(
                &mut env,
                HONEST,
                allowance,
                prefund,
                state_rent,
                &format!("honest prefund {prefund} allowance {allowance}"),
            );
            assert_eq!(ok, prefund + allowance >= state_rent);
            // Hostile spend of an extra SOL.
            let ok = run_and_check(
                &mut env,
                INIT_DRAINS_PAYER,
                allowance,
                prefund,
                needed_drain,
                &format!("drain prefund {prefund} allowance {allowance}"),
            );
            assert_eq!(ok, prefund + allowance >= needed_drain);
        }
    }
}

/// Crafted state (`set_account`, not reachable by a plain transfer): a setup PDA already
/// holding a rent-paying balance below the 0-byte minimum. The allowance tops it up and
/// the creator is refunded the leftover; with no allowance the transaction fails cleanly.
#[test]
fn h022_crafted_rent_paying_prefund() {
    let mut env = registered_evil_env();
    let state_rent = rent(&env, STATE_LEN);
    let min0 = rent(&env, 0);
    let market_rent = rent(&env, MARKET_SPACE);
    for crafted in [1u64, 1_000, min0 - 1] {
        for allowance in [0, state_rent, state_rent + 7, SOL] {
            let creator = env.funded_keypair();
            let evil = evil_accounts(&mut env);
            env.svm
                .set_account(
                    evil.setup_payer(),
                    Account {
                        lamports: crafted,
                        data: vec![],
                        owner: client::SYSTEM_PROGRAM_ID,
                        executable: false,
                        rent_epoch: 0,
                    },
                )
                .unwrap();
            let c0 = env.lamports(&creator.pubkey());
            let ix = create_ix(&creator.pubkey(), &evil, HONEST, allowance);
            let result = env.send(&[ix], &[&creator]);
            let c1 = env.lamports(&creator.pubkey());
            let context = format!("crafted {crafted} allowance {allowance}");
            match result {
                Ok(_) => {
                    let debit = c0 as i128 - c1 as i128;
                    assert_eq!(
                        debit,
                        market_rent as i128 + state_rent as i128 - crafted as i128,
                        "{context}"
                    );
                    assert!(
                        debit <= market_rent as i128 + allowance as i128,
                        "{context}"
                    );
                    assert_setup_payer_empty(&env, &evil, &context);
                    println!("{context}: success");
                }
                Err(failed) => {
                    assert_eq!(c1, c0, "{context}");
                    assert_eq!(env.lamports(&evil.setup_payer()), crafted, "{context}");
                    println!("{context}: fails ({:?})", failed.err);
                }
            }
        }
    }
}

/// Lamports sent to the setup PDA after its market exists (for example a strategy
/// account closed to it, or a donation) can never be moved: the only signer is the router
/// inside `create_market`, which can't run again for an existing market.
#[test]
fn h022_lamports_sent_after_creation_are_stranded() {
    let mut env = registered_evil_env();
    let creator = env.funded_keypair();
    let evil = evil_accounts(&mut env);
    let ix = create_ix(&creator.pubkey(), &evil, HONEST, MAX_SETUP_LAMPORTS);
    env.send(&[ix], &[&creator]).unwrap();
    assert_setup_payer_empty(&env, &evil, "after creation");

    let late = rent(&env, 0) + 123;
    env.svm.airdrop(&evil.setup_payer(), late).unwrap();
    let ix = create_ix(&creator.pubkey(), &evil, HONEST, MAX_SETUP_LAMPORTS);
    assert!(env.send(&[ix], &[&creator]).is_err());
    assert_eq!(env.lamports(&evil.setup_payer()), late);
}
