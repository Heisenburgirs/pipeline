//! SOS audit #2, H001: the freeze-aware pause (recheck of audit #1 H007).
//!
//! The pause power ends at the freeze: `paused` can become true only while the router has an upgrade
//! authority, and once it is frozen anyone can clear it with `lift_pause`. "Frozen" is
//! read from `router_data`, which must be the router's own ProgramData. These tests
//! check that binding against foreign and crafted ProgramData accounts, the ordering of
//! pause, freeze, lift and swap inside one transaction, the H007 dead-admin scenario,
//! and which instructions the pause does and does not block.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
/// The evil strategy's market state length (four keys, a bump and an info mode).
const STATE_LEN: usize = 32 * 4 + 2;
const HONEST: u8 = 0;

fn config(env: &Env) -> client::Config {
    client::decode_config(&env.account_data(&pda::config())).expect("config")
}

fn router_frozen(env: &Env) -> bool {
    env.account_data(&pda::program_data(&client::PROGRAM_ID))[12] == 0
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

/// Replaces the `router` (index `router_at`) and `router_data` (the next index)
/// accounts of an `update_config` or `lift_pause` instruction.
fn with_router_accounts(
    mut ix: Instruction,
    router_at: usize,
    router: Pubkey,
    router_data: Pubkey,
) -> Instruction {
    assert_eq!(ix.accounts[router_at].pubkey, client::PROGRAM_ID);
    assert_eq!(
        ix.accounts[router_at + 1].pubkey,
        pda::program_data(&client::PROGRAM_ID)
    );
    ix.accounts[router_at].pubkey = router;
    ix.accounts[router_at + 1].pubkey = router_data;
    ix
}

/// `update_config` puts `router` at index 2; `lift_pause` at index 1.
const UPDATE_CONFIG_ROUTER_AT: usize = 2;
const LIFT_PAUSE_ROUTER_AT: usize = 1;

/// A loader-owned account at a fresh address holding a ProgramData header with the
/// given upgrade authority (crafted with `set_account`; nobody could create one on a
/// real cluster, but it isolates the address binding from the owner check).
fn craft_program_data(env: &mut Env, authority: Option<&Pubkey>) -> Pubkey {
    let address = Pubkey::new_unique();
    let mut data = Vec::with_capacity(45);
    data.extend_from_slice(&3u32.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());
    match authority {
        Some(key) => {
            data.push(1);
            data.extend_from_slice(key.as_ref());
        }
        None => {
            data.push(0);
            data.extend_from_slice(&[0u8; 32]);
        }
    }
    let lamports = env.svm.minimum_balance_for_rent_exemption(data.len());
    env.svm
        .set_account(
            address,
            solana_account::Account {
                lamports,
                data,
                owner: client::BPF_LOADER_UPGRADEABLE_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    address
}

fn send_as(env: &mut Env, payer: &Keypair, instructions: &[Instruction]) -> TxResult {
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        instructions,
        Some(&payer.pubkey()),
        &[payer],
        env.svm.latest_blockhash(),
    );
    env.svm.send_transaction(tx)
}

/// A CP market with liquidity and a trader holding `mint_a`, plus the swap instruction.
fn cp_trade(env: &mut Env) -> (CpMarket, Keypair, Instruction) {
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

fn trader_output_balance(env: &Env, market: &CpMarket, trader: &Keypair) -> u64 {
    env.token_balance(&pda::associated_token(
        &trader.pubkey(),
        &market.mint_b,
        &TOKEN,
    ))
}

// ---------------------------------------------------------------------------------------
// (a), (b): `router` and `router_data` binding
// ---------------------------------------------------------------------------------------

/// On an upgradeable, paused router, `lift_pause` refuses a frozen strategy's
/// ProgramData, the frozen strategy itself as `router`, and a crafted frozen ProgramData;
/// the pause stays in place. The honest call is refused with `RouterNotFrozen`.
#[test]
fn h001_lift_pause_cannot_borrow_a_frozen_programdata() {
    let mut env = Env::new();
    let admin = env.admin.pubkey();
    env.deploy_strategy(&EVIL_ID, "test_strategy_evil", &admin);
    set_upgrade_authority(&mut env.svm, &EVIL_ID, None);
    let evil_data = pda::program_data(&EVIL_ID);
    assert_eq!(
        env.account_data(&evil_data)[12],
        0,
        "setup: strategy frozen"
    );

    env.send(&[pause_ix(&admin, true)], &[])
        .expect("setup: pause while upgradeable");
    assert!(config(&env).paused && !router_frozen(&env));

    let spoof = with_router_accounts(
        client::lift_pause(),
        LIFT_PAUSE_ROUTER_AT,
        client::PROGRAM_ID,
        evil_data,
    );
    assert_fails_with(env.send(&[spoof], &[]), "ConstraintRaw");

    let spoof = with_router_accounts(
        client::lift_pause(),
        LIFT_PAUSE_ROUTER_AT,
        EVIL_ID,
        evil_data,
    );
    assert_fails_with(env.send(&[spoof], &[]), "InvalidProgramId");

    let crafted = craft_program_data(&mut env, None);
    let spoof = with_router_accounts(
        client::lift_pause(),
        LIFT_PAUSE_ROUTER_AT,
        client::PROGRAM_ID,
        crafted,
    );
    assert_fails_with(env.send(&[spoof], &[]), "ConstraintRaw");

    assert_fails_with(env.send(&[client::lift_pause()], &[]), "RouterNotFrozen");
    assert!(
        config(&env).paused,
        "H001: a foreign ProgramData lifted the pause of an upgradeable router"
    );
}

/// On a frozen router, `update_config(paused = true)` refuses an upgradeable strategy's
/// ProgramData, the upgradeable strategy itself as `router`, and a crafted upgradeable
/// ProgramData; the router stays unpaused. The honest call fails with
/// `FrozenRouterCannotPause`.
#[test]
fn h001_frozen_router_cannot_pause_with_a_borrowed_programdata() {
    let mut env = Env::new();
    let admin = env.admin.pubkey();
    env.deploy_strategy(&CP_PROGRAM_ID, "test_strategy_cp", &admin);
    let cp_data = pda::program_data(&CP_PROGRAM_ID);
    assert_eq!(
        env.account_data(&cp_data)[12],
        1,
        "setup: strategy upgradeable"
    );

    env.send(&[loader_set_authority(&admin, None)], &[])
        .expect("setup: freeze the router through the loader");
    assert!(router_frozen(&env) && !config(&env).paused);

    let spoof = with_router_accounts(
        pause_ix(&admin, true),
        UPDATE_CONFIG_ROUTER_AT,
        client::PROGRAM_ID,
        cp_data,
    );
    assert_fails_with(env.send(&[spoof], &[]), "ConstraintRaw");

    let spoof = with_router_accounts(
        pause_ix(&admin, true),
        UPDATE_CONFIG_ROUTER_AT,
        CP_PROGRAM_ID,
        cp_data,
    );
    assert_fails_with(env.send(&[spoof], &[]), "InvalidProgramId");

    let crafted = craft_program_data(&mut env, Some(&admin));
    let spoof = with_router_accounts(
        pause_ix(&admin, true),
        UPDATE_CONFIG_ROUTER_AT,
        client::PROGRAM_ID,
        crafted,
    );
    assert_fails_with(env.send(&[spoof], &[]), "ConstraintRaw");

    assert_fails_with(
        env.send(&[pause_ix(&admin, true)], &[]),
        "FrozenRouterCannotPause",
    );
    assert!(
        !config(&env).paused,
        "H001: a frozen router was paused through a foreign ProgramData"
    );
    // The admin keeps its other powers after the freeze.
    let ix = client::update_config(
        &admin,
        &client::UpdateConfigArgs {
            listing_fee_lamports: Some(0),
            ..Default::default()
        },
    );
    env.send(&[ix], &[])
        .expect("frozen router: non-pause update");
    assert_eq!(config(&env).listing_fee_lamports, 0);
}

// ---------------------------------------------------------------------------------------
// (d): ordering inside one transaction
// ---------------------------------------------------------------------------------------

/// [pause, freeze, swap] fails with `Paused` and reverts the pause and the freeze;
/// [pause, freeze, lift_pause, swap] succeeds and leaves a frozen, unpaused router that
/// can never be paused again.
#[test]
fn h001_pause_freeze_lift_and_swap_in_one_transaction() {
    let mut env = Env::new();
    let admin = env.admin.pubkey();
    let (market, trader, swap) = cp_trade(&mut env);

    let ixs = [
        pause_ix(&admin, true),
        loader_set_authority(&admin, None),
        swap.clone(),
    ];
    assert_fails_with(env.send(&ixs, &[&trader]), "Paused");
    assert!(
        !router_frozen(&env),
        "the failed transaction reverts the freeze"
    );
    assert!(
        !config(&env).paused,
        "the failed transaction reverts the pause"
    );
    assert_eq!(trader_output_balance(&env, &market, &trader), 0);

    let ixs = [
        pause_ix(&admin, true),
        loader_set_authority(&admin, None),
        client::lift_pause(),
        swap,
    ];
    env.send(&ixs, &[&trader])
        .expect("H001: a pause left at the freeze could not be lifted in the same transaction");
    assert!(router_frozen(&env));
    assert!(!config(&env).paused);
    assert!(trader_output_balance(&env, &market, &trader) > 0);

    assert_fails_with(
        env.send(&[pause_ix(&admin, true)], &[]),
        "FrozenRouterCannotPause",
    );
}

/// The H007 scenario under the fix: pause while upgradeable, hand the admin role to
/// `Pubkey::default()`, then freeze. The old admin can no longer unpause, but a
/// stranger's `lift_pause` clears the pause and swaps work again.
#[test]
fn h001_h007_dead_admin_pause_is_lifted_by_anyone_after_the_freeze() {
    let mut env = Env::new();
    let admin = env.admin.pubkey();
    let (market, trader, swap) = cp_trade(&mut env);

    env.send(&[pause_ix(&admin, true)], &[])
        .expect("setup: pause");
    let ix = client::update_config(
        &admin,
        &client::UpdateConfigArgs {
            admin: Some(Pubkey::default()),
            ..Default::default()
        },
    );
    env.send(&[ix], &[]).expect("setup: admin to a dead key");
    env.send(&[loader_set_authority(&admin, None)], &[])
        .expect("setup: freeze");
    assert!(config(&env).paused && router_frozen(&env));
    assert_fails_with(env.send(&[pause_ix(&admin, false)], &[]), "NotAdmin");
    assert_fails_with(env.send(std::slice::from_ref(&swap), &[&trader]), "Paused");

    let stranger = env.funded_keypair();
    send_as(&mut env, &stranger, &[client::lift_pause()])
        .expect("H001: a stranger could not lift a pause on a frozen router");
    assert!(!config(&env).paused);
    env.send(&[swap], &[&trader])
        .expect("H001: swaps stay blocked after lift_pause");
    assert!(trader_output_balance(&env, &market, &trader) > 0);
}

// ---------------------------------------------------------------------------------------
// (c): the gate matrix
// ---------------------------------------------------------------------------------------

/// An evil market state written directly, as if the strategy had created it itself.
fn plant_evil_state(env: &mut Env) -> Pubkey {
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    let mut data = Vec::with_capacity(STATE_LEN);
    for key in [mint_a, mint_b, vault_a, vault_b] {
        data.extend_from_slice(key.as_ref());
    }
    data.push(255);
    data.push(HONEST);
    let lamports = env.svm.minimum_balance_for_rent_exemption(STATE_LEN);
    env.svm
        .set_account(
            state,
            solana_account::Account {
                lamports,
                data,
                owner: EVIL_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    state
}

/// While paused, exactly `register_strategy`, `create_market`,
/// `register_existing_market` and `swap` fail with `Paused`; `update_config`,
/// `update_metadata`, `init_fee_account` and `sweep_fees` keep working (`lift_pause`
/// is refused only because the router is not frozen). After unpausing, each blocked
/// call succeeds as built, so the failures were the pause and nothing else.
#[test]
fn h001_pause_blocks_exactly_the_four_gated_instructions() {
    let mut env = Env::new();
    let admin = env.admin.pubkey();
    let (market, trader, swap) = cp_trade(&mut env);
    env.send(std::slice::from_ref(&swap), &[&trader])
        .expect("setup: a swap so there are fees to sweep");
    let fee_account = pda::fee_account(&market.market, &market.mint_a, &TOKEN);
    let collected = env.token_balance(&fee_account);
    assert!(collected > 0, "setup: fees collected");
    let treasury_owner = env.treasury_owner;
    let treasury = env.create_ata(&treasury_owner, &market.mint_a, &TOKEN);

    let evil_owner = env.register(&EVIL_ID, "test_strategy_evil");
    let planted = plant_evil_state(&mut env);

    // A second CP market without fee accounts, for `init_fee_account` while paused.
    let mint_c = env.create_mint(&TOKEN, 6, None, None);
    let mint_d = env.create_mint(&TOKEN, 6, None, None);
    let creator = env.funded_keypair();
    env.send(
        &[cp_create_market_ix(
            &creator.pubkey(),
            &mint_c,
            &TOKEN,
            &mint_d,
            &TOKEN,
            30,
        )],
        &[&creator],
    )
    .expect("setup: second market");
    let second_market = pda::market(&CP_PROGRAM_ID, &cp_pool(&mint_c, &mint_d, 30));
    let second_fee_account = pda::fee_account(&second_market, &mint_c, &TOKEN);
    assert!(env.svm.get_account(&second_fee_account).is_none());

    // A new strategy program to register, and mints for a new CP market.
    let new_program = Pubkey::new_unique();
    let new_owner = env.funded_keypair();
    env.deploy_strategy(&new_program, "test_strategy_cp", &new_owner.pubkey());
    let register = client::register_strategy(
        &new_owner.pubkey(),
        &new_owner.pubkey(),
        &new_program,
        &env.listing_fee_recipient,
        "https://example.com/new.json",
        u64::MAX,
    );
    let mint_e = env.create_mint(&TOKEN, 6, None, None);
    let mint_f = env.create_mint(&TOKEN, 6, None, None);
    let create = cp_create_market_ix(&creator.pubkey(), &mint_e, &TOKEN, &mint_f, &TOKEN, 30);
    let register_existing =
        client::register_existing_market(&creator.pubkey(), &EVIL_ID, &planted, vec![]);

    env.send(&[pause_ix(&admin, true)], &[])
        .expect("setup: pause");
    assert!(config(&env).paused);

    // Gated.
    assert_fails_with(
        env.send(std::slice::from_ref(&register), &[&new_owner]),
        "Paused",
    );
    assert_fails_with(
        env.send(std::slice::from_ref(&create), &[&creator]),
        "Paused",
    );
    assert_fails_with(
        env.send(std::slice::from_ref(&register_existing), &[&creator]),
        "Paused",
    );
    assert_fails_with(env.send(std::slice::from_ref(&swap), &[&trader]), "Paused");

    // Not gated.
    let metadata = client::update_metadata(
        &evil_owner.pubkey(),
        &EVIL_ID,
        "https://example.com/paused.json",
    );
    env.send(&[metadata], &[&evil_owner])
        .expect("H001: update_metadata blocked by the pause");
    env.send(
        &[client::init_fee_account(
            &admin,
            &second_market,
            &mint_c,
            &TOKEN,
        )],
        &[],
    )
    .expect("H001: init_fee_account blocked by the pause");
    assert!(env.svm.get_account(&second_fee_account).is_some());
    env.send(
        &[client::sweep_fees(
            &market.market,
            &market.mint_a,
            &TOKEN,
            &treasury_owner,
            vec![],
        )],
        &[],
    )
    .expect("H001: sweep_fees blocked by the pause");
    assert_eq!(env.token_balance(&treasury), collected);
    let fee_change = client::update_config(
        &admin,
        &client::UpdateConfigArgs {
            listing_fee_lamports: Some(LISTING_FEE),
            ..Default::default()
        },
    );
    env.send(&[fee_change], &[])
        .expect("H001: update_config blocked by the pause");
    assert_fails_with(env.send(&[client::lift_pause()], &[]), "RouterNotFrozen");
    assert!(config(&env).paused);

    // Controls: the same blocked calls succeed once unpaused.
    env.send(&[pause_ix(&admin, false)], &[]).expect("unpause");
    env.send(&[register], &[&new_owner])
        .expect("control: register_strategy");
    env.send(&[create], &[&creator])
        .expect("control: create_market");
    env.send(&[register_existing], &[&creator])
        .expect("control: register_existing_market");
    env.send(&[swap], &[&trader]).expect("control: swap");
}
