//! SOS audit #2, H051: a third party registers a program through its `["pipeline-register"]`
//! PDA path, which the program's own code signs for. The test checks what that registrant
//! gains and whether the upgrade authority stays in control of the listing.
//!
//! Safe behaviour pinned here:
//!   * the registrant gets no stored privilege (`Strategy` has no owner field), only the
//!     listing and the metadata URI until the authority overwrites it;
//!   * the listing fee is paid by the registrant to the configured recipient;
//!   * the upgrade authority can still overwrite the metadata after the third-party
//!     registration, and after a freeze the program's own PDA path (open to anyone the
//!     program lets in) still works for the owner;
//!   * the authority cannot re-register a program that is already registered.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_keypair::Keypair;
use solana_signer::Signer;

const EVIL: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

fn uri_of(env: &Env, program: &Pubkey) -> String {
    client::decode_strategy(&env.account_data(&pda::strategy(program)))
        .expect("strategy account")
        .metadata_uri
}

/// Sends `inner` through EVIL's `FORWARD_TO_ROUTER`, which signs `["pipeline-register"]`.
/// `signers` are the outer transaction's signers (the PDA is never one of them).
fn forward_via(
    env: &mut Env,
    signer_pda: &Pubkey,
    inner: Instruction,
    signers: &[&Keypair],
) -> TxResult {
    let mut accounts = vec![AccountMeta::new_readonly(client::PROGRAM_ID, false)];
    accounts.extend(inner.accounts.iter().map(|meta| AccountMeta {
        is_signer: meta.is_signer && meta.pubkey != *signer_pda,
        ..meta.clone()
    }));
    let mut data = b"evilfwd!".to_vec();
    data.extend_from_slice(&inner.data);
    let forward = Instruction::new_with_bytes(EVIL, &data, accounts);
    env.send(&[forward], signers)
}

#[test]
fn h051_third_party_pda_registration_leaves_the_authority_in_control() {
    let mut env = Env::new();
    let evil_owner = env.funded_keypair();
    let attacker = env.funded_keypair();
    env.deploy_strategy(&EVIL, "test_strategy_evil", &evil_owner.pubkey());
    let evil_pda = pda::register(&EVIL);
    let recipient = env.listing_fee_recipient;

    // (1) The attacker registers EVIL through EVIL's PDA. The attacker pays the fee.
    let recipient_before = env.lamports(&recipient);
    let inner = client::register_strategy(
        &attacker.pubkey(),
        &evil_pda,
        &EVIL,
        &recipient,
        "attacker-uri",
        u64::MAX,
    );
    forward_via(&mut env, &evil_pda, inner, &[&attacker]).expect("third-party PDA registration");
    assert_eq!(uri_of(&env, &EVIL), "attacker-uri");
    assert_eq!(env.lamports(&recipient) - recipient_before, LISTING_FEE);

    // (2) The authority cannot register again: the strategy account already exists.
    let again = client::register_strategy(
        &evil_owner.pubkey(),
        &evil_owner.pubkey(),
        &EVIL,
        &recipient,
        "owner-uri",
        u64::MAX,
    );
    let result = env.send(&[again], &[&evil_owner]);
    assert!(result.is_err(), "second registration must fail");
    assert_eq!(uri_of(&env, &EVIL), "attacker-uri");

    // (3) The authority overwrites the metadata while still upgradeable.
    let update = client::update_metadata(&evil_owner.pubkey(), &EVIL, "owner-uri");
    env.send(&[update], &[&evil_owner])
        .expect("authority updates metadata");
    assert_eq!(uri_of(&env, &EVIL), "owner-uri");

    // (4) After the authority freezes the program, the authority path is gone, but the
    // program's own PDA path still works for the owner: the owner is not locked out.
    set_upgrade_authority(&mut env.svm, &EVIL, None);
    let stale = client::update_metadata(&evil_owner.pubkey(), &EVIL, "frozen-owner");
    assert_fails_with(env.send(&[stale], &[&evil_owner]), "NotStrategyOwner");
    let inner = client::update_metadata(&evil_pda, &EVIL, "frozen-by-owner-via-pda");
    forward_via(&mut env, &evil_pda, inner, &[]).expect("owner uses the PDA path");
    assert_eq!(uri_of(&env, &EVIL), "frozen-by-owner-via-pda");
}
