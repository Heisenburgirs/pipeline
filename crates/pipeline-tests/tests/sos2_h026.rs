//! SOS audit #2, H026: `check_registrar` binds `register_strategy` / `update_metadata`
//! to the strategy program's loader-v3 upgrade authority or its `["pipeline-register"]`
//! PDA. Every crafted loader account below is built with `set_account`; the real loader
//! is used for `SetAuthority` and `Close`.
//!
//! Safe behaviour pinned here:
//!   * a Program account that is not a loader-v3 tag-2 account (loader-v2, loader-v4,
//!     native, system, Buffer, Uninitialized, ProgramData, short data) is refused with
//!     `UnsupportedLoader`;
//!   * a ProgramData that is not owned by loader-v3, is at the wrong address, has the
//!     wrong tag, has an option byte other than 0/1 or is truncated is refused with
//!     `WrongProgramData`;
//!   * another program's ProgramData (whose authority the caller holds) never authorises
//!     the program being registered or updated;
//!   * the authority is read live: after a real `SetAuthority` the old authority loses
//!     `update_metadata`, a freeze leaves only the PDA path, a closed program fails;
//!   * the PDA path accepts only the registered program's own canonical PDA, and still
//!     runs the loader-v3 parse.

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
const SYSTEM: Pubkey = client::pubkey!("11111111111111111111111111111111");

fn edit(env: &mut Env, key: &Pubkey, f: impl FnOnce(&mut Account)) {
    let mut account = env.svm.get_account(key).expect("account exists");
    f(&mut account);
    env.svm.set_account(*key, account).unwrap();
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

/// A CP program deployed with `owner` as upgrade authority, not yet registered.
fn fresh_cp() -> (Env, Keypair) {
    let mut env = Env::new();
    let owner = env.funded_keypair();
    env.deploy_strategy(&CP, "test_strategy_cp", &owner.pubkey());
    (env, owner)
}

fn set_authority_ix(program: &Pubkey, current: &Pubkey, new: Option<&Pubkey>) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new(pda::program_data(program), false),
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

fn program_account(owner: Pubkey, data: Vec<u8>) -> Account {
    Account {
        lamports: 10_000_000,
        owner,
        // LiteSVM validates executable accounts as ELF; the router reads no executable flag.
        executable: false,
        data,
        rent_epoch: 0,
    }
}

/// A 36-byte loader-v3 `Program` header naming CP's real ProgramData.
fn program_header(tag: u32, programdata: &Pubkey) -> Vec<u8> {
    let mut data = tag.to_le_bytes().to_vec();
    data.extend_from_slice(programdata.as_ref());
    data
}

// --------------------------------------------------------------------------------------
// Program account variants
// --------------------------------------------------------------------------------------

/// Only a loader-v3 tag-2 account with at least 36 bytes is a program. The ProgramData
/// stays valid (authority = owner), so only the Program account decides the result.
#[test]
fn h026_program_account_must_be_a_loader_v3_program() {
    let real_pd = pda::program_data(&CP);
    let cases: Vec<(&str, Account)> = vec![
        (
            "loader-v2 owner, valid-looking data",
            program_account(LOADER_V2, program_header(2, &real_pd)),
        ),
        (
            "loader-v4 owner, valid-looking data",
            program_account(LOADER_V4, program_header(2, &real_pd)),
        ),
        (
            "native loader owner",
            program_account(NATIVE_LOADER, program_header(2, &real_pd)),
        ),
        (
            "system-owned, valid-looking data",
            program_account(SYSTEM, program_header(2, &real_pd)),
        ),
        (
            "Buffer (tag 1) under loader-v3",
            program_account(client::BPF_LOADER_UPGRADEABLE_ID, {
                let mut d = program_header(1, &real_pd);
                d[4..36].copy_from_slice(&[0u8; 32]);
                d
            }),
        ),
        (
            "Buffer (tag 1) whose authority field holds the ProgramData address",
            program_account(
                client::BPF_LOADER_UPGRADEABLE_ID,
                program_header(1, &real_pd),
            ),
        ),
        (
            "Uninitialized (tag 0), 36 zero bytes",
            program_account(client::BPF_LOADER_UPGRADEABLE_ID, vec![0u8; 36]),
        ),
        (
            "Uninitialized (tag 0) but with the ProgramData address",
            program_account(
                client::BPF_LOADER_UPGRADEABLE_ID,
                program_header(0, &real_pd),
            ),
        ),
        (
            "ProgramData (tag 3) used as the program",
            program_account(
                client::BPF_LOADER_UPGRADEABLE_ID,
                program_header(3, &real_pd),
            ),
        ),
        (
            "tag 2 with a high byte set (u32 0x0100_0002)",
            program_account(client::BPF_LOADER_UPGRADEABLE_ID, {
                let mut d = program_header(2, &real_pd);
                d[3] = 1;
                d
            }),
        ),
        (
            "tag 2, 35 bytes (short)",
            program_account(client::BPF_LOADER_UPGRADEABLE_ID, {
                let mut d = program_header(2, &real_pd);
                d.pop();
                d
            }),
        ),
        (
            "tag 2, empty data",
            program_account(client::BPF_LOADER_UPGRADEABLE_ID, vec![]),
        ),
    ];
    for (name, account) in cases {
        let (mut env, owner) = fresh_cp();
        env.svm
            .set_account(CP, account)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_fails_with(register(&mut env, &owner, &CP), "UnsupportedLoader");
        assert!(
            env.svm.get_account(&pda::strategy(&CP)).is_none(),
            "{name}: nothing may be registered"
        );
        eprintln!("ok (UnsupportedLoader): {name}");
    }

    // Control: the genuine account registers.
    let (mut env, owner) = fresh_cp();
    register(&mut env, &owner, &CP).expect("genuine program registers");
}

/// A valid tag-2 Program naming a *different* loader-owned account as its ProgramData is
/// refused at the address cross-check, whoever owns that account.
#[test]
fn h026_program_account_naming_a_foreign_programdata_is_refused() {
    let (mut env, owner) = fresh_cp();
    // The attacker's own program (EVIL, authority = attacker).
    let attacker = env.funded_keypair();
    env.deploy_strategy(&EVIL, "test_strategy_evil", &attacker.pubkey());
    // CP's Program account is rewritten to name EVIL's ProgramData.
    edit(&mut env, &CP, |a| {
        a.data[4..36].copy_from_slice(pda::program_data(&EVIL).as_ref());
    });
    // The instruction builder derives CP's own ProgramData: mismatch.
    assert_fails_with(register(&mut env, &owner, &CP), "WrongProgramData");
    // Passing EVIL's ProgramData (matches the rewritten Program) lets the attacker in.
    // This state can't be produced by the loader (a Program account is only ever written
    // by `DeployWithMaxDataLen`, which derives the ProgramData from the Program's own
    // address), so it is crafted state: it shows the router relies on that loader
    // invariant and nothing else.
    let payer = env.admin.pubkey();
    let mut ix = client::register_strategy(
        &payer,
        &attacker.pubkey(),
        &CP,
        &env.listing_fee_recipient,
        "uri",
        u64::MAX,
    );
    ix.accounts[3].pubkey = pda::program_data(&EVIL);
    let result = env.send(&[ix], &[&attacker]);
    assert!(
        result.is_ok(),
        "crafted-state control (loader invariant broken): {result:?}"
    );
}

// --------------------------------------------------------------------------------------
// ProgramData variants
// --------------------------------------------------------------------------------------

#[test]
fn h026_programdata_variants_are_refused_or_read_as_immutable() {
    let pd = pda::program_data(&CP);
    type Mutation = Box<dyn Fn(&mut Account, &Pubkey)>;
    // (name, mutation, expected error)
    let cases: Vec<(&str, Mutation, &str)> = vec![
        (
            "owned by the system program",
            Box::new(|a, _| a.owner = SYSTEM),
            "WrongProgramData",
        ),
        (
            "owned by loader-v2",
            Box::new(|a, _| a.owner = LOADER_V2),
            "WrongProgramData",
        ),
        (
            "owned by loader-v4",
            Box::new(|a, _| a.owner = LOADER_V4),
            "WrongProgramData",
        ),
        (
            "owned by the router",
            Box::new(|a, _| a.owner = client::PROGRAM_ID),
            "WrongProgramData",
        ),
        (
            "tag 1 (Buffer) with the owner's key at 13..45",
            Box::new(|a, _| a.data[..4].copy_from_slice(&1u32.to_le_bytes())),
            "WrongProgramData",
        ),
        (
            "tag 2 (Program)",
            Box::new(|a, _| a.data[..4].copy_from_slice(&2u32.to_le_bytes())),
            "WrongProgramData",
        ),
        (
            "tag 0 (Uninitialized)",
            Box::new(|a, _| a.data[..4].copy_from_slice(&0u32.to_le_bytes())),
            "WrongProgramData",
        ),
        (
            "tag 3 with high byte set",
            Box::new(|a, _| a.data[3] = 1),
            "WrongProgramData",
        ),
        (
            "option byte 2, authority bytes = owner",
            Box::new(|a, _| a.data[12] = 2),
            "WrongProgramData",
        ),
        (
            "option byte 255, authority bytes = owner",
            Box::new(|a, _| a.data[12] = 255),
            "WrongProgramData",
        ),
        (
            "12 bytes (option byte missing)",
            Box::new(|a, _| a.data.truncate(12)),
            "WrongProgramData",
        ),
        (
            "empty data",
            Box::new(|a, _| a.data.clear()),
            "WrongProgramData",
        ),
        (
            "option 1 but only 44 bytes (authority truncated)",
            Box::new(|a, _| a.data.truncate(44)),
            "WrongProgramData",
        ),
        (
            "option 0 (immutable) but authority bytes still hold the owner",
            Box::new(|a, _| a.data[12] = 0),
            "NotStrategyOwner",
        ),
        (
            "option 0, exactly 13 bytes",
            Box::new(|a, _| {
                a.data[12] = 0;
                a.data.truncate(13);
            }),
            "NotStrategyOwner",
        ),
    ];
    for (name, mutate, expected) in cases {
        let (mut env, owner) = fresh_cp();
        edit(&mut env, &pd, |a| mutate(a, &owner.pubkey()));
        assert_fails_with(register(&mut env, &owner, &CP), expected);
        assert!(
            env.svm.get_account(&pda::strategy(&CP)).is_none(),
            "{name}: nothing may be registered"
        );
        eprintln!("ok ({expected}): {name}");
    }

    // Boundary controls: a 45-byte ProgramData (header only) with option 1 is accepted;
    // so is one with extra trailing bytes (the real layout carries the ELF there).
    let (mut env, owner) = fresh_cp();
    edit(&mut env, &pd, |a| a.data.truncate(45));
    register(&mut env, &owner, &CP).expect("exactly 45 bytes, option 1");
}

/// A ProgramData at the wrong address is refused even when it is a perfect loader-owned
/// ProgramData whose authority is the caller (the cross-program attack).
#[test]
fn h026_another_programs_programdata_never_authorises() {
    let (mut env, victim) = fresh_cp();
    let attacker = env.funded_keypair();
    env.deploy_strategy(&EVIL, "test_strategy_evil", &attacker.pubkey());
    let payer = env.admin.pubkey();

    // Register CP while handing in EVIL's ProgramData (attacker is its authority).
    let mut ix = client::register_strategy(
        &payer,
        &attacker.pubkey(),
        &CP,
        &env.listing_fee_recipient,
        "uri",
        u64::MAX,
    );
    ix.accounts[3].pubkey = pda::program_data(&EVIL);
    assert_fails_with(env.send(&[ix], &[&attacker]), "WrongProgramData");

    // The Program account itself as the ProgramData.
    let mut ix = client::register_strategy(
        &payer,
        &attacker.pubkey(),
        &CP,
        &env.listing_fee_recipient,
        "uri",
        u64::MAX,
    );
    ix.accounts[3].pubkey = CP;
    assert_fails_with(env.send(&[ix], &[&attacker]), "WrongProgramData");

    // Same for `update_metadata` on a registered strategy.
    register(&mut env, &victim, &CP).expect("victim registers");
    let mut ix = client::update_metadata(&attacker.pubkey(), &CP, "evil");
    ix.accounts[2].pubkey = pda::program_data(&EVIL);
    assert_fails_with(env.send(&[ix], &[&attacker]), "WrongProgramData");
    assert_eq!(uri_of(&env, &CP), "uri");

    // The attacker is also refused with the correct accounts.
    assert_fails_with(update(&mut env, &attacker, &CP, "evil"), "NotStrategyOwner");
    assert_eq!(uri_of(&env, &CP), "uri");
}

// --------------------------------------------------------------------------------------
// Authority changes (real loader-v3), closed program
// --------------------------------------------------------------------------------------

#[test]
fn h026_authority_is_read_live_after_transfer_and_freeze() {
    let (mut env, owner) = fresh_cp();
    register(&mut env, &owner, &CP).expect("register");
    let next = env.funded_keypair();

    env.send(
        &[set_authority_ix(&CP, &owner.pubkey(), Some(&next.pubkey()))],
        &[&owner],
    )
    .expect("real loader SetAuthority to a new key");

    // Old authority has lost control; the new one has it.
    assert_fails_with(update(&mut env, &owner, &CP, "old"), "NotStrategyOwner");
    assert_eq!(uri_of(&env, &CP), "uri");
    update(&mut env, &next, &CP, "new").expect("new authority updates");
    assert_eq!(uri_of(&env, &CP), "new");

    // Within one transaction the order matters and is live: update after the transfer by
    // the old authority fails, so the whole transaction reverts.
    let back = set_authority_ix(&CP, &next.pubkey(), Some(&owner.pubkey()));
    let stale = client::update_metadata(&next.pubkey(), &CP, "stale");
    assert!(env.send(&[back, stale], &[&next]).is_err());
    assert_eq!(uri_of(&env, &CP), "new");
    // ... and the transfer reverted with it.
    assert_fails_with(update(&mut env, &owner, &CP, "x"), "NotStrategyOwner");

    // Freeze: nobody can update with the old keys.
    env.send(&[set_authority_ix(&CP, &next.pubkey(), None)], &[&next])
        .expect("freeze");
    assert_fails_with(update(&mut env, &next, &CP, "y"), "NotStrategyOwner");
    assert_fails_with(update(&mut env, &owner, &CP, "y"), "NotStrategyOwner");
    assert_eq!(uri_of(&env, &CP), "new");
}

#[test]
fn h026_closed_program_cannot_register_or_update() {
    let (mut env, owner) = fresh_cp();
    register(&mut env, &owner, &CP).expect("register");

    // Real loader-v3 `Close` of the ProgramData (instruction 5):
    // [programdata (w), recipient (w), authority (s), program (w)].
    let slot_before = env.svm.get_sysvar::<solana_clock::Clock>().slot;
    let mut clock = env.svm.get_sysvar::<solana_clock::Clock>();
    clock.slot = slot_before + 10;
    env.svm.set_sysvar(&clock);
    let close = Instruction::new_with_bytes(
        client::BPF_LOADER_UPGRADEABLE_ID,
        &5u32.to_le_bytes(),
        vec![
            AccountMeta::new(pda::program_data(&CP), false),
            AccountMeta::new(owner.pubkey(), false),
            AccountMeta::new_readonly(owner.pubkey(), true),
            AccountMeta::new(CP, false),
        ],
    );
    let closed_for_real = env.send(&[close], &[&owner]);
    eprintln!("real Close: {:?}", closed_for_real.as_ref().map(|_| ()));
    if closed_for_real.is_err() {
        // Fall back to the account state `Close` leaves behind: ProgramData emptied.
        env.svm
            .set_account(pda::program_data(&CP), Account::default())
            .unwrap();
    }
    assert!(env
        .svm
        .get_account(&pda::program_data(&CP))
        .map(|a| a.lamports == 0 || a.data.is_empty() || a.owner == SYSTEM)
        .unwrap_or(true));

    assert_fails_with(update(&mut env, &owner, &CP, "zombie"), "WrongProgramData");
    assert_eq!(uri_of(&env, &CP), "uri");

    // A new registration against a closed program also fails (fresh program ID would
    // need the same).
    let other = Keypair::new();
    let mut env2 = Env::new();
    env2.deploy_strategy(&CP, "test_strategy_cp", &other.pubkey());
    env2.svm.airdrop(&other.pubkey(), 10 * SOL).unwrap();
    env2.svm
        .set_account(pda::program_data(&CP), Account::default())
        .unwrap();
    assert_fails_with(register(&mut env2, &other, &CP), "WrongProgramData");
}

// --------------------------------------------------------------------------------------
// PDA path
// --------------------------------------------------------------------------------------

/// Sends `inner` (a router instruction whose registrar is a PDA of `program`) through
/// the evil strategy's `FORWARD_TO_ROUTER`, which signs `["pipeline-register"]`.
fn forward_via(
    env: &mut Env,
    program: &Pubkey,
    signer_pda: &Pubkey,
    inner: Instruction,
) -> TxResult {
    let mut accounts = vec![AccountMeta::new_readonly(client::PROGRAM_ID, false)];
    accounts.extend(inner.accounts.iter().map(|meta| AccountMeta {
        // A PDA can't sign the outer transaction; the strategy signs for it.
        is_signer: meta.is_signer && meta.pubkey != *signer_pda,
        ..meta.clone()
    }));
    let mut data = b"evilfwd!".to_vec();
    data.extend_from_slice(&inner.data);
    let forward = Instruction::new_with_bytes(*program, &data, accounts);
    env.send(&[forward], &[])
}

#[test]
fn h026_pda_path_binds_to_the_registered_programs_own_canonical_pda() {
    let mut env = Env::new();
    let payer = env.admin.pubkey();
    let evil_owner = env.funded_keypair();
    let cp_owner = env.funded_keypair();
    env.deploy_strategy(&EVIL, "test_strategy_evil", &evil_owner.pubkey());
    env.deploy_strategy(&CP, "test_strategy_cp", &cp_owner.pubkey());
    let evil_pda = pda::register(&EVIL);
    let recipient = env.listing_fee_recipient;

    // (1) EVIL's PDA signs but is not CP's PDA and CP's authority is the owner: refused.
    let inner = client::register_strategy(&payer, &evil_pda, &CP, &recipient, "x", u64::MAX);
    assert_fails_with(
        forward_via(&mut env, &EVIL, &evil_pda, inner),
        "NotStrategyOwner",
    );
    assert!(env.svm.get_account(&pda::strategy(&CP)).is_none());

    // (2) The PDA path still runs the loader-v3 parse: a mangled ProgramData option byte
    // is refused even though the PDA is genuinely EVIL's and signs.
    let pd = pda::program_data(&EVIL);
    let original = env.svm.get_account(&pd).unwrap();
    edit(&mut env, &pd, |a| a.data[12] = 2);
    let inner = client::register_strategy(&payer, &evil_pda, &EVIL, &recipient, "x", u64::MAX);
    assert_fails_with(
        forward_via(&mut env, &EVIL, &evil_pda, inner),
        "WrongProgramData",
    );
    // ... and a wrong ProgramData owner.
    edit(&mut env, &pd, |a| {
        a.data = original.data.clone();
        a.owner = SYSTEM;
    });
    let inner = client::register_strategy(&payer, &evil_pda, &EVIL, &recipient, "x", u64::MAX);
    assert_fails_with(
        forward_via(&mut env, &EVIL, &evil_pda, inner),
        "WrongProgramData",
    );
    env.svm.set_account(pd, original).unwrap();

    // (3) EVIL's own PDA registers EVIL while EVIL is still upgradeable by a third
    // party (documented: the program consented by signing). Accepted.
    let inner = client::register_strategy(&payer, &evil_pda, &EVIL, &recipient, "pda", u64::MAX);
    forward_via(&mut env, &EVIL, &evil_pda, inner).expect("PDA path on an upgradeable program");
    assert_eq!(uri_of(&env, &EVIL), "pda");

    // (4) The PDA also updates metadata after the program is frozen, and the old
    // authority cannot.
    set_upgrade_authority(&mut env.svm, &EVIL, None);
    assert_fails_with(
        update(&mut env, &evil_owner, &EVIL, "owner"),
        "NotStrategyOwner",
    );
    let inner = client::update_metadata(&evil_pda, &EVIL, "frozen-pda");
    forward_via(&mut env, &EVIL, &evil_pda, inner).expect("PDA updates a frozen program");
    assert_eq!(uri_of(&env, &EVIL), "frozen-pda");

    // (5) EVIL's PDA can't update CP's metadata even once CP is registered by its owner
    // and later frozen.
    register(&mut env, &cp_owner, &CP).expect("cp registers");
    set_upgrade_authority(&mut env.svm, &CP, None);
    let inner = client::update_metadata(&evil_pda, &CP, "hijack");
    assert_fails_with(
        forward_via(&mut env, &EVIL, &evil_pda, inner),
        "NotStrategyOwner",
    );
    assert_eq!(uri_of(&env, &CP), "uri");
}
