//! SOS audit #2, H013: `create_market` does not require `market_state` to be uncreated.
//! The router checks only that it is writable before calling the strategy's
//! `pipeline_init_market`, and checks ownership afterwards. So a strategy-owned account
//! that exists but isn't registered yet reaches the strategy's init with the caller's
//! `init_data` and the market PDA's signature.
//!
//! Asserted here:
//! - an existing evil-strategy state (made by a direct, router-bypassing init call, or
//!   planted with `set_account`) reaches the strategy's init through `create_market` for
//!   every `init_data`. The router doesn't refuse it: the failure comes from the system
//!   program inside the strategy's own `create_account`. Nothing is recorded, and the
//!   honest `register_existing_market` still works afterwards;
//! - a CP pool made by `pipeline_seed_liquidity` (the graduation flow, waiting for
//!   `register_existing_market`) can't be taken over with `create_market`: CP's Anchor
//!   `init` refuses it, nothing is recorded, and registration then records the pool's own
//!   mints and vaults;
//! - once a market is recorded, `create_market` fails at the router's own `Market` init,
//!   before the strategy is called. A second record is impossible, and `created_by` is
//!   first-come through `register_existing_market` anyway, so `create_market` gives no
//!   attribution power that registration doesn't already give.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_strategy_interface as psi;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
/// The evil strategy's market state length (`STATE_LEN`).
const STATE_LEN: usize = 32 * 4 + 2;
/// The system program's `AccountAlreadyInUse` error.
const ALREADY_IN_USE: &str = "already in use";
const CP_FEE_BPS: u16 = 30;

struct Evil {
    mint_a: Pubkey,
    mint_b: Pubkey,
    state: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
    market: Pubkey,
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

fn evil_strategy_accounts(evil: &Evil) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(evil.mint_a, false),
        AccountMeta::new_readonly(evil.mint_b, false),
        AccountMeta::new(evil.vault_a, false),
        AccountMeta::new(evil.vault_b, false),
    ]
}

fn evil_create_ix(creator: &Pubkey, evil: &Evil, init_data: Vec<u8>) -> Instruction {
    client::create_market(&client::CreateMarket {
        creator: *creator,
        strategy_program: EVIL_ID,
        market_state: evil.state,
        market_state_signs: false,
        init_data,
        max_creator_lamports: SOL,
        strategy_accounts: evil_strategy_accounts(evil),
    })
}

/// A direct call to the evil strategy's `pipeline_init_market`, bypassing the router
/// (accepted design): it creates the state at `["evil", a, b]`, unregistered.
fn evil_direct_init_ix(payer: &Pubkey, evil: &Evil, mode: u8) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new_readonly(*payer, false),
        AccountMeta::new(evil.state, false),
        AccountMeta::new(*payer, true),
        AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
    ];
    accounts.extend(evil_strategy_accounts(evil));
    Instruction::new_with_bytes(
        EVIL_ID,
        &psi::instruction_data(psi::discriminator::INIT_MARKET, &vec![mode]),
        accounts,
    )
}

/// Writes an evil-owned, fully formed but unregistered state at the evil PDA.
fn plant_evil_state(env: &mut Env, info_mode: u8) -> Evil {
    let evil = evil_accounts(env);
    let mut data = Vec::with_capacity(STATE_LEN);
    for key in [evil.mint_a, evil.mint_b, evil.vault_a, evil.vault_b] {
        data.extend_from_slice(key.as_ref());
    }
    data.push(255);
    data.push(info_mode);
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
    evil
}

fn invoked_at(logs: &str, program: &Pubkey, depth: u8) -> bool {
    logs.contains(&format!("Program {program} invoke [{depth}]"))
}

/// The evil strategy is native and logs no Anchor errors, so any `AnchorError` in the
/// logs would be a router check refusing the transaction.
fn router_error_logged(logs: &str) -> bool {
    logs.contains("AnchorError")
}

fn market_count(env: &Env, program: &Pubkey) -> u64 {
    client::decode_strategy(&env.account_data(&pda::strategy(program)))
        .unwrap()
        .market_count
}

/// `create_market` on an existing evil state: the router passes it to the strategy's
/// init; the strategy's `create_account` refuses it. Nothing is recorded.
fn assert_reaches_strategy_and_strategy_refuses(env: &mut Env, attacker: &Keypair, evil: &Evil) {
    let state_before = env.svm.get_account(&evil.state).expect("state exists");
    let count = market_count(env, &EVIL_ID);
    for init_data in [vec![0u8], vec![1], vec![4], vec![7], vec![], vec![0xff; 64]] {
        let ix = evil_create_ix(&attacker.pubkey(), evil, init_data.clone());
        let failed = env
            .send(&[ix], &[attacker])
            .expect_err("H013: create_market on an existing state must not succeed here");
        let logs = failed.meta.logs.join("\n");
        assert!(
            invoked_at(&logs, &EVIL_ID, 2),
            "H013: the router called the strategy's init on the existing state \
             (init_data {init_data:?}):\n{logs}"
        );
        assert!(
            logs.contains(ALREADY_IN_USE),
            "H013: the refusal comes from the strategy's own create_account:\n{logs}"
        );
        println!("init_data {init_data:?}: {:?}", failed.err);
        assert!(
            !router_error_logged(&logs),
            "H013: no router check refused the existing state before the init CPI:\n{logs}"
        );
        assert!(
            env.svm.get_account(&evil.market).is_none(),
            "nothing recorded"
        );
        assert_eq!(market_count(env, &EVIL_ID), count);
        let state_after = env.svm.get_account(&evil.state).unwrap();
        assert_eq!(state_after.data, state_before.data, "state untouched");
        assert_eq!(state_after.lamports, state_before.lamports);
        assert_eq!(state_after.owner, EVIL_ID);
    }
}

/// An existing, unregistered evil state (created by a direct init call, bypassing the
/// router) reaches the strategy's init through `create_market` for every `init_data`.
/// The strategy refuses it, nothing is recorded, and the honest registrant still records
/// the market afterwards.
#[test]
fn h013_direct_init_state_reaches_strategy_init_and_strategy_refuses() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let owner = env.funded_keypair();
    let attacker = env.funded_keypair();
    let honest = env.funded_keypair();

    let evil = evil_accounts(&mut env);
    env.send(&[evil_direct_init_ix(&owner.pubkey(), &evil, 0)], &[&owner])
        .expect("setup: direct init creates the state without the router");
    let state = env.svm.get_account(&evil.state).expect("state created");
    assert_eq!(state.owner, EVIL_ID);
    assert_eq!(state.data.len(), STATE_LEN);
    assert!(
        env.svm.get_account(&evil.market).is_none(),
        "not registered"
    );

    assert_reaches_strategy_and_strategy_refuses(&mut env, &attacker, &evil);

    let ix = client::register_existing_market(&honest.pubkey(), &EVIL_ID, &evil.state, vec![]);
    env.send(&[ix], &[&honest])
        .expect("the honest registration is not blocked");
    let market = env.market(&evil.market);
    assert_eq!(market.created_by, honest.pubkey());
    assert_eq!(market.market_state, evil.state);
    assert_eq!(market.mints(), &[evil.mint_a, evil.mint_b]);
}

/// The same with state planted by `set_account` (an arbitrary account the strategy owns).
#[test]
fn h013_planted_state_reaches_strategy_init_and_strategy_refuses() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let attacker = env.funded_keypair();
    let evil = plant_evil_state(&mut env, 0);
    assert!(
        env.svm.get_account(&evil.market).is_none(),
        "not registered"
    );
    assert_reaches_strategy_and_strategy_refuses(&mut env, &attacker, &evil);
}

/// Once a market is recorded, `create_market` on the same state fails at the router's own
/// `Market` init, before the strategy is called; the record doesn't change. `created_by`
/// is first-come through `register_existing_market` already, so `create_market` adds no
/// attribution power.
#[test]
fn h013_recorded_market_create_market_fails_before_strategy_runs() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let owner = env.funded_keypair();
    let first = env.funded_keypair();
    let attacker = env.funded_keypair();

    let evil = evil_accounts(&mut env);
    env.send(&[evil_direct_init_ix(&owner.pubkey(), &evil, 0)], &[&owner])
        .expect("setup: direct init");
    let ix = client::register_existing_market(&first.pubkey(), &EVIL_ID, &evil.state, vec![]);
    env.send(&[ix], &[&first])
        .expect("anyone may register an existing state");
    assert_eq!(
        env.market(&evil.market).created_by,
        first.pubkey(),
        "registration is first-come: the first registrant is created_by"
    );
    let record = env.account_data(&evil.market);
    let count = market_count(&env, &EVIL_ID);

    for init_data in [vec![0u8], vec![1], vec![4]] {
        let ix = evil_create_ix(&attacker.pubkey(), &evil, init_data);
        let failed = env
            .send(&[ix], &[&attacker])
            .expect_err("a recorded market can't be created again");
        let logs = failed.meta.logs.join("\n");
        assert!(
            logs.contains(ALREADY_IN_USE),
            "Market init refuses:\n{logs}"
        );
        assert!(
            !logs.contains(&format!("Program {EVIL_ID} invoke")),
            "the strategy is never called once the market is recorded:\n{logs}"
        );
    }
    assert_eq!(env.account_data(&evil.market), record, "record unchanged");
    assert_eq!(market_count(&env, &EVIL_ID), count);
}

/// CP's graduation-style seeded pool, waiting for `register_existing_market`, can't be
/// taken over with `create_market`: CP's Anchor `init` refuses the existing account, and
/// registration then records the pool's own mints and vaults.
#[test]
fn h013_cp_seeded_pool_create_market_refused_then_registered() {
    let mut env = Env::new();
    env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    let seeder = env.funded_keypair();
    let payer = env.funded_keypair();
    let attacker = env.funded_keypair();
    let honest = env.funded_keypair();

    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 9, None, None);
    let escrow_a = env.create_ata(&seeder.pubkey(), &mint_a, &TOKEN);
    let escrow_b = env.create_ata(&seeder.pubkey(), &mint_b, &TOKEN);
    env.mint_to(&mint_a, &TOKEN, &escrow_a, 1_000_000);
    env.mint_to(&mint_b, &TOKEN, &escrow_b, 2_000_000);
    let fee_seed = CP_FEE_BPS.to_le_bytes();
    let pool = Pubkey::find_program_address(
        &[
            b"seeded",
            seeder.pubkey().as_ref(),
            mint_a.as_ref(),
            mint_b.as_ref(),
            &fee_seed,
        ],
        &CP_PROGRAM_ID,
    )
    .0;
    let vault_a = pda::associated_token(&pool, &mint_a, &TOKEN);
    let vault_b = pda::associated_token(&pool, &mint_b, &TOKEN);
    let request = psi::SeedRequest {
        amounts: vec![1_000_000, 2_000_000],
        data: borsh::to_vec(&CP_FEE_BPS).unwrap(),
    };
    let seed_ix = Instruction::new_with_bytes(
        CP_PROGRAM_ID,
        &psi::instruction_data(psi::discriminator::SEED_LIQUIDITY, &request),
        vec![
            AccountMeta::new_readonly(seeder.pubkey(), true),
            AccountMeta::new(pool, false),
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(seeder.pubkey(), false),
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new(escrow_a, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(escrow_b, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
            AccountMeta::new_readonly(client::ASSOCIATED_TOKEN_PROGRAM_ID, false),
        ],
    );
    env.send(&[seed_ix], &[&seeder, &payer])
        .expect("setup: seed the pool outside the router");
    let market = pda::market(&CP_PROGRAM_ID, &pool);
    assert_eq!(env.svm.get_account(&pool).unwrap().owner, CP_PROGRAM_ID);
    assert_eq!(env.token_balance(&vault_a), 1_000_000);
    assert!(env.svm.get_account(&market).is_none(), "not registered");
    let pool_before = env.account_data(&pool);

    // The attacker's create_market on the seeded pool, with their own fee tier.
    for fee in [CP_FEE_BPS, 0, 9_999] {
        let ix = client::create_market(&client::CreateMarket {
            creator: attacker.pubkey(),
            strategy_program: CP_PROGRAM_ID,
            market_state: pool,
            market_state_signs: false,
            init_data: borsh::to_vec(&fee).unwrap(),
            max_creator_lamports: MAX_SETUP_LAMPORTS,
            strategy_accounts: vec![
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(vault_a, false),
                AccountMeta::new(vault_b, false),
                AccountMeta::new_readonly(TOKEN, false),
                AccountMeta::new_readonly(TOKEN, false),
                AccountMeta::new_readonly(client::ASSOCIATED_TOKEN_PROGRAM_ID, false),
            ],
        });
        let failed = env
            .send(&[ix], &[&attacker])
            .expect_err("H013: create_market must not take over a seeded pool");
        let logs = failed.meta.logs.join("\n");
        assert!(
            invoked_at(&logs, &CP_PROGRAM_ID, 2),
            "the router called CP's init on the seeded pool:\n{logs}"
        );
        println!("fee {fee}: CP refused with {:?}", failed.err);
        assert!(env.svm.get_account(&market).is_none(), "nothing recorded");
        assert_eq!(env.account_data(&pool), pool_before, "pool untouched");
    }

    let ix = client::register_existing_market(&honest.pubkey(), &CP_PROGRAM_ID, &pool, vec![]);
    env.send(&[ix], &[&honest])
        .expect("graduation's registration still works");
    let m = env.market(&market);
    assert_eq!(m.created_by, honest.pubkey());
    assert_eq!(m.mints(), &[mint_a, mint_b]);
    assert_eq!(&m.vaults[..2], &[vault_a, vault_b]);
}
