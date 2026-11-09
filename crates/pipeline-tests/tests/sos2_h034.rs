//! SOS audit #2, H034: loader-v4 strategies can never register once the router is
//! frozen, and a strategy migrated to loader-v4 loses `update_metadata`.
//!
//! Context (H002, H026): `check_registrar` accepts loader-v3 programs only
//! (`lib.rs:840-877`). Loader-v4 was abandoned in Agave 4.3 (no builtin, no `Migrate`),
//! so the only loader other than v3 that can actually run programs is loader-v2.
//!
//! Properties pinned here:
//!   * (setting, settled by crafted `set_account` state) a program owned by loader-v4,
//!     in a fresh or a post-`Migrate` layout, is refused by `register_strategy` with
//!     `UnsupportedLoader`; the PDA path is doubly blocked because the program cannot
//!     execute to sign;
//!   * (crafted state) a loader-v2 program, which does execute, is refused on both the
//!     authority path and the `["pipeline-register"]` PDA path with `UnsupportedLoader`;
//!   * (crafted state) a registered strategy whose program is re-owned to loader-v4 or
//!     loader-v2 loses `update_metadata` (`UnsupportedLoader`), keeps its Strategy
//!     account and does not affect any other strategy; restoring loader-v3 restores it;
//!   * (real runtime) the premise is not reachable: loader-v3 `Migrate` is rejected by
//!     the runtime and a loader-v4-owned program cannot execute.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

const CP: Pubkey = CP_PROGRAM_ID;
const EVIL: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
const LOADER_V2: Pubkey = client::pubkey!("BPFLoader2111111111111111111111111111111111");
const LOADER_V4: Pubkey = client::pubkey!("LoaderV411111111111111111111111111111111111");
const NATIVE_LOADER: Pubkey = client::pubkey!("NativeLoader1111111111111111111111111111111");
const MIGRATE_TAG: u32 = 8;

fn elf(so_name: &str) -> Vec<u8> {
    std::fs::read(sbf_dir().join(format!("{so_name}.so"))).expect("strategy .so")
}

/// `LoaderV4State { slot, authority_address_or_next_version, status }`, 2 = Finalized.
fn loader_v4_header(authority: &Pubkey, status: u64) -> Vec<u8> {
    let mut header = 0u64.to_le_bytes().to_vec();
    header.extend_from_slice(authority.as_ref());
    header.extend_from_slice(&status.to_le_bytes());
    header
}

/// The loader-v4 account the 4.3.2 runtime lacks: an executable native-loader account
/// with no builtin behind it, as in H002, so owner lookups behave like Agave's.
fn add_loader_v4_placeholder(env: &mut Env) {
    env.svm
        .set_account(
            LOADER_V4,
            Account {
                lamports: 1,
                data: b"loader_v4 (no builtin)".to_vec(),
                owner: NATIVE_LOADER,
                executable: true,
                rent_epoch: 0,
            },
        )
        .unwrap();
}

/// Re-owns a deployed program the way a loader migration would leave it: the ELF under
/// `owner` (behind `header`), and optionally the loader-v3 ProgramData emptied.
fn reown(
    env: &mut Env,
    program: &Pubkey,
    so_name: &str,
    owner: Pubkey,
    header: Vec<u8>,
    drain_pd: bool,
) {
    let mut data = header;
    data.extend_from_slice(&elf(so_name));
    env.svm
        .set_account(
            *program,
            Account {
                lamports: 10 * SOL,
                data,
                owner,
                executable: true,
                rent_epoch: 0,
            },
        )
        .unwrap();
    if drain_pd {
        env.svm
            .set_account(pda::program_data(program), Account::default())
            .unwrap();
    }
}

fn uri_of(env: &Env, program: &Pubkey) -> String {
    client::decode_strategy(&env.account_data(&pda::strategy(program)))
        .expect("strategy account")
        .metadata_uri
}

fn register(env: &mut Env, registrar: &Keypair, program: &Pubkey) -> TxResult {
    let payer = env.admin.pubkey();
    let ix = client::register_strategy(
        &payer,
        &registrar.pubkey(),
        program,
        &env.listing_fee_recipient,
        "uri",
        u64::MAX,
    );
    env.send(&[ix], &[registrar])
}

fn update(env: &mut Env, registrar: &Keypair, program: &Pubkey, uri: &str) -> TxResult {
    let ix = client::update_metadata(&registrar.pubkey(), program, uri);
    env.send(&[ix], &[registrar])
}

/// Sends `inner` (a router instruction whose registrar is `signer_pda`) through the
/// evil strategy's `FORWARD_TO_ROUTER`, which signs `["pipeline-register"]`.
fn forward_via(
    env: &mut Env,
    program: &Pubkey,
    signer_pda: &Pubkey,
    inner: Instruction,
) -> TxResult {
    let mut accounts = vec![AccountMeta::new_readonly(client::PROGRAM_ID, false)];
    accounts.extend(inner.accounts.iter().map(|meta| AccountMeta {
        is_signer: meta.is_signer && meta.pubkey != *signer_pda,
        ..meta.clone()
    }));
    let mut data = b"evilfwd!".to_vec();
    data.extend_from_slice(&inner.data);
    let forward = Instruction::new_with_bytes(*program, &data, accounts);
    env.send(&[forward], &[])
}

fn fresh_cp() -> (Env, Keypair) {
    let mut env = Env::new();
    let owner = env.funded_keypair();
    env.deploy_strategy(&CP, "test_strategy_cp", &owner.pubkey());
    (env, owner)
}

// --------------------------------------------------------------------------------------
// Registration of non-loader-v3 programs
// --------------------------------------------------------------------------------------

/// Loader-v4 programs can't register on the authority path in any layout, including the
/// exact post-`Migrate` shape (Finalized header, ProgramData emptied).
#[test]
fn h034_loader_v4_program_cannot_register_by_authority() {
    let cases: Vec<(&str, Vec<u8>, bool)> = vec![
        (
            "v4 Deployed, v3 ProgramData left behind",
            loader_v4_header(&Pubkey::new_unique(), 1),
            false,
        ),
        (
            "v4 Finalized, v3 ProgramData emptied (post-Migrate)",
            loader_v4_header(&CP, 2),
            true,
        ),
    ];
    for (name, header, drain) in cases {
        let (mut env, owner) = fresh_cp();
        add_loader_v4_placeholder(&mut env);
        reown(&mut env, &CP, "test_strategy_cp", LOADER_V4, header, drain);
        assert_eq!(
            env.svm.get_account(&CP).unwrap().owner,
            LOADER_V4,
            "setup: {name}"
        );
        assert_fails_with(register(&mut env, &owner, &CP), "UnsupportedLoader");
        assert!(env.svm.get_account(&pda::strategy(&CP)).is_none(), "{name}");
        eprintln!("ok (UnsupportedLoader): {name}");
    }
}

/// On the PDA path a loader-v4 program is blocked twice: it cannot execute on this
/// runtime, so it can never sign its PDA, and the router would refuse it anyway.
#[test]
fn h034_loader_v4_program_cannot_register_by_pda() {
    let mut env = Env::new();
    let owner = env.funded_keypair();
    env.deploy_strategy(&EVIL, "test_strategy_evil", &owner.pubkey());
    add_loader_v4_placeholder(&mut env);
    reown(
        &mut env,
        &EVIL,
        "test_strategy_evil",
        LOADER_V4,
        loader_v4_header(&EVIL, 2),
        true,
    );
    assert_eq!(
        env.svm.get_account(&EVIL).unwrap().owner,
        LOADER_V4,
        "setup"
    );

    let payer = env.admin.pubkey();
    let evil_pda = pda::register(&EVIL);
    let recipient = env.listing_fee_recipient;
    let inner = client::register_strategy(&payer, &evil_pda, &EVIL, &recipient, "x", u64::MAX);
    assert_fails_with(
        forward_via(&mut env, &EVIL, &evil_pda, inner),
        "UnsupportedProgramId",
    );
    assert!(env.svm.get_account(&pda::strategy(&EVIL)).is_none());
}

/// Loader-v2 is the one non-v3 loader whose programs still execute. A v2 program is
/// excluded on both paths. (Loader-v2 programs are immutable by nature, so the PDA path
/// would be the natural one for them.)
#[test]
fn h034_loader_v2_program_cannot_register_on_either_path() {
    // Authority path: any signer, ProgramData left as the old loader-v3 one.
    let (mut env, owner) = fresh_cp();
    reown(&mut env, &CP, "test_strategy_cp", LOADER_V2, vec![], false);
    assert_eq!(env.svm.get_account(&CP).unwrap().owner, LOADER_V2, "setup");
    assert_fails_with(register(&mut env, &owner, &CP), "UnsupportedLoader");
    assert!(env.svm.get_account(&pda::strategy(&CP)).is_none());

    // PDA path: the v2 program runs and signs its PDA, and the router still refuses it.
    let mut env = Env::new();
    let owner = env.funded_keypair();
    env.deploy_strategy(&EVIL, "test_strategy_evil", &owner.pubkey());
    reown(
        &mut env,
        &EVIL,
        "test_strategy_evil",
        LOADER_V2,
        vec![],
        true,
    );
    assert_eq!(
        env.svm.get_account(&EVIL).unwrap().owner,
        LOADER_V2,
        "setup"
    );
    let payer = env.admin.pubkey();
    let evil_pda = pda::register(&EVIL);
    let recipient = env.listing_fee_recipient;
    let inner = client::register_strategy(&payer, &evil_pda, &EVIL, &recipient, "x", u64::MAX);
    assert_fails_with(
        forward_via(&mut env, &EVIL, &evil_pda, inner),
        "UnsupportedLoader",
    );
    assert!(env.svm.get_account(&pda::strategy(&EVIL)).is_none());
}

// --------------------------------------------------------------------------------------
// update_metadata after a program is re-owned
// --------------------------------------------------------------------------------------

/// A registered strategy whose program is later re-owned loses `update_metadata`, but
/// its Strategy account and every other strategy stay intact, and restoring the loader-v3
/// owner brings the authority back.
#[test]
fn h034_reowned_strategy_loses_update_metadata_only() {
    let mut env = Env::new();
    let other_owner = env.funded_keypair();
    env.deploy_strategy(&EVIL, "test_strategy_evil", &other_owner.pubkey());
    let cp_owner = env.register(&CP, "test_strategy_cp");
    register(&mut env, &other_owner, &EVIL).expect("setup: second strategy");
    assert_eq!(uri_of(&env, &CP), "https://example.com/strategy.json");
    update(&mut env, &cp_owner, &CP, "before").expect("setup: update works on loader-v3");

    let original = env.svm.get_account(&CP).unwrap();
    let original_pd = env.svm.get_account(&pda::program_data(&CP)).unwrap();
    add_loader_v4_placeholder(&mut env);

    let reowned: Vec<(&str, Pubkey, Vec<u8>)> = vec![
        (
            "loader-v4 (post-Migrate)",
            LOADER_V4,
            loader_v4_header(&CP, 2),
        ),
        ("loader-v2", LOADER_V2, vec![]),
    ];
    for (name, loader, header) in reowned {
        reown(&mut env, &CP, "test_strategy_cp", loader, header, true);
        assert_fails_with(
            update(&mut env, &cp_owner, &CP, "after"),
            "UnsupportedLoader",
        );
        assert_eq!(uri_of(&env, &CP), "before", "{name}: metadata unchanged");
        // The strategy account is still there and other strategies are unaffected.
        update(&mut env, &other_owner, &EVIL, "evil-ok").expect("other strategy unaffected");
        assert_eq!(uri_of(&env, &EVIL), "evil-ok");
        eprintln!("ok (UnsupportedLoader on update_metadata): {name}");
    }

    // Restoring the loader-v3 accounts restores the authority: the loader is the cause.
    env.svm.set_account(CP, original).unwrap();
    env.svm
        .set_account(pda::program_data(&CP), original_pd)
        .unwrap();
    update(&mut env, &cp_owner, &CP, "restored").expect("update after restoring loader-v3");
    assert_eq!(uri_of(&env, &CP), "restored");
}

// --------------------------------------------------------------------------------------
// Reachability on the real runtime
// --------------------------------------------------------------------------------------

/// The premise needs a loader-v4 strategy. On this runtime the real loader-v3 `Migrate`
/// is rejected even from the upgrade authority, and a loader-v4-owned program can't run.
#[test]
fn h034_loader_v4_strategy_is_unreachable_on_this_runtime() {
    let (mut env, owner) = fresh_cp();
    let migrate = Instruction::new_with_bytes(
        client::BPF_LOADER_UPGRADEABLE_ID,
        &MIGRATE_TAG.to_le_bytes(),
        vec![
            AccountMeta::new(pda::program_data(&CP), false),
            AccountMeta::new(CP, false),
            AccountMeta::new_readonly(owner.pubkey(), true),
        ],
    );
    assert_fails_with(env.send(&[migrate], &[&owner]), "InvalidInstructionData");
    assert_eq!(
        env.svm.get_account(&CP).unwrap().owner,
        client::BPF_LOADER_UPGRADEABLE_ID,
        "still loader-v3 after the refused Migrate"
    );

    add_loader_v4_placeholder(&mut env);
    reown(
        &mut env,
        &CP,
        "test_strategy_cp",
        LOADER_V4,
        loader_v4_header(&CP, 2),
        true,
    );
    let call = Instruction::new_with_bytes(CP, &[0u8; 8], vec![]);
    assert_fails_with(env.send(&[call], &[]), "UnsupportedProgramId");
}
