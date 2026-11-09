//! SOS audit #2, H046: a "freeze" that hands the router's upgrade authority to a burn
//! key (instead of `SetAuthority(None)`) keeps the pause power.
//!
//! The freeze test is `upgrade_authority_address.is_none()`. A burn key is a `Some`
//! authority, so the router still counts as upgradeable: `update_config(paused)` is
//! accepted and `lift_pause` refuses. This is the documented definition of frozen
//! (an indexer would report such a program as upgradeable), so the
//! test characterises the behaviour and documents the runbook requirement. It does not
//! assert a safety property the code is meant to have.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_signer::Signer;

/// Loader-v3 `SetAuthority` (instruction 4) on the router's ProgramData.
fn loader_set_authority(current: &Pubkey, new: Option<&Pubkey>) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new(pda::program_data(&client::PROGRAM_ID), false),
        AccountMeta::new_readonly(*current, true),
    ];
    if let Some(new) = new {
        accounts.push(AccountMeta::new_readonly(*new, false));
    }
    Instruction::new_with_bytes(
        client::BPF_LOADER_UPGRADEABLE_ID,
        &4u32.to_le_bytes(),
        accounts,
    )
}

fn pause_ix(admin: &Pubkey, paused: bool) -> Instruction {
    client::update_config(
        admin,
        &client::UpdateConfigArgs {
            paused: Some(paused),
            ..Default::default()
        },
    )
}

/// The "freeze" by burn key: the authority is a key nobody holds. The router can't be
/// upgraded by anyone, yet `paused = true` still succeeds and `lift_pause` refuses,
/// so the admin keeps a pause power that the freeze was meant to end.
#[test]
fn h046_burn_key_authority_keeps_pause_power() {
    let mut env = Env::new();
    let admin = env.admin.pubkey();
    let burn = Pubkey::new_unique();

    env.send(&[loader_set_authority(&admin, Some(&burn))], &[])
        .expect("setup: real loader SetAuthority to a burn key");
    let program_data = env.account_data(&pda::program_data(&client::PROGRAM_ID));
    assert_eq!(program_data[12], 1, "setup: authority is still Some(burn)");

    env.send(&[pause_ix(&admin, true)], &[])
        .expect("characterisation: a burn-key authority still counts as upgradeable");
    assert!(
        client::decode_config(&env.account_data(&pda::config()))
            .expect("config")
            .paused
    );

    assert_fails_with(env.send(&[client::lift_pause()], &[]), "RouterNotFrozen");

    env.send(&[pause_ix(&admin, false)], &[])
        .expect("the admin can still unpause, so the runbook must use SetAuthority(None)");
}
