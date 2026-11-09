//! SOS audit 2, H035: what the admin can still do, field by field, after the router is
//! frozen (upgrade authority = None).
//!
//! `Config` has six fields: `admin`, `paused`, `listing_fee_lamports`,
//! `listing_fee_recipient`, `treasury_owner`, `bump`. Writers: `initialize_config` (once,
//! needs the upgrade authority), `update_config` (admin) and `lift_pause` (anyone, frozen
//! only, writes `paused = false`). These tests show, with the router frozen, that
//! - no other instruction writes `Config`, and `update_config` writes no `Strategy`/`Market`;
//! - `treasury_owner` redirects fees already accrued (accepted: "change recipients");
//! - `admin` can be handed to a dead key, which freezes the config in place;
//! - the listing fee is bounded by the cap and the rent floor;
//! - a hostile config (cap fee, dead recipient, dead admin) does not touch swaps, market
//!   creation, fee-account creation, metadata updates or sweeps for other people;
//! - `paused` cannot be set after the freeze, and a non-admin cannot write any field.
//!
//! The recipient write-demotion block on registration is H027, not repeated here.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;

fn freeze(env: &mut Env) {
    set_upgrade_authority(&mut env.svm, &client::PROGRAM_ID, None);
}

fn config_bytes(env: &Env) -> Vec<u8> {
    env.account_data(&pda::config())
}

fn config(env: &Env) -> client::Config {
    client::decode_config(&config_bytes(env)).expect("config")
}

fn update(env: &mut Env, args: client::UpdateConfigArgs) -> TxResult {
    let ix = client::update_config(&env.admin.pubkey(), &args);
    env.send(&[ix], &[])
}

/// A CP market, a funded trader and a swap instruction on it.
fn trade(env: &mut Env) -> (CpMarket, Keypair, pipeline_client::Instruction) {
    let market = setup_cp_market(env, TOKEN, TOKEN, 1_000_000_000, 2_000_000_000, 30);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 100_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    let ix = client::swap(&accounts, &exact_in(1_000_000, 0));
    (market, trader, ix)
}

fn fee_balance(env: &Env, market: &CpMarket) -> u64 {
    env.token_balance(&pda::fee_account(&market.market, &market.mint_a, &TOKEN))
}

fn treasury_of(env: &Env, owner: &Pubkey, market: &CpMarket) -> Pubkey {
    let _ = env;
    pda::associated_token(owner, &market.mint_a, &TOKEN)
}

fn sweep(env: &mut Env, market: &CpMarket, owner: &Pubkey) -> TxResult {
    let ix = client::sweep_fees(&market.market, &market.mint_a, &TOKEN, owner, vec![]);
    env.send(&[ix], &[])
}

/// Every `Config` writer after the freeze: only `update_config` (admin) and `lift_pause`
/// (a no-op write while unpaused) touch the account. Swap, sweep, create_market,
/// init_fee_account, register_strategy, update_metadata, register_existing_market and
/// a failed update leave the Config bytes untouched. `update_config` leaves `Strategy`
/// and `Market` untouched. Settled by test (plus code reading: `Config` is `mut` only in
/// `UpdateConfig` and `LiftPause`).
#[test]
fn h035_only_update_config_and_lift_pause_write_config() {
    let mut env = Env::new();
    let (market, trader, swap) = trade(&mut env);
    freeze(&mut env);
    let before = config_bytes(&env);

    // swap
    env.send(std::slice::from_ref(&swap), &[&trader])
        .expect("swap");
    assert!(fee_balance(&env, &market) > 0, "setup: fees accrued");
    assert_eq!(config_bytes(&env), before, "H035: swap wrote Config");

    // sweep
    let owner = env.treasury_owner;
    env.create_ata(&owner, &market.mint_a, &TOKEN);
    sweep(&mut env, &market, &owner).expect("sweep");
    assert_eq!(config_bytes(&env), before, "H035: sweep wrote Config");

    // create_market + init_fee_account (inside setup_cp_market)
    let _second = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000, 1_000_000, 30);
    assert_eq!(
        config_bytes(&env),
        before,
        "H035: create_market / init_fee_account wrote Config"
    );

    // register_strategy (listing fee paid) + update_metadata
    let program = Pubkey::new_unique();
    let reg_owner = env.funded_keypair();
    env.deploy_strategy(&program, "test_strategy_cp", &reg_owner.pubkey());
    let recipient = env.listing_fee_recipient;
    let register = client::register_strategy(
        &reg_owner.pubkey(),
        &reg_owner.pubkey(),
        &program,
        &recipient,
        "https://example.com/a.json",
        u64::MAX,
    );
    env.send(&[register], &[&reg_owner]).expect("register");
    assert_eq!(config_bytes(&env), before, "H035: register wrote Config");
    let metadata =
        client::update_metadata(&reg_owner.pubkey(), &program, "https://example.com/b.json");
    env.send(&[metadata], &[&reg_owner])
        .expect("update_metadata");
    assert_eq!(
        config_bytes(&env),
        before,
        "H035: update_metadata wrote Config"
    );

    // lift_pause on a frozen, unpaused router rewrites paused=false: identical bytes.
    env.send(&[client::lift_pause()], &[]).expect("lift_pause");
    assert_eq!(
        config_bytes(&env),
        before,
        "H035: lift_pause changed more than paused"
    );

    // A non-admin update fails and writes nothing.
    let attacker = env.funded_keypair();
    let ix = client::update_config(
        &attacker.pubkey(),
        &client::UpdateConfigArgs {
            admin: Some(attacker.pubkey()),
            treasury_owner: Some(attacker.pubkey()),
            listing_fee_lamports: Some(0),
            listing_fee_recipient: Some(attacker.pubkey()),
            ..Default::default()
        },
    );
    assert_fails_with(env.send(&[ix], &[&attacker]), "NotAdmin");
    assert_eq!(
        config_bytes(&env),
        before,
        "H035: failed update wrote Config"
    );

    // The admin's update_config writes only Config: Strategy and Market are untouched.
    let strategy = pda::strategy(&CP_PROGRAM_ID);
    let strategy_before = env.account_data(&strategy);
    let market_before = env.account_data(&market.market);
    update(
        &mut env,
        client::UpdateConfigArgs {
            listing_fee_lamports: Some(0),
            listing_fee_recipient: Some(Pubkey::new_unique()),
            treasury_owner: Some(Pubkey::new_unique()),
            ..Default::default()
        },
    )
    .expect("admin update_config");
    assert_ne!(
        config_bytes(&env),
        before,
        "setup: update_config changed Config"
    );
    assert_eq!(
        env.account_data(&strategy),
        strategy_before,
        "H035: Strategy changed"
    );
    assert_eq!(
        env.account_data(&market.market),
        market_before,
        "H035: Market changed"
    );
}

/// `treasury_owner`: redirects fees that were accrued before the change. The next sweep
/// pays the new owner; the old owner's account gets nothing; a pre-built sweep to the
/// old treasury fails `WrongTreasuryAccount`. Without the new owner's token account the
/// sweep reverts and anyone can create that account, so a redirect cannot jam sweeps.
/// Pointing it at an address nobody controls sends the fees there. This is the accepted
/// "change recipients" power; the test records its exact reach.
#[test]
fn h035_treasury_owner_redirects_accrued_fees_and_cannot_jam_sweeps() {
    let mut env = Env::new();
    let (market, trader, swap) = trade(&mut env);
    freeze(&mut env);
    env.send(&[swap], &[&trader]).expect("swap");
    let accrued = fee_balance(&env, &market);
    assert!(accrued > 0, "setup: fees accrued before the redirect");

    let old_owner = env.treasury_owner;
    let old_treasury = env.create_ata(&old_owner, &market.mint_a, &TOKEN);
    let new_owner = Pubkey::new_unique();
    update(
        &mut env,
        client::UpdateConfigArgs {
            treasury_owner: Some(new_owner),
            ..Default::default()
        },
    )
    .expect("setup: admin redirects treasury_owner after the freeze");
    assert_eq!(config(&env).treasury_owner, new_owner);

    // A sweep built for the old treasury now fails (no fee loss, no jam).
    assert_fails_with(sweep(&mut env, &market, &old_owner), "WrongTreasuryAccount");
    assert_eq!(fee_balance(&env, &market), accrued);

    // New owner has no token account yet: reverts, funds stay in the fee account.
    assert!(sweep(&mut env, &market, &new_owner).is_err());
    assert_eq!(fee_balance(&env, &market), accrued);

    // Anyone creates the account (idempotent ATA creation); then the sweep works.
    let new_treasury = env.create_ata(&new_owner, &market.mint_a, &TOKEN);
    assert_eq!(new_treasury, treasury_of(&env, &new_owner, &market));
    sweep(&mut env, &market, &new_owner).expect("sweep after the redirect");
    assert_eq!(
        env.token_balance(&new_treasury),
        accrued,
        "accrued fees moved to the new owner"
    );
    assert_eq!(
        env.token_balance(&old_treasury),
        0,
        "old treasury received nothing"
    );
    println!("treasury redirect: {accrued} accrued fee tokens went to the new owner");

    // Dead owner: Pubkey::default() still has a creatable ATA, so the sweep completes
    // and the tokens are unrecoverable. Admin-only; recorded, not a jam.
    update(
        &mut env,
        client::UpdateConfigArgs {
            treasury_owner: Some(Pubkey::default()),
            ..Default::default()
        },
    )
    .expect("dead treasury owner accepted");
    let (trader2, trader2_a) = env.trader_with(&market.mint_a, &TOKEN, 10_000_000);
    let trader2_b = env.create_ata(&trader2.pubkey(), &market.mint_b, &TOKEN);
    let accounts = cp_swap_accounts(
        &market,
        &trader2.pubkey(),
        &market.mint_a,
        &trader2_a,
        &market.mint_b,
        &trader2_b,
    );
    env.send(
        &[client::swap(&accounts, &exact_in(1_000_000, 0))],
        &[&trader2],
    )
    .expect("swap after redirect");
    let dead = env.create_ata(&Pubkey::default(), &market.mint_a, &TOKEN);
    sweep(&mut env, &market, &Pubkey::default()).expect("sweep to the dead owner");
    assert_eq!(fee_balance(&env, &market), 0);
    assert!(env.token_balance(&dead) > 0);
}

/// `admin`: one-step handover to any key, including `Pubkey::default()`. After a handover
/// to a dead key nobody can call `update_config` again, so the config (including any
/// H027 recipient) is permanent, but nothing else is affected. The old admin loses power
/// at once. Pause: still impossible after the freeze, for either admin.
#[test]
fn h035_admin_handover_to_dead_key_freezes_config_only() {
    let mut env = Env::new();
    let (market, trader, swap) = trade(&mut env);
    freeze(&mut env);

    // Pause cannot be set after the freeze (H001) -- also not bundled with other fields.
    let paused = update(
        &mut env,
        client::UpdateConfigArgs {
            paused: Some(true),
            listing_fee_lamports: Some(0),
            ..Default::default()
        },
    );
    assert_fails_with(paused, "FrozenRouterCannotPause");
    assert_eq!(
        config(&env).listing_fee_lamports,
        LISTING_FEE,
        "bundled update reverted whole"
    );

    // Hand to a live key: the new key works, the old one is locked out.
    let new_admin = env.funded_keypair();
    update(
        &mut env,
        client::UpdateConfigArgs {
            admin: Some(new_admin.pubkey()),
            ..Default::default()
        },
    )
    .expect("handover");
    assert_fails_with(
        update(&mut env, client::UpdateConfigArgs::default()),
        "NotAdmin",
    );
    let ix = client::update_config(
        &new_admin.pubkey(),
        &client::UpdateConfigArgs {
            listing_fee_lamports: Some(0),
            ..Default::default()
        },
    );
    env.send(&[ix], &[&new_admin]).expect("new admin works");

    // Hand to the dead key.
    let ix = client::update_config(
        &new_admin.pubkey(),
        &client::UpdateConfigArgs {
            admin: Some(Pubkey::default()),
            ..Default::default()
        },
    );
    env.send(&[ix], &[&new_admin])
        .expect("handover to Pubkey::default()");
    assert_eq!(config(&env).admin, Pubkey::default());
    let before = config_bytes(&env);
    let ix = client::update_config(&new_admin.pubkey(), &client::UpdateConfigArgs::default());
    assert_fails_with(env.send(&[ix], &[&new_admin]), "NotAdmin");
    assert_eq!(config_bytes(&env), before);

    // Everything else keeps working: swap, sweep, market creation, registration (fee 0).
    env.send(&[swap], &[&trader])
        .expect("swap with a dead admin");
    let owner = env.treasury_owner;
    env.create_ata(&owner, &market.mint_a, &TOKEN);
    sweep(&mut env, &market, &owner).expect("sweep with a dead admin");
    let _ = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000, 1_000_000, 30);
    let program = Pubkey::new_unique();
    let reg_owner = env.funded_keypair();
    env.deploy_strategy(&program, "test_strategy_cp", &reg_owner.pubkey());
    let recipient = env.listing_fee_recipient;
    let register = client::register_strategy(
        &reg_owner.pubkey(),
        &reg_owner.pubkey(),
        &program,
        &recipient,
        "https://example.com/a.json",
        u64::MAX,
    );
    env.send(&[register], &[&reg_owner])
        .expect("register with a dead admin");
}

/// `listing_fee_lamports`: at most the cap and either 0 or at least the rent floor.
/// Nothing but `register_strategy` reads it. At the cap, every swap, sweep and market
/// creation still works; registration costs 1,000 SOL (deterrent only: H005).
#[test]
fn h035_listing_fee_is_bounded_and_read_only_by_registration() {
    let mut env = Env::new();
    let (market, trader, swap) = trade(&mut env);
    freeze(&mut env);

    let cap = 1_000_000_000_000u64;
    let fail = |env: &mut Env, fee: u64, needle: &str| {
        let r = update(
            env,
            client::UpdateConfigArgs {
                listing_fee_lamports: Some(fee),
                ..Default::default()
            },
        );
        assert_fails_with(r, needle);
    };
    fail(&mut env, cap + 1, "ListingFeeTooHigh");
    fail(&mut env, u64::MAX, "ListingFeeTooHigh");
    fail(&mut env, 1, "ListingFeeBelowRent");
    assert_eq!(config(&env).listing_fee_lamports, LISTING_FEE);

    update(
        &mut env,
        client::UpdateConfigArgs {
            listing_fee_lamports: Some(cap),
            ..Default::default()
        },
    )
    .expect("cap accepted");

    // Swap, sweep, create_market, init_fee_account at the cap fee.
    env.send(&[swap], &[&trader]).expect("swap at cap fee");
    let owner = env.treasury_owner;
    env.create_ata(&owner, &market.mint_a, &TOKEN);
    sweep(&mut env, &market, &owner).expect("sweep at cap fee");
    let _ = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000, 1_000_000, 30);

    // A normal registrant (100 SOL) cannot pay; max_listing_fee_lamports also protects.
    let program = Pubkey::new_unique();
    let reg_owner = env.funded_keypair();
    env.deploy_strategy(&program, "test_strategy_cp", &reg_owner.pubkey());
    let recipient = env.listing_fee_recipient;
    let capped = client::register_strategy(
        &reg_owner.pubkey(),
        &reg_owner.pubkey(),
        &program,
        &recipient,
        "https://example.com/a.json",
        LISTING_FEE,
    );
    assert_fails_with(env.send(&[capped], &[&reg_owner]), "ListingFeeAboveMaximum");

    // Fee 0 restores free registration.
    update(
        &mut env,
        client::UpdateConfigArgs {
            listing_fee_lamports: Some(0),
            ..Default::default()
        },
    )
    .expect("fee 0");
    let free = client::register_strategy(
        &reg_owner.pubkey(),
        &reg_owner.pubkey(),
        &program,
        &recipient,
        "https://example.com/a.json",
        0,
    );
    env.send(&[free], &[&reg_owner]).expect("free registration");
}

/// A hostile config reaches no trader, sweeper or market creator: with the cap fee, a
/// dead listing recipient and a dead admin all set after the freeze, swaps, market
/// creation, fee-account creation, update_metadata and (with the unchanged treasury) sweeps
/// behave exactly as before. Only `register_strategy` can be affected (H027).
#[test]
fn h035_hostile_config_does_not_touch_swaps_markets_or_sweeps() {
    let mut env = Env::new();
    let (market, trader, swap) = trade(&mut env);
    let reg_program = Pubkey::new_unique();
    let reg_owner = env.funded_keypair();
    env.deploy_strategy(&reg_program, "test_strategy_cp", &reg_owner.pubkey());
    let ix = client::register_strategy(
        &reg_owner.pubkey(),
        &reg_owner.pubkey(),
        &reg_program,
        &env.listing_fee_recipient.clone(),
        "https://example.com/a.json",
        u64::MAX,
    );
    env.send(&[ix], &[&reg_owner])
        .expect("setup: registered strategy");
    freeze(&mut env);

    update(
        &mut env,
        client::UpdateConfigArgs {
            admin: Some(Pubkey::default()),
            listing_fee_lamports: Some(1_000_000_000_000),
            listing_fee_recipient: Some(Pubkey::default()),
            ..Default::default()
        },
    )
    .expect("setup: hostile config written after the freeze");
    let cfg = config(&env);
    assert_eq!(cfg.admin, Pubkey::default());
    assert_eq!(cfg.listing_fee_recipient, Pubkey::default());
    assert!(!cfg.paused);

    env.send(&[swap], &[&trader])
        .expect("H035: swap blocked by hostile config");
    assert!(fee_balance(&env, &market) > 0);
    let owner = env.treasury_owner;
    env.create_ata(&owner, &market.mint_a, &TOKEN);
    sweep(&mut env, &market, &owner).expect("H035: sweep blocked by hostile config");
    assert_eq!(fee_balance(&env, &market), 0);
    let _ = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000, 1_000_000, 30);
    let metadata = client::update_metadata(
        &reg_owner.pubkey(),
        &reg_program,
        "https://example.com/c.json",
    );
    env.send(&[metadata], &[&reg_owner])
        .expect("H035: update_metadata blocked by hostile config");

    // Registration is the one thing that fails (fee 1,000 SOL / dead recipient: H027).
    let program = Pubkey::new_unique();
    let owner2 = env.funded_keypair();
    env.deploy_strategy(&program, "test_strategy_cp", &owner2.pubkey());
    let register = client::register_strategy(
        &owner2.pubkey(),
        &owner2.pubkey(),
        &program,
        &Pubkey::default(),
        "https://example.com/a.json",
        u64::MAX,
    );
    assert!(env.send(&[register], &[&owner2]).is_err());
}

/// Fields written only by `initialize_config`: `bump` and the initial `admin`. A second
/// `initialize_config` cannot overwrite the live config (the PDA is `init`), even by the
/// upgrade authority before the freeze, and after the freeze nobody can call it at all.
#[test]
fn h035_initialize_config_cannot_be_replayed() {
    let mut env = Env::new();
    let before = config_bytes(&env);
    let args = client::ConfigArgs {
        listing_fee_lamports: 0,
        listing_fee_recipient: Pubkey::new_unique(),
        treasury_owner: Pubkey::new_unique(),
    };
    let ix = client::initialize_config(&env.admin.pubkey(), &args);
    assert!(
        env.send(std::slice::from_ref(&ix), &[]).is_err(),
        "replay before freeze"
    );
    assert_eq!(config_bytes(&env), before);
    freeze(&mut env);
    assert!(env.send(&[ix], &[]).is_err(), "replay after freeze");
    assert_eq!(config_bytes(&env), before);
}
