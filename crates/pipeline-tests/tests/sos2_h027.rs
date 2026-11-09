//! SOS audit 2, H027: after the router is frozen, the admin can block every
//! `register_strategy` by pointing `listing_fee_recipient` at an account the runtime
//! write-demotes (or that cannot take lamports), because the recipient is an
//! unconditional `#[account(mut, address = config.listing_fee_recipient)]`.
//!
//! Status: accepted as admin trust. The admin can equally block registrations by raising
//! the listing fee to the cap, and can undo either. These tests pin the behaviour: which
//! recipients block, that the admin can reverse it, and that only the admin can set it.

mod common;

use common::*;
use pipeline_client::{self as client, pda, Instruction, Pubkey};
use solana_keypair::Keypair;
use solana_signer::Signer;

struct Registration {
    program: Pubkey,
    owner: Keypair,
}

fn new_registration(env: &mut Env) -> Registration {
    let program = Pubkey::new_unique();
    let owner = Keypair::new();
    env.svm.airdrop(&owner.pubkey(), SOL).unwrap();
    env.deploy_strategy(&program, "test_strategy_cp", &owner.pubkey());
    Registration { program, owner }
}

fn register_ix(r: &Registration, payer: &Pubkey, recipient: &Pubkey) -> Instruction {
    client::register_strategy(
        payer,
        &r.owner.pubkey(),
        &r.program,
        recipient,
        "https://example.com/strategy.json",
        u64::MAX,
    )
}

fn set_config(env: &mut Env, fee: Option<u64>, recipient: Option<Pubkey>) {
    let ix = client::update_config(
        &env.admin.pubkey(),
        &client::UpdateConfigArgs {
            listing_fee_lamports: fee,
            listing_fee_recipient: recipient,
            ..Default::default()
        },
    );
    env.send(&[ix], &[]).expect("admin update_config");
}

fn freeze(env: &mut Env) {
    set_upgrade_authority(&mut env.svm, &client::PROGRAM_ID, None);
}

fn try_register(env: &mut Env, recipient: &Pubkey) -> Result<(), String> {
    let r = new_registration(env);
    let payer = env.admin.pubkey();
    let ix = register_ix(&r, &payer, recipient);
    match env.send(&[ix], &[&r.owner]) {
        Ok(_) => {
            assert!(env.svm.get_account(&pda::strategy(&r.program)).is_some());
            Ok(())
        }
        Err(failed) => {
            assert!(
                env.svm.get_account(&pda::strategy(&r.program)).is_none(),
                "failed registration left a Strategy account"
            );
            let logs = failed.meta.logs.join(" | ");
            Err(format!("{:?} :: {}", failed.err, logs))
        }
    }
}

/// Recipients to probe: (label, key). Reserved keys are runtime-demoted to read-only; the
/// executables are not necessarily demoted but may not be writable or creditable.
fn candidates(env: &Env) -> Vec<(&'static str, Pubkey)> {
    let rent = client::pubkey!("SysvarRent111111111111111111111111111111111");
    let clock = client::pubkey!("SysvarC1ock11111111111111111111111111111111");
    let compute_budget = client::pubkey!("ComputeBudget111111111111111111111111111111");
    let token = client::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
    let _ = env;
    vec![
        ("System program / Pubkey::default()", Pubkey::default()),
        ("Rent sysvar", rent),
        ("Clock sysvar", clock),
        ("ComputeBudget builtin", compute_budget),
        ("router program id", client::PROGRAM_ID),
        ("SPL Token program (executable)", token),
        ("router config PDA", pda::config()),
    ]
}

/// Records what every candidate recipient does at fee 0 and at the normal fee, after
/// the router is frozen. Prints a matrix; asserts only that the setup works and that a
/// normal recipient is accepted.
#[test]
fn h027_matrix_of_recipients_after_freeze() {
    for (label, key) in candidates(&Env::new()) {
        for fee in [0u64, LISTING_FEE] {
            let mut env = Env::new();
            freeze(&mut env);
            set_config(&mut env, Some(fee), Some(key));
            let out = try_register(&mut env, &key);
            match &out {
                Ok(()) => println!("[{label}] fee {fee}: registered OK"),
                Err(e) => println!("[{label}] fee {fee}: REFUSED {e}"),
            }
        }
    }

    // Control: an ordinary fresh key works at fee 0 and the normal fee.
    for fee in [0u64, LISTING_FEE] {
        let mut env = Env::new();
        freeze(&mut env);
        let fresh = Pubkey::new_unique();
        set_config(&mut env, Some(fee), Some(fresh));
        try_register(&mut env, &fresh).unwrap_or_else(|e| panic!("control at fee {fee}: {e}"));
    }
}

/// Accepted admin trust: at fee 0 the admin can still block every registration by setting
/// a recipient the runtime write-demotes (reserved program ids and sysvars), which fails
/// Anchor's `mut` constraint. Executable but non-reserved accounts and PDAs do not block at
/// fee 0. `update_config` accepts every candidate.
#[test]
fn h027_admin_can_block_zero_fee_registration_via_recipient_accepted() {
    let expected_blocking = [
        "System program / Pubkey::default()",
        "Rent sysvar",
        "Clock sysvar",
        "ComputeBudget builtin",
        "router program id",
    ];
    let mut blocked = Vec::new();
    for (label, key) in candidates(&Env::new()) {
        let mut env = Env::new();
        freeze(&mut env);
        set_config(&mut env, Some(0), Some(key));
        if let Err(e) = try_register(&mut env, &key) {
            assert!(
                e.contains("Custom(2000)"),
                "H027 {label}: expected Anchor ConstraintMut (2000), got {e}"
            );
            blocked.push(label);
        }
    }
    assert_eq!(blocked, expected_blocking);
}

/// The block is reversible by the admin: `update_config` has no dependence on the
/// recipient, so the admin can restore a good recipient (or the fee path) after the
/// freeze. Safety test; passes if the router lets the admin recover.
#[test]
fn h027_block_is_admin_reversible() {
    let mut env = Env::new();
    freeze(&mut env);
    set_config(&mut env, Some(0), Some(Pubkey::default()));
    let bad = Pubkey::default();
    let blocked = try_register(&mut env, &bad);
    println!("blocked state: {blocked:?}");

    // Another party pointing at a different account cannot dodge it (address pinned).
    let other = Pubkey::new_unique();
    assert!(try_register(&mut env, &other).is_err(), "address pin");

    // Setting fee 0 alone (already 0) does not help; a new recipient does.
    let fresh = Pubkey::new_unique();
    set_config(&mut env, None, Some(fresh));
    try_register(&mut env, &fresh).expect("registration works again after admin fix");
}

/// Anyone but the admin cannot do it (access control sanity).
#[test]
fn h027_non_admin_cannot_change_recipient() {
    let mut env = Env::new();
    freeze(&mut env);
    let attacker = env.funded_keypair();
    let ix = client::update_config(
        &attacker.pubkey(),
        &client::UpdateConfigArgs {
            listing_fee_recipient: Some(Pubkey::default()),
            ..Default::default()
        },
    );
    assert!(env.send(&[ix], &[&attacker]).is_err());
}
