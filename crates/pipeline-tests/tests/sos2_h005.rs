//! SOS audit 2, H005: the listing-fee payer maximum is checked against the value that is
//! actually transferred (recheck of audit 1 H023(a)).
//!
//! Each test asserts the safe behaviour and passes if the router is safe.

mod common;

use common::*;
use pipeline_client::{self as client, pda, Instruction, Pubkey};
use pipeline_events::PipelineEvent;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

/// `8 + Strategy::INIT_SPACE` in programs/pipeline/src/lib.rs.
const STRATEGY_SPACE: usize = 8 + 32 + 4 + 200 + 1 + 8 + 8 + 1;

/// A strategy owner (registrar) plus a separate sponsor who pays.
struct Registration {
    program: Pubkey,
    owner: Keypair,
    sponsor: Keypair,
}

fn new_registration(env: &mut Env) -> Registration {
    let program = Pubkey::new_unique();
    let owner = Keypair::new();
    env.svm.airdrop(&owner.pubkey(), SOL).unwrap();
    env.deploy_strategy(&program, "test_strategy_cp", &owner.pubkey());
    let sponsor = Keypair::new();
    env.svm.airdrop(&sponsor.pubkey(), 50 * SOL).unwrap();
    Registration {
        program,
        owner,
        sponsor,
    }
}

fn register_ix(env: &Env, r: &Registration, max: u64) -> Instruction {
    client::register_strategy(
        &r.sponsor.pubkey(),
        &r.owner.pubkey(),
        &r.program,
        &env.listing_fee_recipient,
        "https://example.com/strategy.json",
        max,
    )
}

fn fee_update(env: &Env, fee: u64) -> Instruction {
    client::update_config(
        &env.admin.pubkey(),
        &client::UpdateConfigArgs {
            listing_fee_lamports: Some(fee),
            ..Default::default()
        },
    )
}

fn stored_fee(env: &Env) -> u64 {
    client::decode_config(&env.account_data(&pda::config()))
        .expect("config")
        .listing_fee_lamports
}

/// Sends with the admin as fee payer and returns the message's account keys too, so
/// inner instructions can be decoded.
fn send_with_keys(
    env: &mut Env,
    instructions: &[Instruction],
    signers: &[&Keypair],
) -> (TxResult, Vec<Pubkey>) {
    env.svm.expire_blockhash();
    let mut all: Vec<&Keypair> = vec![&env.admin];
    all.extend_from_slice(signers);
    let tx = Transaction::new_signed_with_payer(
        instructions,
        Some(&env.admin.pubkey()),
        &all,
        env.svm.latest_blockhash(),
    );
    let keys = tx.message.account_keys.clone();
    (env.svm.send_transaction(tx), keys)
}

/// Every lamport-moving System instruction (CreateAccount = 0, Transfer = 2) among the
/// inner instructions, as (kind, from, to, lamports).
fn inner_system_moves(
    meta: &litesvm::types::TransactionMetadata,
    keys: &[Pubkey],
) -> Vec<(u32, Pubkey, Pubkey, u64)> {
    meta.inner_instructions
        .iter()
        .flatten()
        .map(|inner| &inner.instruction)
        .filter(|ix| keys[ix.program_id_index as usize] == client::SYSTEM_PROGRAM_ID)
        .filter(|ix| ix.data.len() >= 12)
        .filter_map(|ix| {
            let kind = u32::from_le_bytes(ix.data[..4].try_into().unwrap());
            if kind != 0 && kind != 2 {
                return None;
            }
            Some((
                kind,
                keys[ix.accounts[0] as usize],
                keys[ix.accounts[1] as usize],
                u64::from_le_bytes(ix.data[4..12].try_into().unwrap()),
            ))
        })
        .collect()
}

/// Steps 1 and 3: with fee F, a maximum below F is refused before anything moves (the
/// Anchor `init` rent that runs before the check is reverted with the transaction);
/// a maximum of exactly F pays exactly F to the pinned recipient, the sponsor loses
/// F + Strategy rent and nothing else, and the registrar pays nothing.
#[test]
fn h005_max_is_checked_against_the_amount_transferred() {
    let mut env = Env::new();
    let fee = LISTING_FEE;
    let recipient = env.listing_fee_recipient;
    let strategy_rent = env.svm.minimum_balance_for_rent_exemption(STRATEGY_SPACE);
    let r = new_registration(&mut env);
    let strategy_pda = pda::strategy(&r.program);

    for max in [0, 1, fee - 1] {
        let before = (
            env.lamports(&r.sponsor.pubkey()),
            env.lamports(&r.owner.pubkey()),
            env.lamports(&recipient),
        );
        let ix = register_ix(&env, &r, max);
        assert_fails_with(
            env.send(&[ix], &[&r.owner, &r.sponsor]),
            "ListingFeeAboveMaximum",
        );
        let after = (
            env.lamports(&r.sponsor.pubkey()),
            env.lamports(&r.owner.pubkey()),
            env.lamports(&recipient),
        );
        assert_eq!(
            after, before,
            "H005: refused registration (max {max}) moved lamports"
        );
        assert!(
            env.svm.get_account(&strategy_pda).is_none(),
            "H005: refused registration left a Strategy account"
        );
    }

    let (s0, o0, r0) = (
        env.lamports(&r.sponsor.pubkey()),
        env.lamports(&r.owner.pubkey()),
        env.lamports(&recipient),
    );
    let ix = register_ix(&env, &r, fee);
    let (result, keys) = send_with_keys(&mut env, &[ix], &[&r.owner, &r.sponsor]);
    let meta = result.unwrap_or_else(|e| panic!("register at max = fee: {:?}", e.err));

    assert_eq!(env.lamports(&recipient) - r0, fee, "H005: recipient delta");
    assert_eq!(
        s0 - env.lamports(&r.sponsor.pubkey()),
        fee + strategy_rent,
        "H005: sponsor charged more than fee + Strategy rent"
    );
    assert_eq!(
        env.lamports(&r.owner.pubkey()),
        o0,
        "H005: registrar charged"
    );
    assert_eq!(env.lamports(&strategy_pda), strategy_rent);

    // The only lamport movements out of the sponsor: Strategy creation, then the fee.
    let moves = inner_system_moves(&meta, &keys);
    println!("system moves: {moves:?}");
    assert_eq!(
        moves,
        vec![
            (0, r.sponsor.pubkey(), strategy_pda, strategy_rent),
            (2, r.sponsor.pubkey(), recipient, fee),
        ],
        "H005: unexpected lamport movements"
    );

    let fee_paid: Vec<u64> = events(&meta)
        .into_iter()
        .filter_map(|e| match e {
            PipelineEvent::StrategyRegistered(s) => Some(s.fee_paid),
            _ => None,
        })
        .collect();
    assert_eq!(fee_paid, vec![fee], "H005: event fee_paid");
}

/// Step 2: the admin's fee change and the registration in one transaction. A raise
/// above the maximum before the registration reverts everything (including the raise);
/// the registration first, then a raise to the cap, pays the old fee; a cut first pays
/// the cut fee. The check and the transfer read the same, read-only `config`.
#[test]
fn h005_fee_change_in_the_same_transaction_both_orders() {
    let mut env = Env::new();
    let fee = LISTING_FEE;
    let recipient = env.listing_fee_recipient;
    let strategy_rent = env.svm.minimum_balance_for_rent_exemption(STRATEGY_SPACE);

    // Raise to F + 1 then register with max F: refused, nothing changes.
    let r = new_registration(&mut env);
    let (s0, r0) = (env.lamports(&r.sponsor.pubkey()), env.lamports(&recipient));
    let ixs = [fee_update(&env, fee + 1), register_ix(&env, &r, fee)];
    assert_fails_with(
        env.send(&ixs, &[&r.owner, &r.sponsor]),
        "ListingFeeAboveMaximum",
    );
    assert_eq!(stored_fee(&env), fee, "H005: reverted raise persisted");
    assert_eq!(
        env.lamports(&r.sponsor.pubkey()),
        s0,
        "H005: sponsor charged"
    );
    assert_eq!(env.lamports(&recipient), r0);
    assert!(env.svm.get_account(&pda::strategy(&r.program)).is_none());

    // Register with max F, then raise to the cap: the old fee is paid.
    let ixs = [
        register_ix(&env, &r, fee),
        fee_update(&env, 1_000_000_000_000),
    ];
    env.send(&ixs, &[&r.owner, &r.sponsor])
        .expect("register then raise");
    assert_eq!(
        env.lamports(&recipient) - r0,
        fee,
        "H005: register-then-raise"
    );
    assert_eq!(s0 - env.lamports(&r.sponsor.pubkey()), fee + strategy_rent);
    assert_eq!(stored_fee(&env), 1_000_000_000_000);

    // At the 1,000 SOL fee, a cut to F/2 first and a max of F pays F/2.
    let r = new_registration(&mut env);
    let (s0, r0) = (env.lamports(&r.sponsor.pubkey()), env.lamports(&recipient));
    let ixs = [fee_update(&env, fee / 2), register_ix(&env, &r, fee)];
    env.send(&ixs, &[&r.owner, &r.sponsor])
        .expect("cut then register");
    assert_eq!(
        env.lamports(&recipient) - r0,
        fee / 2,
        "H005: cut-then-register"
    );
    assert_eq!(
        s0 - env.lamports(&r.sponsor.pubkey()),
        fee / 2 + strategy_rent
    );
}

/// Step 4: fees between 1 and rent_min(0) - 1 can't be set; a fee of exactly rent_min(0)
/// is payable to a recipient that doesn't exist yet; a zero fee makes no transfer and
/// the recipient stays nonexistent. Even at fee 0 the recipient must be the configured
/// one and writable (the H027 surface).
#[test]
fn h005_rent_floor_and_zero_fee() {
    let mut env = Env::new();
    let rent0 = env.svm.minimum_balance_for_rent_exemption(0);
    let strategy_rent = env.svm.minimum_balance_for_rent_exemption(STRATEGY_SPACE);

    for bad in [1, rent0 - 1] {
        let ix = fee_update(&env, bad);
        assert_fails_with(env.send(&[ix], &[]), "ListingFeeBelowRent");
        assert_eq!(stored_fee(&env), LISTING_FEE);
    }

    // Fee = rent_min(0) into a fresh recipient.
    let fresh = Pubkey::new_unique();
    let ix = client::update_config(
        &env.admin.pubkey(),
        &client::UpdateConfigArgs {
            listing_fee_lamports: Some(rent0),
            listing_fee_recipient: Some(fresh),
            ..Default::default()
        },
    );
    env.send(&[ix], &[]).expect("fee = rent_min(0)");
    env.listing_fee_recipient = fresh;
    let r = new_registration(&mut env);
    let s0 = env.lamports(&r.sponsor.pubkey());
    let ix = register_ix(&env, &r, rent0);
    env.send(&[ix], &[&r.owner, &r.sponsor])
        .expect("register at fee = rent_min(0)");
    assert_eq!(env.lamports(&fresh), rent0);
    assert_eq!(
        s0 - env.lamports(&r.sponsor.pubkey()),
        rent0 + strategy_rent
    );

    // Fee 0 into another fresh recipient.
    let fresh = Pubkey::new_unique();
    let ix = client::update_config(
        &env.admin.pubkey(),
        &client::UpdateConfigArgs {
            listing_fee_lamports: Some(0),
            listing_fee_recipient: Some(fresh),
            ..Default::default()
        },
    );
    env.send(&[ix], &[]).expect("fee = 0");
    env.listing_fee_recipient = fresh;

    // Wrong or read-only recipient is refused even when nothing is to be paid.
    let r = new_registration(&mut env);
    let mut wrong = register_ix(&env, &r, 0);
    wrong.accounts[5].pubkey = Pubkey::new_unique();
    assert_fails_with(
        env.send(&[wrong], &[&r.owner, &r.sponsor]),
        "WrongFeeRecipient",
    );
    let mut read_only = register_ix(&env, &r, 0);
    read_only.accounts[5].is_writable = false;
    assert_fails_with(
        env.send(&[read_only], &[&r.owner, &r.sponsor]),
        "ConstraintMut",
    );

    let s0 = env.lamports(&r.sponsor.pubkey());
    let ix = register_ix(&env, &r, 0);
    let (result, keys) = send_with_keys(&mut env, &[ix], &[&r.owner, &r.sponsor]);
    let meta = result.unwrap_or_else(|e| panic!("register at fee 0: {:?}", e.err));
    assert!(
        env.svm.get_account(&fresh).is_none(),
        "H005: fee 0 created the recipient"
    );
    assert_eq!(s0 - env.lamports(&r.sponsor.pubkey()), strategy_rent);
    let to_recipient: Vec<_> = inner_system_moves(&meta, &keys)
        .into_iter()
        .filter(|(_, _, to, _)| *to == fresh)
        .collect();
    assert!(
        to_recipient.is_empty(),
        "H005: fee 0 transferred {to_recipient:?}"
    );
}

/// The recipient can't be swapped by the transaction builder: a payer whose
/// transaction names another recipient is refused at any fee, so the maximum caps a
/// payment that can only go to `config.listing_fee_recipient`.
#[test]
fn h005_recipient_is_pinned() {
    let mut env = Env::new();
    let r = new_registration(&mut env);
    let s0 = env.lamports(&r.sponsor.pubkey());
    let attacker = Keypair::new();
    let mut ix = register_ix(&env, &r, u64::MAX);
    ix.accounts[5].pubkey = attacker.pubkey();
    assert_fails_with(
        env.send(&[ix], &[&r.owner, &r.sponsor]),
        "WrongFeeRecipient",
    );
    assert_eq!(env.lamports(&r.sponsor.pubkey()), s0);
    assert_eq!(env.lamports(&attacker.pubkey()), 0);
}

/// Crafted state (`set_account` on the Rent sysvar): `ListingFeeBelowRent` is checked only
/// when the fee is set. If Rent later rose, a stored fee of the old rent_min(0) would be
/// below the new one, and paying it into a recipient that doesn't exist yet would fail.
/// Shows the consequence is a refused registration (nothing moves), never an
/// overcharge; an existing recipient still receives exactly the fee. Mainnet Rent has
/// never risen, so this is a liveness note only.
#[test]
fn h005_rent_rise_after_fee_set_only_refuses_crafted() {
    let mut env = Env::new();
    let old_rent0 = env.svm.minimum_balance_for_rent_exemption(0);
    let fresh = Pubkey::new_unique();
    let ix = client::update_config(
        &env.admin.pubkey(),
        &client::UpdateConfigArgs {
            listing_fee_lamports: Some(old_rent0),
            listing_fee_recipient: Some(fresh),
            ..Default::default()
        },
    );
    env.send(&[ix], &[]).expect("fee = rent_min(0)");
    env.listing_fee_recipient = fresh;
    let r = new_registration(&mut env);

    // Double lamports_per_byte_year (Rent = u64, f64 threshold, u8 burn percent).
    let rent_id = client::pubkey!("SysvarRent111111111111111111111111111111111");
    let mut rent = env.svm.get_account(&rent_id).expect("rent sysvar");
    let per_byte_year = u64::from_le_bytes(rent.data[..8].try_into().unwrap());
    rent.data[..8].copy_from_slice(&(2 * per_byte_year).to_le_bytes());
    env.svm.set_account(rent_id, rent).unwrap();
    let new_rent0 = env.svm.minimum_balance_for_rent_exemption(0);
    println!("rent_min(0): {old_rent0} -> {new_rent0}");
    assert!(new_rent0 > old_rent0, "crafted Rent rise took effect");

    let s0 = env.lamports(&r.sponsor.pubkey());
    let ix = register_ix(&env, &r, old_rent0);
    let result = env.send(&[ix], &[&r.owner, &r.sponsor]);
    if let Err(failed) = &result {
        println!("fresh recipient: {:?}", failed.err);
    }
    assert!(
        result.is_err(),
        "sub-rent payment into a fresh recipient fails"
    );
    assert_eq!(
        env.lamports(&r.sponsor.pubkey()),
        s0,
        "H005: refused but charged"
    );
    assert!(env.svm.get_account(&fresh).is_none());

    // Once the recipient exists, the same stored fee is paid exactly.
    env.svm.airdrop(&fresh, new_rent0).unwrap();
    let strategy_rent = env.svm.minimum_balance_for_rent_exemption(STRATEGY_SPACE);
    let ix = register_ix(&env, &r, old_rent0);
    env.send(&[ix], &[&r.owner, &r.sponsor])
        .expect("register into an existing recipient");
    assert_eq!(env.lamports(&fresh), new_rent0 + old_rent0);
    assert_eq!(
        s0 - env.lamports(&r.sponsor.pubkey()),
        old_rent0 + strategy_rent
    );
}
