//! SOS audit 2, H045: `initialize_config` is bound to the router's upgrade authority, runs
//! once, and only before the freeze. These tests cover the cases the hypothesis lists that
//! the H035 replay test does not: a non-authority caller, a router frozen before init, and
//! the unvalidated recipient fields and missing event at init.
//!
//! `Env::new` initialises the config, so these tests build a bare SVM with the router
//! deployed but no config yet.

mod common;

use common::*;
use litesvm::LiteSVM;
use pipeline_client::{self as client, pda, Pubkey};
use solana_keypair::Keypair;
use solana_signer::Signer;

/// Router deployed with upgrade authority `admin`, and no `Config` account yet.
fn bare_env() -> Env {
    let mut svm = LiteSVM::new();
    let admin = Keypair::new();
    svm.airdrop(&admin.pubkey(), 1_000 * SOL).unwrap();
    svm.add_program_from_file(client::PROGRAM_ID, sbf_dir().join("pipeline.so"))
        .expect("build programs first: cd programs && cargo build-sbf");
    set_upgrade_authority(&mut svm, &client::PROGRAM_ID, Some(&admin.pubkey()));
    Env {
        svm,
        admin,
        listing_fee_recipient: Pubkey::new_unique(),
        treasury_owner: Pubkey::new_unique(),
    }
}

fn init_args(recipient: Pubkey, treasury: Pubkey) -> client::ConfigArgs {
    client::ConfigArgs {
        listing_fee_lamports: 0,
        listing_fee_recipient: recipient,
        treasury_owner: treasury,
    }
}

/// Only the router's upgrade authority can create the config. Any other signer is refused
/// with `NotAdmin`, and no config account is created.
#[test]
fn h045_non_authority_cannot_initialize_config() {
    let mut env = bare_env();
    let attacker = env.funded_keypair();
    let args = init_args(Pubkey::new_unique(), Pubkey::new_unique());
    let ix = client::initialize_config(&attacker.pubkey(), &args);
    let result = env.send(&[ix], &[&attacker]);
    assert_fails_with(result, "NotAdmin");
    assert!(
        env.svm.get_account(&pda::config()).is_none(),
        "H045: a refused init created the config account"
    );
}

/// The upgrade authority can initialise the config before the freeze, and the stored
/// admin is the signer.
#[test]
fn h045_upgrade_authority_initializes_config_once() {
    let mut env = bare_env();
    let args = init_args(env.listing_fee_recipient, env.treasury_owner);
    let ix = client::initialize_config(&env.admin.pubkey(), &args);
    env.send(&[ix], &[])
        .expect("initialize_config by upgrade authority");
    let config = client::decode_config(&env.account_data(&pda::config())).expect("config");
    assert_eq!(config.admin, env.admin.pubkey());
    assert!(!config.paused);
}

/// A router frozen before its config is initialised can never have one: init needs the
/// upgrade authority, and a frozen program has none. This is the deploy runbook rule
/// (initialise before freezing), not an exploit. The test pins the behaviour.
#[test]
fn h045_frozen_before_init_makes_config_impossible() {
    let mut env = bare_env();
    set_upgrade_authority(&mut env.svm, &client::PROGRAM_ID, None);
    let args = init_args(env.listing_fee_recipient, env.treasury_owner);
    let ix = client::initialize_config(&env.admin.pubkey(), &args);
    assert_fails_with(env.send(&[ix], &[]), "NotAdmin");
    assert!(env.svm.get_account(&pda::config()).is_none());
}

/// `initialize_config` checks only the listing fee. Recipients are stored as given, so a
/// default `Pubkey` is accepted for both. This is INFO: only the upgrade authority can
/// call init, and the config admin can change both fields later. The test records that
/// init emits no `ConfigChanged` event, so indexers have to read the account.
#[test]
fn h045_init_stores_recipients_unvalidated_and_emits_no_event() {
    let mut env = bare_env();
    let args = init_args(Pubkey::default(), Pubkey::default());
    let ix = client::initialize_config(&env.admin.pubkey(), &args);
    let meta = env.send(&[ix], &[]).expect("init with default recipients");
    let config = client::decode_config(&env.account_data(&pda::config())).expect("config");
    assert_eq!(config.listing_fee_recipient, Pubkey::default());
    assert_eq!(config.treasury_owner, Pubkey::default());
    assert!(
        events(&meta).is_empty(),
        "H045: initialize_config emitted an event; the note in the report is wrong"
    );
}
