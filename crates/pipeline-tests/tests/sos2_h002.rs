//! SOS audit #2, H002: can a loader-v3 → loader-v4 migration make a pause outlive the
//! router's upgradeability?
//!
//! `UpdateConfig` and `LiftPause` both bind `router_data` with
//! `router.programdata_address()? == Some(router_data.key())` and load it as
//! `Account<ProgramData>`. Anchor returns `Ok(None)` for any Program account not owned
//! by loader-v3, so if the router ever stopped being a loader-v3 program both
//! instructions would be unsatisfiable.
//!
//! The question is whether anything can move the router off loader-v3. Agave 3.1/4.0
//! had a loader-v3 `Migrate` instruction (feature `enable_loader_v4`). From Agave 4.3
//! (the clusters run 4.4) loader-v4 is abandoned: the `Migrate` handler and variant are
//! gone, the loader-v4 builtin is gone, and the feature ID is the placeholder
//! `LoaderV4WasAbandoned1111…`. LiteSVM 0.18.2 runs that same 4.3.2 runtime.
//!
//! These tests settle:
//! 1. the runtime rejects `Migrate` (tag 8) outright, even from the upgrade authority;
//! 2. a program owned by loader-v4 can't execute at all on this runtime, so the
//!    "migrated, still trading, can't unpause" state doesn't exist;
//! 3. (characterisation, crafted state) if the router were somehow moved to a loader
//!    other than loader-v3, `update_config` and `lift_pause` would indeed both be
//!    unsatisfiable for every candidate `router_data`, and a pause would stay in place.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_account::Account;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const LOADER_V2: Pubkey = client::pubkey!("BPFLoader2111111111111111111111111111111111");
const LOADER_V4: Pubkey = client::pubkey!("LoaderV411111111111111111111111111111111111");
const SYSTEM: Pubkey = client::pubkey!("11111111111111111111111111111111");

/// Bincode tag of `UpgradeableLoaderInstruction::Migrate` in loader-v3-interface ≤ 6.x.
const MIGRATE_TAG: u32 = 8;

fn config(env: &Env) -> client::Config {
    client::decode_config(&env.account_data(&pda::config())).expect("config")
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

/// Router ELF as deployed by `Env::new`.
fn router_elf() -> Vec<u8> {
    std::fs::read(sbf_dir().join("pipeline.so")).expect("pipeline.so")
}

/// The loader-v3 `Migrate` instruction as agave 3.1.8 / 4.0.0 defined it.
fn migrate_ix(program: &Pubkey, authority: &Pubkey) -> Instruction {
    Instruction::new_with_bytes(
        client::BPF_LOADER_UPGRADEABLE_ID,
        &MIGRATE_TAG.to_le_bytes(),
        vec![
            AccountMeta::new(pda::program_data(program), false),
            AccountMeta::new(*program, false),
            AccountMeta::new_readonly(*authority, true),
        ],
    )
}

/// Rewrites the router as agave 3.1.8 `Migrate` would leave it, except that the
/// Program account's new owner is `owner` with `header` before the ELF. The ProgramData
/// is drained and emptied, and its lamports move to the Program account.
fn hand_migrate_router(env: &mut Env, owner: Pubkey, header: Vec<u8>) {
    let program_data = pda::program_data(&client::PROGRAM_ID);
    let pd = env
        .svm
        .get_account(&program_data)
        .expect("router ProgramData");
    let program = env.svm.get_account(&client::PROGRAM_ID).expect("router");
    let mut data = header;
    data.extend_from_slice(&router_elf());
    env.svm
        .set_account(
            client::PROGRAM_ID,
            Account {
                lamports: program.lamports + pd.lamports,
                data,
                owner,
                executable: true,
                rent_epoch: 0,
            },
        )
        .expect("rewrite router Program account");
    env.svm
        .set_account(
            program_data,
            Account {
                lamports: 0,
                data: vec![],
                owner: SYSTEM,
                executable: false,
                rent_epoch: 0,
            },
        )
        .expect("empty router ProgramData");
}

/// `LoaderV4State { slot: u64, authority_address_or_next_version: Pubkey, status: u64 }`
/// with status 2 = Finalized, as `Migrate` leaves a frozen program.
fn loader_v4_finalized_header() -> Vec<u8> {
    let mut header = Vec::with_capacity(48);
    header.extend_from_slice(&0u64.to_le_bytes());
    header.extend_from_slice(client::PROGRAM_ID.as_ref());
    header.extend_from_slice(&2u64.to_le_bytes());
    header
}

/// Replaces the `router_data` account meta (index `index`) of a router instruction.
fn with_router_data(mut ix: Instruction, index: usize, router_data: Pubkey) -> Instruction {
    assert_eq!(
        ix.accounts[index].pubkey,
        pda::program_data(&client::PROGRAM_ID),
        "index {index} is router_data"
    );
    ix.accounts[index].pubkey = router_data;
    ix
}

/// Property 1 (settled by test): the current runtime has no loader-v3 `Migrate`. Even
/// the router's upgrade authority, signing, gets `InvalidInstructionData`, the router
/// stays a loader-v3 program, and the pause/freeze/`lift_pause` cycle still works.
#[test]
fn the_runtime_rejects_loader_v3_migrate_even_from_the_upgrade_authority() {
    let mut env = Env::new();
    let admin = env.admin.pubkey();
    env.send(&[pause_ix(&admin, true)], &[])
        .expect("setup: pause while upgradeable");
    assert!(config(&env).paused, "setup: paused");

    let router_before = env.svm.get_account(&client::PROGRAM_ID).unwrap();
    let pd_before = env
        .svm
        .get_account(&pda::program_data(&client::PROGRAM_ID))
        .unwrap();

    let result = env.send(&[migrate_ix(&client::PROGRAM_ID, &admin)], &[]);
    match &result {
        Ok(meta) => panic!(
            "H002: loader-v3 Migrate succeeded on this runtime:\n{}",
            meta.logs.join("\n")
        ),
        Err(failed) => println!("Migrate signed by the upgrade authority: {:?}", failed.err),
    }
    assert_fails_with(result, "InvalidInstructionData");

    let router_after = env.svm.get_account(&client::PROGRAM_ID).unwrap();
    let pd_after = env
        .svm
        .get_account(&pda::program_data(&client::PROGRAM_ID))
        .unwrap();
    assert_eq!(router_after.owner, client::BPF_LOADER_UPGRADEABLE_ID);
    assert_eq!(router_after.data, router_before.data);
    assert_eq!(pd_after.data, pd_before.data);
    assert_eq!(pd_after.lamports, pd_before.lamports);

    // The freeze-aware pause is intact: freeze, then anyone lifts the pause.
    set_upgrade_authority(&mut env.svm, &client::PROGRAM_ID, None);
    let anyone = env.funded_keypair();
    let ix = client::lift_pause();
    env.svm.expire_blockhash();
    let tx = solana_transaction::Transaction::new_signed_with_payer(
        &[ix],
        Some(&anyone.pubkey()),
        &[&anyone],
        env.svm.latest_blockhash(),
    );
    env.svm
        .send_transaction(tx)
        .expect("H002: lift_pause by anyone after the freeze");
    assert!(!config(&env).paused, "H002: pause lifted after the freeze");
    println!("router still loader-v3; frozen + lift_pause by a stranger cleared the pause");
}

/// Property 2 (settled by crafted state): the exact layout `Migrate` would leave (Program
/// account owned by loader-v4 with a Finalized header, ProgramData emptied) can't run on
/// this runtime at all: there is no loader-v4 builtin, so every router instruction fails
/// with `UnsupportedProgramId`. The hypothesised "migrated but still serving a permanent
/// pause" state does not exist on Agave ≥ 4.3.
#[test]
fn a_loader_v4_owned_router_cannot_execute_on_this_runtime() {
    let mut env = Env::new();
    let admin = env.admin.pubkey();
    env.send(&[pause_ix(&admin, true)], &[])
        .expect("setup: pause while upgradeable");

    // The 4.3.2 runtime registers loader-v3 as a builtin but not loader-v4.
    assert!(
        env.svm
            .get_account(&client::BPF_LOADER_UPGRADEABLE_ID)
            .is_some_and(|a| a.executable),
        "loader-v3 builtin present"
    );
    assert!(
        env.svm.get_account(&LOADER_V4).is_none(),
        "this runtime registers no loader-v4 builtin"
    );
    // LiteSVM unwraps the owner account before executing, where Agave only checks the
    // owner against its loader list. Give loader-v4 an executable native-loader account
    // with no builtin behind it, which is what an Agave ≥ 4.3 validator effectively has.
    env.svm
        .set_account(
            LOADER_V4,
            Account {
                lamports: 1,
                data: b"loader_v4 (no builtin)".to_vec(),
                owner: client::pubkey!("NativeLoader1111111111111111111111111111111"),
                executable: true,
                rent_epoch: 0,
            },
        )
        .expect("setup: loader-v4 placeholder");

    hand_migrate_router(&mut env, LOADER_V4, loader_v4_finalized_header());
    assert_eq!(
        env.svm.get_account(&client::PROGRAM_ID).unwrap().owner,
        LOADER_V4,
        "setup: router owned by loader-v4"
    );

    let lift = env.send(&[client::lift_pause()], &[]);
    let unpause = env.send(&[pause_ix(&admin, false)], &[]);
    for (name, result) in [("lift_pause", lift), ("update_config", unpause)] {
        match &result {
            Ok(meta) => panic!(
                "H002: {name} ran on a loader-v4 router:\n{}",
                meta.logs.join("\n")
            ),
            Err(failed) => println!("{name} on a loader-v4 router: {:?}", failed.err),
        }
        assert_fails_with(result, "UnsupportedProgramId");
    }
}

/// Property 3 (characterisation, crafted state). Moves the router to a loader this
/// runtime can execute (loader-v2) with its ProgramData emptied. For the router's code
/// this is the same branch as loader-v4: `programdata_address()` returns `Ok(None)` for
/// every owner other than loader-v3. Every candidate `router_data` is then rejected by
/// both `update_config` and `lift_pause`, settings can't change, and the pause stays.
///
/// This documents the router's dependency on loader-v3. It is not an exploitable state
/// today, because nothing on the current runtime can move the router off loader-v3
/// (see the two tests above).
#[test]
fn off_loader_v3_the_router_admin_and_lift_pause_are_unsatisfiable() {
    let mut env = Env::new();
    let admin = env.admin.insecure_clone();
    let cp = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000_000, 1_000_000_000, 30);
    let (trader, trader_a) = env.trader_with(&cp.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &cp.mint_b, &TOKEN);
    let accounts = cp_swap_accounts(
        &cp,
        &trader.pubkey(),
        &cp.mint_a,
        &trader_a,
        &cp.mint_b,
        &trader_b,
    );
    env.send(
        &[client::swap(&accounts, &exact_in(1_000_000, 0))],
        &[&trader],
    )
    .expect("setup: swap before the pause");
    env.send(&[pause_ix(&admin.pubkey(), true)], &[])
        .expect("setup: pause while upgradeable");
    let before = config(&env);
    assert!(before.paused, "setup: paused");

    hand_migrate_router(&mut env, LOADER_V2, vec![]);
    // The router still runs (it rejects the swap with its own error, not the runtime's).
    assert_fails_with(
        env.send(
            &[client::swap(&accounts, &exact_in(1_000_000, 0))],
            &[&trader],
        ),
        "Paused",
    );

    // Candidate router_data accounts: the old (emptied) ProgramData, the router itself,
    // and a genuine loader-v3 ProgramData belonging to another program (the CP).
    let candidates = [
        ("old ProgramData", pda::program_data(&client::PROGRAM_ID)),
        ("router account", client::PROGRAM_ID),
        ("CP ProgramData", pda::program_data(&CP_PROGRAM_ID)),
    ];
    let rotate = client::update_config(
        &admin.pubkey(),
        &client::UpdateConfigArgs {
            treasury_owner: Some(Pubkey::new_unique()),
            ..Default::default()
        },
    );
    for (label, router_data) in candidates {
        let attempts = [
            (
                "update_config(paused=false)",
                with_router_data(pause_ix(&admin.pubkey(), false), 3, router_data),
            ),
            (
                "update_config(treasury_owner)",
                with_router_data(rotate.clone(), 3, router_data),
            ),
            (
                "lift_pause",
                with_router_data(client::lift_pause(), 2, router_data),
            ),
        ];
        for (name, ix) in attempts {
            let result = env.send(&[ix], &[]);
            match &result {
                Ok(meta) => panic!(
                    "{name} with router_data = {label} unexpectedly succeeded:\n{}",
                    meta.logs.join("\n")
                ),
                Err(failed) => println!("{name}, router_data = {label}: {:?}", failed.err),
            }
        }
    }
    assert_eq!(config(&env), before, "no setting changed");
    assert_fails_with(
        env.send(
            &[client::swap(&accounts, &exact_in(1_000_000, 0))],
            &[&trader],
        ),
        "Paused",
    );
    println!("off loader-v3: no router_data satisfies UpdateConfig or LiftPause; pause stays");
}
