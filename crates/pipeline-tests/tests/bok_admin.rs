//! BOK LiteSVM tests — router config admin and strategy registration
//! (INV-ADM-1, INV-ADM-2, INV-ADM-3), run against the real router binary.

mod common;

use common::*;
use litesvm::LiteSVM;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_events::PipelineEvent;
use proptest::prelude::*;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

/// `MAX_LISTING_FEE_LAMPORTS` in programs/pipeline/src/lib.rs.
const MAX_FEE: u64 = 1_000_000_000_000;
/// `8 + Strategy::INIT_SPACE`: discriminator, program id, String(200), version,
/// registered_at, market_count, bump.
const STRATEGY_SPACE: usize = 8 + 32 + 4 + 200 + 1 + 8 + 8 + 1;

/// The router deployed with `admin` as upgrade authority, but no config yet.
fn env_without_config() -> Env {
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

fn rent_min_0(env: &Env) -> u64 {
    env.svm.minimum_balance_for_rent_exemption(0)
}

fn config(env: &Env) -> client::Config {
    client::decode_config(&env.account_data(&pda::config())).expect("config")
}

/// The error `check_listing_fee` must give for `fee`, if any.
fn fee_error(fee: u64, rent: u64) -> Option<&'static str> {
    if fee > MAX_FEE {
        Some("ListingFeeTooHigh")
    } else if fee != 0 && fee < rent {
        Some("ListingFeeBelowRent")
    } else {
        None
    }
}

/// The fee values worth probing: both sides of both boundaries, plus 0 and extremes.
fn boundary_fees(rent: u64) -> Vec<u64> {
    vec![
        0,
        1,
        rent - 1,
        rent,
        rent + 1,
        LISTING_FEE,
        MAX_FEE - 1,
        MAX_FEE,
        MAX_FEE + 1,
        u64::MAX,
    ]
}

fn set_fee(env: &mut Env, fee: u64) {
    let ix = client::update_config(
        &env.admin.pubkey(),
        &client::UpdateConfigArgs {
            listing_fee_lamports: Some(fee),
            ..Default::default()
        },
    );
    env.send(&[ix], &[]).expect("set listing fee");
}

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

fn router_frozen(env: &Env) -> bool {
    env.account_data(&pda::program_data(&client::PROGRAM_ID))[12] == 0
}

fn config_changed(
    meta: &litesvm::types::TransactionMetadata,
) -> Vec<pipeline_events::ConfigChanged> {
    events(meta)
        .into_iter()
        .filter_map(|e| match e {
            PipelineEvent::ConfigChanged(c) => Some(c),
            _ => None,
        })
        .collect()
}

fn assert_event_matches_config(
    meta: &litesvm::types::TransactionMetadata,
    config: &client::Config,
) {
    let changed = config_changed(meta);
    assert_eq!(changed.len(), 1, "one ConfigChanged event");
    let event = &changed[0];
    assert_eq!(event.admin, config.admin);
    assert_eq!(event.paused, config.paused);
    assert_eq!(event.listing_fee_lamports, config.listing_fee_lamports);
    assert_eq!(event.listing_fee_recipient, config.listing_fee_recipient);
    assert_eq!(event.treasury_owner, config.treasury_owner);
}

// ---------------------------------------------------------------------------------------
// INV-ADM-1 — listing fee bounds on both writers
// ---------------------------------------------------------------------------------------

/// INV-ADM-1: `initialize_config` accepts exactly 0 and [rent_min(0), 1e12], refuses the
/// rest with the right error, and a refused initialisation creates no config.
#[test]
fn bok_inv_adm_1_initialize_config_fee_bounds() {
    let rent = rent_min_0(&env_without_config());
    for fee in boundary_fees(rent) {
        let mut env = env_without_config();
        assert_eq!(rent_min_0(&env), rent);
        let recipient = Pubkey::new_unique();
        let treasury = Pubkey::new_unique();
        let ix = client::initialize_config(
            &env.admin.pubkey(),
            &client::ConfigArgs {
                listing_fee_lamports: fee,
                listing_fee_recipient: recipient,
                treasury_owner: treasury,
            },
        );
        let result = env.send(&[ix], &[]);
        match fee_error(fee, rent) {
            Some(error) => {
                assert_fails_with(result, error);
                assert!(
                    env.svm.get_account(&pda::config()).is_none(),
                    "fee {fee}: a refused initialisation must not create the config"
                );
            }
            None => {
                result.unwrap_or_else(|e| panic!("fee {fee} must be accepted: {:?}", e.err));
                let config = config(&env);
                assert_eq!(config.listing_fee_lamports, fee);
                assert_eq!(config.admin, env.admin.pubkey());
                assert_eq!(config.listing_fee_recipient, recipient);
                assert_eq!(config.treasury_owner, treasury);
                assert!(!config.paused);
            }
        }
    }
}

/// INV-ADM-1: `update_config` applies the same bounds, emits the new config on success,
/// and leaves the config unchanged on refusal.
#[test]
fn bok_inv_adm_1_update_config_fee_bounds() {
    let mut env = Env::new();
    let rent = rent_min_0(&env);
    for fee in boundary_fees(rent) {
        let before = env.account_data(&pda::config());
        let ix = client::update_config(
            &env.admin.pubkey(),
            &client::UpdateConfigArgs {
                listing_fee_lamports: Some(fee),
                ..Default::default()
            },
        );
        let result = env.send(&[ix], &[]);
        match fee_error(fee, rent) {
            Some(error) => {
                assert_fails_with(result, error);
                assert_eq!(env.account_data(&pda::config()), before, "fee {fee}");
            }
            None => {
                let meta = result.unwrap_or_else(|e| panic!("fee {fee}: {:?}", e.err));
                let config = config(&env);
                assert_eq!(config.listing_fee_lamports, fee);
                assert_event_matches_config(&meta, &config);
            }
        }
    }
}

/// INV-ADM-1: an `update_config` that bundles a bad fee with changes to every other
/// field reverts completely: admin, recipient, treasury owner and pause stay as they
/// were (the account bytes are identical).
#[test]
fn bok_inv_adm_1_rejected_update_changes_no_field() {
    let mut env = Env::new();
    let rent = rent_min_0(&env);
    let original = config(&env);
    for (fee, error) in [
        (MAX_FEE + 1, "ListingFeeTooHigh"),
        (u64::MAX, "ListingFeeTooHigh"),
        (1, "ListingFeeBelowRent"),
        (rent - 1, "ListingFeeBelowRent"),
    ] {
        let before = env.account_data(&pda::config());
        let ix = client::update_config(
            &env.admin.pubkey(),
            &client::UpdateConfigArgs {
                admin: Some(Pubkey::new_unique()),
                paused: Some(true),
                listing_fee_lamports: Some(fee),
                listing_fee_recipient: Some(Pubkey::new_unique()),
                treasury_owner: Some(Pubkey::new_unique()),
            },
        );
        assert_fails_with(env.send(&[ix], &[]), error);
        assert_eq!(env.account_data(&pda::config()), before);
        assert_eq!(config(&env), original);
    }

    // Control: the same bundle with a valid fee changes every field.
    let (admin, recipient, treasury) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    let ix = client::update_config(
        &env.admin.pubkey(),
        &client::UpdateConfigArgs {
            admin: Some(admin),
            paused: Some(true),
            listing_fee_lamports: Some(rent),
            listing_fee_recipient: Some(recipient),
            treasury_owner: Some(treasury),
        },
    );
    let meta = env.send(&[ix], &[]).unwrap();
    let config = config(&env);
    assert_eq!(
        (
            config.admin,
            config.paused,
            config.listing_fee_lamports,
            config.listing_fee_recipient,
            config.treasury_owner
        ),
        (admin, true, rent, recipient, treasury)
    );
    assert_event_matches_config(&meta, &config);
}

// ---------------------------------------------------------------------------------------
// INV-ADM-2 — registration charges exactly the configured fee
// ---------------------------------------------------------------------------------------

/// Sends like `Env::send` (admin pays the transaction fee) and also returns the
/// message's account keys, so inner instructions can be decoded.
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

/// System-program transfers among the transaction's inner instructions, as
/// (from, to, lamports).
fn inner_system_transfers(
    meta: &litesvm::types::TransactionMetadata,
    keys: &[Pubkey],
) -> Vec<(Pubkey, Pubkey, u64)> {
    meta.inner_instructions
        .iter()
        .flatten()
        .filter(|inner| {
            keys[inner.instruction.program_id_index as usize] == client::SYSTEM_PROGRAM_ID
        })
        .filter(|inner| {
            inner.instruction.data.len() == 12 && inner.instruction.data[..4] == 2u32.to_le_bytes()
        })
        .map(|inner| {
            let ix = &inner.instruction;
            (
                keys[ix.accounts[0] as usize],
                keys[ix.accounts[1] as usize],
                u64::from_le_bytes(ix.data[4..12].try_into().unwrap()),
            )
        })
        .collect()
}

/// Deploys a copy of the CP strategy at a new address, owned by a new funded keypair.
fn new_strategy(env: &mut Env, owner_lamports: u64) -> (Pubkey, Keypair) {
    let program = Pubkey::new_unique();
    let owner = Keypair::new();
    env.svm.airdrop(&owner.pubkey(), owner_lamports).unwrap();
    env.deploy_strategy(&program, "test_strategy_cp", &owner.pubkey());
    (program, owner)
}

fn register_ix(env: &Env, program: &Pubkey, owner: &Pubkey, max: u64) -> Instruction {
    client::register_strategy(
        owner,
        owner,
        program,
        &env.listing_fee_recipient,
        "https://example.com/strategy.json",
        max,
    )
}

/// INV-ADM-2: for each (fee F, registrant maximum), a registration succeeds iff F ≤ max;
/// on success the recipient gains exactly F, the payer loses exactly F plus the Strategy
/// rent, the event reports F, and F = 0 makes no transfer at all; on refusal nothing
/// moves.
#[test]
fn bok_inv_adm_2_registration_charges_exactly_the_fee() {
    let mut env = Env::new();
    let rent0 = rent_min_0(&env);
    let strategy_rent = env.svm.minimum_balance_for_rent_exemption(STRATEGY_SPACE);
    let cases = [
        (0, 0),
        (0, u64::MAX),
        (rent0, rent0),
        (LISTING_FEE, LISTING_FEE),
        (LISTING_FEE, u64::MAX),
        (LISTING_FEE, LISTING_FEE - 1),
        (LISTING_FEE, 0),
        (MAX_FEE, MAX_FEE),
        (MAX_FEE, MAX_FEE - 1),
    ];
    for (fee, max) in cases {
        // A recipient that doesn't exist yet: a fee of exactly rent_min(0) must still
        // be payable to it.
        let recipient = Pubkey::new_unique();
        let ix = client::update_config(
            &env.admin.pubkey(),
            &client::UpdateConfigArgs {
                listing_fee_lamports: Some(fee),
                listing_fee_recipient: Some(recipient),
                ..Default::default()
            },
        );
        env.send(&[ix], &[]).unwrap();
        env.listing_fee_recipient = recipient;

        let (program, owner) = new_strategy(&mut env, MAX_FEE + 10 * SOL);
        let payer_before = env.lamports(&owner.pubkey());
        let ix = register_ix(&env, &program, &owner.pubkey(), max);
        let (result, keys) = send_with_keys(&mut env, &[ix], &[&owner]);
        let strategy_pda = pda::strategy(&program);
        if fee <= max {
            let meta = result.unwrap_or_else(|e| panic!("({fee}, {max}): {:?}", e.err));
            assert_eq!(env.lamports(&recipient), fee, "({fee}, {max}) recipient");
            assert_eq!(
                payer_before - env.lamports(&owner.pubkey()),
                fee + strategy_rent,
                "({fee}, {max}) payer"
            );
            let account = env.svm.get_account(&strategy_pda).unwrap();
            assert_eq!(account.lamports, strategy_rent);
            assert_eq!(account.data.len(), STRATEGY_SPACE);
            let registered: Vec<_> = events(&meta)
                .into_iter()
                .filter_map(|e| match e {
                    PipelineEvent::StrategyRegistered(r) => Some(r),
                    _ => None,
                })
                .collect();
            assert_eq!(registered.len(), 1);
            assert_eq!(registered[0].fee_paid, fee);
            assert_eq!(registered[0].strategy, program);
            let to_recipient: Vec<_> = inner_system_transfers(&meta, &keys)
                .into_iter()
                .filter(|(_, to, _)| *to == recipient)
                .collect();
            if fee == 0 {
                assert!(to_recipient.is_empty(), "F = 0 must not transfer");
                assert!(env.svm.get_account(&recipient).is_none());
            } else {
                assert_eq!(to_recipient, vec![(owner.pubkey(), recipient, fee)]);
            }
        } else {
            assert_fails_with(result, "ListingFeeAboveMaximum");
            assert_eq!(env.lamports(&owner.pubkey()), payer_before);
            assert_eq!(env.lamports(&recipient), 0);
            assert!(env.svm.get_account(&strategy_pda).is_none());
        }
    }
}

/// INV-ADM-2: a fee change earlier in the same transaction is honoured if it is within
/// the registrant's maximum and makes the whole transaction fail otherwise; a change
/// after the registration doesn't affect it.
#[test]
fn bok_inv_adm_2_update_then_register_in_one_transaction() {
    let mut env = Env::new();
    let strategy_rent = env.svm.minimum_balance_for_rent_exemption(STRATEGY_SPACE);
    let recipient = env.listing_fee_recipient;
    let max = 2 * LISTING_FEE;
    let fee_update = |admin: &Pubkey, fee: u64| {
        client::update_config(
            admin,
            &client::UpdateConfigArgs {
                listing_fee_lamports: Some(fee),
                ..Default::default()
            },
        )
    };
    let admin = env.admin.pubkey();

    // New fee within the maximum: charged at the new fee.
    for new_fee in [0, LISTING_FEE / 2, max] {
        set_fee(&mut env, LISTING_FEE);
        let (program, owner) = new_strategy(&mut env, 10 * SOL);
        let (p0, r0) = (env.lamports(&owner.pubkey()), env.lamports(&recipient));
        let ixs = [
            fee_update(&admin, new_fee),
            register_ix(&env, &program, &owner.pubkey(), max),
        ];
        env.send(&ixs, &[&owner]).unwrap();
        assert_eq!(env.lamports(&recipient) - r0, new_fee);
        assert_eq!(p0 - env.lamports(&owner.pubkey()), new_fee + strategy_rent);
    }

    // New fee above the maximum: everything reverts, including the fee change.
    set_fee(&mut env, LISTING_FEE);
    let (program, owner) = new_strategy(&mut env, 10 * SOL);
    let (p0, r0) = (env.lamports(&owner.pubkey()), env.lamports(&recipient));
    let ixs = [
        fee_update(&admin, max + 1),
        register_ix(&env, &program, &owner.pubkey(), max),
    ];
    assert_fails_with(env.send(&ixs, &[&owner]), "ListingFeeAboveMaximum");
    assert_eq!(config(&env).listing_fee_lamports, LISTING_FEE);
    assert_eq!(env.lamports(&owner.pubkey()), p0);
    assert_eq!(env.lamports(&recipient), r0);
    assert!(env.svm.get_account(&pda::strategy(&program)).is_none());

    // Register first, then raise the fee: charged at the old fee.
    let ixs = [
        register_ix(&env, &program, &owner.pubkey(), max),
        fee_update(&admin, MAX_FEE),
    ];
    env.send(&ixs, &[&owner]).unwrap();
    assert_eq!(env.lamports(&recipient) - r0, LISTING_FEE);
    assert_eq!(
        p0 - env.lamports(&owner.pubkey()),
        LISTING_FEE + strategy_rent
    );
    assert_eq!(config(&env).listing_fee_lamports, MAX_FEE);
}

/// INV-ADM-2: a pre-funded Strategy PDA only needs its rent topped up, and a payer that
/// is also the recipient ends down by the rent alone.
#[test]
fn bok_inv_adm_2_prefunded_pda_and_payer_as_recipient() {
    let mut env = Env::new();
    let strategy_rent = env.svm.minimum_balance_for_rent_exemption(STRATEGY_SPACE);

    let (program, owner) = new_strategy(&mut env, 10 * SOL);
    // Anyone can send SOL to the deterministic address first (a rent-exempt amount, as
    // the runtime requires for a new system account).
    let prefund = rent_min_0(&env) + 123_456;
    env.svm.airdrop(&pda::strategy(&program), prefund).unwrap();
    let (p0, r0) = (
        env.lamports(&owner.pubkey()),
        env.lamports(&env.listing_fee_recipient),
    );
    let ix = register_ix(&env, &program, &owner.pubkey(), u64::MAX);
    env.send(&[ix], &[&owner]).unwrap();
    assert_eq!(env.lamports(&env.listing_fee_recipient) - r0, LISTING_FEE);
    assert_eq!(
        p0 - env.lamports(&owner.pubkey()),
        LISTING_FEE + strategy_rent - prefund
    );
    assert_eq!(env.lamports(&pda::strategy(&program)), strategy_rent);

    let (program, owner) = new_strategy(&mut env, 10 * SOL);
    let ix = client::update_config(
        &env.admin.pubkey(),
        &client::UpdateConfigArgs {
            listing_fee_recipient: Some(owner.pubkey()),
            ..Default::default()
        },
    );
    env.send(&[ix], &[]).unwrap();
    env.listing_fee_recipient = owner.pubkey();
    let p0 = env.lamports(&owner.pubkey());
    let ix = register_ix(&env, &program, &owner.pubkey(), u64::MAX);
    let meta = env.send(&[ix], &[&owner]).unwrap();
    assert_eq!(p0 - env.lamports(&owner.pubkey()), strategy_rent);
    assert!(events(&meta)
        .iter()
        .any(|e| matches!(e, PipelineEvent::StrategyRegistered(r) if r.fee_paid == LISTING_FEE)));
}

// ---------------------------------------------------------------------------------------
// INV-ADM-3 — pause power ends at freeze
// ---------------------------------------------------------------------------------------

fn pause_ix(admin: &Pubkey, paused: bool) -> Instruction {
    client::update_config(
        admin,
        &client::UpdateConfigArgs {
            paused: Some(paused),
            ..Default::default()
        },
    )
}

/// INV-ADM-3: pausing works only while upgradeable; `lift_pause` works only once frozen
/// (freezing with the real loader instruction) and always leaves `paused = false`; once
/// frozen and unpaused, the router stays unpaused.
#[test]
fn bok_inv_adm_3_pause_and_lift_follow_the_freeze() {
    let mut env = Env::new();
    let admin = env.admin.pubkey();
    assert_fails_with(env.send(&[client::lift_pause()], &[]), "RouterNotFrozen");
    env.send(&[pause_ix(&admin, true)], &[]).unwrap();
    assert_fails_with(env.send(&[client::lift_pause()], &[]), "RouterNotFrozen");
    assert!(config(&env).paused);

    env.send(&[loader_set_authority(&admin, None)], &[])
        .expect("freeze through the loader");
    assert!(router_frozen(&env));
    let meta = env.send(&[client::lift_pause()], &[]).unwrap();
    let after = config(&env);
    assert!(!after.paused);
    assert_event_matches_config(&meta, &after);

    for _ in 0..3 {
        assert_fails_with(
            env.send(&[pause_ix(&admin, true)], &[]),
            "FrozenRouterCannotPause",
        );
        assert!(!config(&env).paused);
        env.send(&[pause_ix(&admin, false)], &[]).unwrap();
        // Lifting an absent pause is a harmless no-op.
        env.send(&[client::lift_pause()], &[]).unwrap();
        assert!(!config(&env).paused);
    }
}

/// INV-ADM-3: on a frozen router, an update that bundles `paused = Some(true)` with
/// valid changes to every other field reverts all of them.
#[test]
fn bok_inv_adm_3_frozen_bundled_pause_reverts_every_field() {
    let mut env = Env::new();
    set_upgrade_authority(&mut env.svm, &client::PROGRAM_ID, None);
    let before = env.account_data(&pda::config());
    let ix = client::update_config(
        &env.admin.pubkey(),
        &client::UpdateConfigArgs {
            admin: Some(Pubkey::new_unique()),
            paused: Some(true),
            listing_fee_lamports: Some(0),
            listing_fee_recipient: Some(Pubkey::new_unique()),
            treasury_owner: Some(Pubkey::new_unique()),
        },
    );
    assert_fails_with(env.send(&[ix], &[]), "FrozenRouterCannotPause");
    assert_eq!(env.account_data(&pda::config()), before);
}

/// INV-ADM-3: the freeze is read at execution time. Freeze-then-pause in one transaction
/// fails (and the freeze reverts with it); pause-then-freeze succeeds, and the leftover
/// pause can then be lifted by anyone.
#[test]
fn bok_inv_adm_3_freeze_and_pause_in_one_transaction() {
    let mut env = Env::new();
    let admin = env.admin.pubkey();
    let ixs = [loader_set_authority(&admin, None), pause_ix(&admin, true)];
    assert_fails_with(env.send(&ixs, &[]), "FrozenRouterCannotPause");
    assert!(!router_frozen(&env));
    assert!(!config(&env).paused);

    let ixs = [pause_ix(&admin, true), loader_set_authority(&admin, None)];
    env.send(&ixs, &[]).unwrap();
    assert!(router_frozen(&env));
    assert!(config(&env).paused);

    // Anyone (here a stranger paying its own fee) can lift it.
    let stranger = env.funded_keypair();
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[client::lift_pause()],
        Some(&stranger.pubkey()),
        &[&stranger],
        env.svm.latest_blockhash(),
    );
    env.svm.send_transaction(tx).unwrap();
    assert!(!config(&env).paused);
    assert_fails_with(
        env.send(&[pause_ix(&admin, true)], &[]),
        "FrozenRouterCannotPause",
    );
}

// ---------------------------------------------------------------------------------------
// INV-ADM-1 + INV-ADM-3 — random sequences against a model of the config
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum FeeChoice {
    Zero,
    One,
    RentMinus1,
    Rent,
    RentPlus1,
    Max,
    MaxPlus1,
    U64Max,
    InRange(u64),
}

impl FeeChoice {
    fn value(&self, rent: u64) -> u64 {
        match self {
            FeeChoice::Zero => 0,
            FeeChoice::One => 1,
            FeeChoice::RentMinus1 => rent - 1,
            FeeChoice::Rent => rent,
            FeeChoice::RentPlus1 => rent + 1,
            FeeChoice::Max => MAX_FEE,
            FeeChoice::MaxPlus1 => MAX_FEE + 1,
            FeeChoice::U64Max => u64::MAX,
            FeeChoice::InRange(x) => *x,
        }
    }
}

fn fee_choice() -> impl Strategy<Value = FeeChoice> {
    prop_oneof![
        Just(FeeChoice::Zero),
        Just(FeeChoice::One),
        Just(FeeChoice::RentMinus1),
        Just(FeeChoice::Rent),
        Just(FeeChoice::RentPlus1),
        Just(FeeChoice::Max),
        Just(FeeChoice::MaxPlus1),
        Just(FeeChoice::U64Max),
        any::<u64>().prop_map(FeeChoice::InRange),
    ]
}

/// Admin choices: keys 0..3 of a known pool (0 = the env admin), or 3 = a dead key
/// (`Pubkey::default()`), which nobody can sign for.
const DEAD: usize = 3;

#[derive(Clone, Debug)]
enum Op {
    Update {
        signer: usize,
        admin: Option<usize>,
        paused: Option<bool>,
        fee: Option<FeeChoice>,
        recipient: Option<u8>,
        treasury: Option<u8>,
    },
    Lift,
    Freeze,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (
            0usize..3,
            proptest::option::of(0usize..4),
            proptest::option::of(any::<bool>()),
            proptest::option::weighted(0.5, fee_choice()),
            proptest::option::of(any::<u8>()),
            proptest::option::of(any::<u8>()),
        )
            .prop_map(|(signer, admin, paused, fee, recipient, treasury)| Op::Update {
                signer,
                admin,
                paused,
                fee,
                recipient,
                treasury,
            }),
        2 => Just(Op::Lift),
        1 => Just(Op::Freeze),
    ]
}

#[derive(Clone, Debug, PartialEq)]
struct Model {
    admin: usize,
    paused: bool,
    fee: u64,
    recipient: Pubkey,
    treasury: Pubkey,
    frozen: bool,
}

fn key_from(byte: u8) -> Pubkey {
    Pubkey::new_from_array([byte; 32])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// INV-ADM-1 + INV-ADM-3: random sequences of updates (any signer, any bundle of
    /// fields), lifts and freezes. Each step's acceptance and error must match the
    /// router's rules, the stored config must match the model, the fee is always 0 or in
    /// [rent_min(0), 1e12], and once frozen and unpaused the router never pauses again.
    #[test]
    fn bok_inv_adm_1_3_config_state_machine(ops in proptest::collection::vec(op(), 1..14)) {
        let mut env = Env::new();
        let rent = rent_min_0(&env);
        let pool = [Keypair::new(), Keypair::new()];
        let pool_key = |i: usize, env: &Env| match i {
            0 => env.admin.pubkey(),
            1 | 2 => pool[i - 1].pubkey(),
            _ => Pubkey::default(),
        };
        let start = config(&env);
        let mut model = Model {
            admin: 0,
            paused: false,
            fee: start.listing_fee_lamports,
            recipient: start.listing_fee_recipient,
            treasury: start.treasury_owner,
            frozen: false,
        };
        let mut unpaused_while_frozen = false;

        for op in ops {
            match op.clone() {
                Op::Update { signer, admin, paused, fee, recipient, treasury } => {
                    let fee_value = fee.as_ref().map(|f| f.value(rent));
                    let ix = client::update_config(
                        &pool_key(signer, &env),
                        &client::UpdateConfigArgs {
                            admin: admin.map(|a| pool_key(a, &env)),
                            paused,
                            listing_fee_lamports: fee_value,
                            listing_fee_recipient: recipient.map(key_from),
                            treasury_owner: treasury.map(key_from),
                        },
                    );
                    let result = if signer == 0 {
                        env.send(&[ix], &[])
                    } else {
                        let keypair = &pool[signer - 1];
                        env.send(&[ix], &[keypair])
                    };
                    let expected_error = if signer != model.admin {
                        Some("NotAdmin")
                    } else if paused == Some(true) && model.frozen {
                        Some("FrozenRouterCannotPause")
                    } else {
                        fee_value.and_then(|f| fee_error(f, rent))
                    };
                    match expected_error {
                        Some(error) => assert_fails_with(result, error),
                        None => {
                            let meta = result.map_err(|e| {
                                TestCaseError::fail(format!("{op:?} should succeed: {:?}", e.err))
                            })?;
                            if let Some(a) = admin {
                                model.admin = a;
                            }
                            if let Some(p) = paused {
                                model.paused = p;
                            }
                            if let Some(f) = fee_value {
                                model.fee = f;
                            }
                            if let Some(r) = recipient {
                                model.recipient = key_from(r);
                            }
                            if let Some(t) = treasury {
                                model.treasury = key_from(t);
                            }
                            assert_event_matches_config(&meta, &config(&env));
                        }
                    }
                }
                Op::Lift => {
                    let result = env.send(&[client::lift_pause()], &[]);
                    if model.frozen {
                        let meta = result.map_err(|e| {
                            TestCaseError::fail(format!("lift on a frozen router: {:?}", e.err))
                        })?;
                        model.paused = false;
                        assert_event_matches_config(&meta, &config(&env));
                    } else {
                        assert_fails_with(result, "RouterNotFrozen");
                    }
                }
                Op::Freeze => {
                    if !model.frozen {
                        let admin = env.admin.pubkey();
                        env.send(&[loader_set_authority(&admin, None)], &[])
                            .expect("freeze");
                        model.frozen = true;
                    }
                }
            }

            let stored = config(&env);
            let model_admin = if model.admin == DEAD {
                Pubkey::default()
            } else {
                pool_key(model.admin, &env)
            };
            prop_assert_eq!(stored.admin, model_admin);
            prop_assert_eq!(stored.paused, model.paused);
            prop_assert_eq!(stored.listing_fee_lamports, model.fee);
            prop_assert_eq!(stored.listing_fee_recipient, model.recipient);
            prop_assert_eq!(stored.treasury_owner, model.treasury);
            prop_assert_eq!(router_frozen(&env), model.frozen);
            let fee = stored.listing_fee_lamports;
            prop_assert!(fee == 0 || (rent..=MAX_FEE).contains(&fee), "fee {} out of bounds", fee);
            if model.frozen && !stored.paused {
                unpaused_while_frozen = true;
            }
            if unpaused_while_frozen {
                prop_assert!(!stored.paused, "a frozen, unpaused router was paused again");
            }
        }
    }
}
