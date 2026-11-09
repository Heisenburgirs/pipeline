//! SOS audit #2, H021: repeating router accounts in the strategy's accounts (the market
//! PDA, config, strategy, fee authority, event authority) widens what the strategy is
//! handed. The router copies each account's transaction-wide writable flag, and the
//! runtime ORs the flags of duplicate metas, so the market PDA reaches the strategy as a
//! writable signer (swap: when the caller marks it writable; create_market: always, as
//! `market` is `init`).
//!
//! The question is whether that matters. Every router-owned account is unwritable and
//! undebitable by another program, and the system-owned PDAs (`fee_authority`,
//! `event_authority`) are never signed for the strategy. These tests pin:
//!   * the widening is real: a writable CPI that names the router account is refused as a
//!     privilege escalation when the caller left it read-only, and gets as far as the
//!     token program when the caller marked it writable (and, in `create_market`, with
//!     the market as signing authority too);
//!   * it gains nothing: no router account changes, no fees move, honest calls still work.
//!
//! Not testable with the prebuilt fixtures: a strategy that tries `system::transfer` /
//! `assign` / `allocate` with the market as `from`. That rests on the runtime rule that only
//! an account's owner debits it or changes its data, and that the system program requires
//! `from` to be system-owned; see the finding for the reasoning.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

/// Evil strategy swap behaviours (programs/test-strategy-evil/src/lib.rs).
const HONEST: u8 = 0;
const WRONG_DESTINATION: u8 = 5;
const STEAL_FEES: u8 = 6;
/// Evil init mode: Approve account 8 to the state PDA with account 9 as the authority.
const INIT_APPROVES_CREATOR_TOKENS: u8 = 7;

const START: u64 = 10_000_000;
const AMOUNT: u64 = 1_000;

struct EvilMarket {
    state: Pubkey,
    market: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
}

fn evil_pda(mint_a: &Pubkey, mint_b: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID)
}

fn setup() -> (Env, EvilMarket) {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let (state, _) = evil_pda(&mint_a, &mint_b);
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    let creator = env.funded_keypair();
    let ix = client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
        strategy_program: EVIL_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![0],
        max_creator_lamports: MAX_SETUP_LAMPORTS,
        strategy_accounts: vec![
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
        ],
    });
    env.send(&[ix], &[&creator]).expect("create evil market");
    let m = EvilMarket {
        state,
        market: pda::market(&EVIL_ID, &state),
        mint_a,
        mint_b,
        vault_a,
        vault_b,
    };
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &m.market, &m.mint_a, &TOKEN),
            client::init_fee_account(&payer, &m.market, &m.mint_b, &TOKEN),
        ],
        &[],
    )
    .expect("init fee accounts");
    env.mint_to(&m.mint_b, &TOKEN, &m.vault_b, 1_000_000_000);
    (env, m)
}

fn trader_accounts(env: &mut Env, m: &EvilMarket, owner: &Pubkey) -> (Pubkey, Pubkey) {
    let a = env.create_ata(owner, &m.mint_a, &TOKEN);
    env.mint_to(&m.mint_a, &TOKEN, &a, START);
    let b = env.create_ata(owner, &m.mint_b, &TOKEN);
    (a, b)
}

fn swap_ix(
    m: &EvilMarket,
    trader: &Pubkey,
    trader_a: &Pubkey,
    trader_b: &Pubkey,
    behaviour: u8,
    extra: Vec<AccountMeta>,
) -> Instruction {
    let mut strategy_accounts = vec![AccountMeta::new(m.vault_b, false)];
    strategy_accounts.extend(extra);
    let accounts = client::SwapAccounts {
        trader: *trader,
        strategy_program: EVIL_ID,
        market_state: m.state,
        trader_input: *trader_a,
        trader_output: *trader_b,
        input_vault: m.vault_a,
        input_mint: m.mint_a,
        output_mint: m.mint_b,
        input_token_program: TOKEN,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts,
        input_hook_accounts: vec![],
    };
    let mut args = exact_in(AMOUNT, 0);
    args.extra = vec![behaviour];
    client::swap(&accounts, &args)
}

/// Everything about an account that another program must not be able to change.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    lamports: u64,
    owner: Pubkey,
    data: Vec<u8>,
}

/// A PDA that holds nothing (`fee_authority`, `event_authority`) does not exist in the
/// SVM; that is recorded as zero lamports, system owner, no data.
fn snapshot(env: &Env, address: &Pubkey) -> Snapshot {
    match env.svm.get_account(address) {
        Some(account) => Snapshot {
            lamports: account.lamports,
            owner: account.owner,
            data: account.data,
        },
        None => Snapshot {
            lamports: 0,
            owner: client::SYSTEM_PROGRAM_ID,
            data: vec![],
        },
    }
}

/// The failure text (error and logs) of a transaction that must have failed.
fn failure_text(result: TxResult, case: &str) -> String {
    match result {
        Ok(meta) => panic!(
            "H021 VIOLATED ({case}): the hostile strategy's call succeeded, so it wrote to \
             a router account it was handed:\n{}",
            meta.logs.join("\n")
        ),
        Err(failed) => format!("{:?}\n{}", failed.err, failed.meta.logs.join("\n")),
    }
}

// ---------------------------------------------------------------------------------------
// swap
// ---------------------------------------------------------------------------------------

/// The router accounts that `swap` knows: market, config, fee authority, event authority.
fn router_accounts(m: &EvilMarket) -> Vec<(&'static str, Pubkey)> {
    vec![
        ("market", m.market),
        ("config", pda::config()),
        ("fee_authority", pda::fee_authority(&m.market)),
        ("event_authority", pda::event_authority()),
    ]
}

/// Control and invariant: an honest swap with the market, config, fee authority and event
/// authority all repeated, writable, in the strategy's accounts still succeeds, and none
/// of those accounts changes (bytes, lamports, owner).
#[test]
fn h021_swap_duplicated_router_accounts_honest_swap_changes_nothing() {
    let (mut env, m) = setup();
    let trader = env.funded_keypair();
    let key = trader.pubkey();
    let (a, b) = trader_accounts(&mut env, &m, &key);
    let routers = router_accounts(&m);
    let before: Vec<Snapshot> = routers.iter().map(|(_, k)| snapshot(&env, k)).collect();

    // Setup check: the market is router-owned with data; fee_authority is a system-owned,
    // dataless PDA.
    assert_eq!(before[0].owner, client::PROGRAM_ID);
    assert!(!before[0].data.is_empty());
    assert_eq!(snapshot(&env, &pda::fee_authority(&m.market)).data.len(), 0);

    let extras: Vec<AccountMeta> = routers
        .iter()
        .map(|(_, k)| AccountMeta::new(*k, false))
        .collect();
    let ix = swap_ix(&m, &key, &a, &b, HONEST, extras);
    env.send(&[ix], &[&trader])
        .expect("setup: an honest swap with duplicated router accounts succeeds");
    assert_eq!(env.token_balance(&b), AMOUNT, "setup: the swap paid out");

    for ((name, address), before) in routers.iter().zip(before) {
        assert_eq!(
            snapshot(&env, address),
            before,
            "H021: the {name} account changed during a swap that repeated it writable"
        );
    }
}

/// Pins the widening: a strategy's writable CPI naming a router account (WrongDestination
/// sends the output tokens to the first extra account, a writable `to` meta).
///   * the caller leaves it read-only: refused as a privilege escalation;
///   * the caller marks it writable (the duplicate meta's flag is OR-ed): the strategy does
///     hold it writable, so the runtime check passes, and the token program refuses an
///     account it does not own. Either way nothing changes.
#[test]
fn h021_swap_writable_flag_follows_the_caller_and_gains_nothing() {
    let (mut env, m) = setup();
    let trader = env.funded_keypair();
    let key = trader.pubkey();
    let (a, b) = trader_accounts(&mut env, &m, &key);

    // Setup check: an honest swap works and gives a baseline.
    let honest = swap_ix(&m, &key, &a, &b, HONEST, vec![]);
    env.send(&[honest], &[&trader]).expect("setup: honest swap");
    let after_a = env.token_balance(&a);
    let after_b = env.token_balance(&b);

    for (name, victim) in router_accounts(&m) {
        let before = snapshot(&env, &victim);

        // Read-only for the whole transaction.
        let ix = swap_ix(
            &m,
            &key,
            &a,
            &b,
            WRONG_DESTINATION,
            vec![AccountMeta::new_readonly(victim, false)],
        );
        let text = failure_text(env.send(&[ix], &[&trader]), name);
        assert!(
            text.contains("writable privilege escalated"),
            "H021 ({name}, read-only): expected a writable privilege escalation, got:\n{text}"
        );

        // Marked writable: the strategy holds it writable.
        let ix = swap_ix(
            &m,
            &key,
            &a,
            &b,
            WRONG_DESTINATION,
            vec![AccountMeta::new(victim, false)],
        );
        let text = failure_text(env.send(&[ix], &[&trader]), name);
        println!("--- {name} writable: {}", text.lines().next().unwrap());
        assert!(
            !text.contains("privilege escalated"),
            "H021 ({name}, writable): expected the CPI to pass the runtime privilege check \
             (the widening), got:\n{text}"
        );
        assert!(
            text.contains(&format!("Program {TOKEN} invoke")),
            "H021 ({name}, writable): expected the token program to be reached, got:\n{text}"
        );

        assert_eq!(snapshot(&env, &victim), before, "H021: {name} changed");
        assert_eq!(env.token_balance(&a), after_a, "H021 ({name})");
        assert_eq!(env.token_balance(&b), after_b, "H021 ({name})");
    }
}

/// StealFees pays out, then tries to move fee-account tokens with the market signature
/// (the only signature the strategy holds). With the market, fee authority and config
/// repeated writable, and the fee account itself named, it still cannot: the fee account's
/// owner is the fee-authority PDA, not the market.
#[test]
fn h021_swap_fee_theft_with_duplicated_router_accounts_fails() {
    let (mut env, m) = setup();
    let trader = env.funded_keypair();
    let key = trader.pubkey();
    let (a, b) = trader_accounts(&mut env, &m, &key);
    let fee_account = pda::fee_account(&m.market, &m.mint_a, &TOKEN);
    let fee_authority = pda::fee_authority(&m.market);

    let honest = swap_ix(&m, &key, &a, &b, HONEST, vec![]);
    env.send(&[honest], &[&trader]).expect("setup: honest swap");
    let collected = env.token_balance(&fee_account);
    assert!(collected > 0, "setup: no fee collected");
    let watched = [
        m.market,
        pda::config(),
        fee_authority,
        pda::event_authority(),
    ];
    let before: Vec<Snapshot> = watched.iter().map(|k| snapshot(&env, k)).collect();

    let extras = vec![
        AccountMeta::new(fee_account, false),
        AccountMeta::new(m.market, false),
        AccountMeta::new(fee_authority, false),
        AccountMeta::new(pda::config(), false),
        AccountMeta::new(pda::event_authority(), false),
    ];
    let ix = swap_ix(&m, &key, &a, &b, STEAL_FEES, extras);
    let text = failure_text(env.send(&[ix], &[&trader]), "StealFees");
    assert!(
        text.contains("owner does not match"),
        "H021: expected the token program to refuse the fee transfer, got:\n{text}"
    );
    assert_eq!(
        env.token_balance(&fee_account),
        collected,
        "H021: fees moved"
    );
    for (address, before) in watched.iter().zip(before) {
        assert_eq!(snapshot(&env, address), before, "H021: {address} changed");
    }
}

// ---------------------------------------------------------------------------------------
// create_market
// ---------------------------------------------------------------------------------------

fn evil_create_ix(
    creator: &Pubkey,
    mints: (Pubkey, Pubkey),
    state: Pubkey,
    vaults: (Pubkey, Pubkey),
    mode: u8,
    tail: Vec<AccountMeta>,
) -> Instruction {
    let mut strategy_accounts = vec![
        AccountMeta::new_readonly(mints.0, false),
        AccountMeta::new_readonly(mints.1, false),
        AccountMeta::new(vaults.0, false),
        AccountMeta::new(vaults.1, false),
    ];
    strategy_accounts.extend(tail);
    client::create_market(&client::CreateMarket {
        creator: *creator,
        strategy_program: EVIL_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![mode],
        max_creator_lamports: MAX_SETUP_LAMPORTS,
        strategy_accounts,
    })
}

struct CreateParts {
    mints: (Pubkey, Pubkey),
    state: Pubkey,
    vaults: (Pubkey, Pubkey),
}

fn create_parts(env: &mut Env) -> CreateParts {
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let (state, _) = evil_pda(&mint_a, &mint_b);
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    CreateParts {
        mints: (mint_a, mint_b),
        state,
        vaults: (vault_a, vault_b),
    }
}

/// A market creation that repeats the market, config, strategy record and event authority
/// (all marked writable) in the strategy's accounts still succeeds with honest setup. The
/// config is unchanged; the strategy record and the market are exactly what an ordinary
/// creation would write.
#[test]
fn h021_create_market_duplicated_router_accounts_honest_setup_changes_nothing() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let creator = env.funded_keypair();
    let p = create_parts(&mut env);
    let market = pda::market(&EVIL_ID, &p.state);
    let strategy = pda::strategy(&EVIL_ID);
    let config = pda::config();
    let event_authority = pda::event_authority();

    let config_before = snapshot(&env, &config);
    let strategy_before = client::decode_strategy(&snapshot(&env, &strategy).data)
        .expect("setup: the strategy record decodes");
    assert_eq!(strategy_before.market_count, 0);

    let tail = vec![
        AccountMeta::new(market, false),
        AccountMeta::new(config, false),
        AccountMeta::new(strategy, false),
        AccountMeta::new(event_authority, false),
    ];
    let ix = evil_create_ix(&creator.pubkey(), p.mints, p.state, p.vaults, HONEST, tail);
    env.send(&[ix], &[&creator])
        .expect("setup: honest create_market with duplicated router accounts succeeds");

    assert_eq!(
        snapshot(&env, &config),
        config_before,
        "H021: config changed"
    );
    let strategy_after = client::decode_strategy(&snapshot(&env, &strategy).data).unwrap();
    assert_eq!(
        strategy_after.market_count, 1,
        "H021: strategy record changed beyond +1"
    );
    assert_eq!(strategy_after.program_id, strategy_before.program_id);
    assert_eq!(strategy_after.metadata_uri, strategy_before.metadata_uri);
    assert_eq!(strategy_after.registered_at, strategy_before.registered_at);

    let market_after = snapshot(&env, &market);
    assert_eq!(market_after.owner, client::PROGRAM_ID);
    let decoded = env.market(&market);
    assert_eq!(decoded.strategy_program, EVIL_ID);
    assert_eq!(decoded.market_state, p.state);
    assert_eq!(decoded.created_by, creator.pubkey());
    assert_eq!(decoded.mints(), &[p.mints.0, p.mints.1]);
}

/// Pins the widening in `create_market`, where `market` is writable through `init`: the
/// strategy's Approve names the market as the token account (account 8, a writable meta)
/// and as the authority (account 9, a signer meta). The runtime accepts both flags, which
/// is the writable-signer market, and the token program then refuses: the market is not a
/// token account. As a control, naming a router account that is not the market as the
/// authority is refused as a signer escalation.
#[test]
fn h021_create_market_market_is_writable_signer_but_unusable() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let creator = env.funded_keypair();
    let p = create_parts(&mut env);
    let market = pda::market(&EVIL_ID, &p.state);
    let config_before = snapshot(&env, &pda::config());

    // Control: config as the authority is not a signer.
    let tail = vec![
        AccountMeta::new(market, false),
        AccountMeta::new_readonly(pda::config(), false),
        AccountMeta::new_readonly(TOKEN, false),
    ];
    let ix = evil_create_ix(
        &creator.pubkey(),
        p.mints,
        p.state,
        p.vaults,
        INIT_APPROVES_CREATOR_TOKENS,
        tail,
    );
    let text = failure_text(env.send(&[ix], &[&creator]), "control: config authority");
    assert!(
        text.contains(&format!("{}'s signer privilege escalated", pda::config())),
        "H021 control: expected a signer escalation on the config, got:\n{text}"
    );
    assert!(
        !text.contains(&format!("{market}'s writable privilege escalated")),
        "H021 control: account 8 (the market) was refused as non-writable:\n{text}"
    );

    // The market as both the token account and the authority.
    let tail = vec![
        AccountMeta::new(market, false),
        AccountMeta::new_readonly(market, false),
        AccountMeta::new_readonly(TOKEN, false),
    ];
    let ix = evil_create_ix(
        &creator.pubkey(),
        p.mints,
        p.state,
        p.vaults,
        INIT_APPROVES_CREATOR_TOKENS,
        tail,
    );
    let text = failure_text(env.send(&[ix], &[&creator]), "market as writable signer");
    println!(
        "--- market writable signer: {}",
        text.lines().last().unwrap_or("")
    );
    assert!(
        !text.contains("privilege escalated"),
        "H021: expected the Approve to pass the runtime privilege check, got:\n{text}"
    );
    assert!(
        text.contains(&format!("Program {TOKEN} invoke")),
        "H021: expected the token program to be reached, got:\n{text}"
    );
    // The failed transaction is rolled back: no market, config untouched.
    assert!(env.svm.get_account(&market).is_none_or(|a| a.lamports == 0));
    assert_eq!(snapshot(&env, &pda::config()), config_before);
}
