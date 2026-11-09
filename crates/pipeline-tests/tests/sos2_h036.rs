//! SOS audit #2, H036: liveness inventory. Can a third party who only sends lamports (or
//! tokens) to addresses the router derives permanently block registration, market
//! creation, fee-account creation, swaps or fee sweeps for others?
//!
//! The levers a third party has are limited to what any system-program transfer or
//! token deposit allows: put lamports on an address before its account is created, or
//! deposit tokens into an account. Only the owning program can create data in a PDA or
//! close/freeze a token account, and the router never closes or freezes anything.
//!
//! Asserted here (all against the real router and token programs):
//! - Config, Strategy, Market, setup-payer, fee-authority PDAs and fee ATAs (SPL Token
//!   and Token-2022) pre-funded with lamports: initialisation, registration, market
//!   creation, fee-account creation, swap and sweep all still work;
//! - `register_existing_market` works with a pre-funded Market PDA;
//! - a third-party deposit into a fee account is swept like any fee;
//! - a missing treasury ATA makes `sweep_fees` fail, but anyone can create it (the ATA
//!   program is permissionless) and the sweep then succeeds: not a permanent block;
//! - the one pre-funding hazard is in a strategy, not the router: a strategy whose init
//!   creates its (deterministic) state PDA with a raw `create_account` is blocked for
//!   that state address by a pre-funded PDA. The in-repo evil test strategy is such a
//!   strategy; the playground pool (Anchor `init`) and strategies that transfer, allocate
//!   and assign are not. This is documented, not a router bug.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use litesvm::LiteSVM;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_strategy_interface as psi;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

/// Sends lamports to an address the way any third party can (a system transfer to an
/// address that doesn't exist yet), and checks the setup took.
fn prefund(env: &mut Env, address: &Pubkey, lamports: u64) {
    let sender = env.funded_keypair();
    let ix = client::wallet::system_transfer(&sender.pubkey(), address, lamports);
    env.send(&[ix], &[&sender]).expect("prefund transfer");
    let account = env.svm.get_account(address).expect("prefunded account");
    assert_eq!(account.lamports, lamports, "setup: prefund landed");
    assert!(account.data.is_empty(), "setup: prefunded account is empty");
    assert_eq!(
        account.owner,
        client::SYSTEM_PROGRAM_ID,
        "setup: prefunded account is system-owned"
    );
}

fn bare_env() -> Env {
    let mut svm = LiteSVM::new();
    let admin = Keypair::new();
    svm.airdrop(&admin.pubkey(), 1_000 * SOL).unwrap();
    svm.add_program_from_file(client::PROGRAM_ID, sbf_dir().join("pipeline.so"))
        .expect("build programs first");
    set_upgrade_authority(&mut svm, &client::PROGRAM_ID, Some(&admin.pubkey()));
    Env {
        svm,
        admin,
        listing_fee_recipient: Pubkey::new_unique(),
        treasury_owner: Pubkey::new_unique(),
    }
}

#[test]
fn prefunded_config_pda_does_not_block_initialize_config() {
    let mut env = bare_env();
    let config = pda::config();
    prefund(&mut env, &config, SOL);

    let ix = client::initialize_config(
        &env.admin.pubkey(),
        &client::ConfigArgs {
            listing_fee_lamports: LISTING_FEE,
            listing_fee_recipient: env.listing_fee_recipient,
            treasury_owner: env.treasury_owner,
        },
    );
    env.send(&[ix], &[])
        .expect("H036: a pre-funded Config PDA must not block initialize_config");
    let account = env.svm.get_account(&config).unwrap();
    assert_eq!(account.owner, client::PROGRAM_ID);
    assert!(client::decode_config(&account.data).is_some());
}

#[test]
fn prefunded_strategy_pda_does_not_block_registration() {
    let mut env = Env::new();
    let owner = env.funded_keypair();
    env.deploy_strategy(&EVIL_ID, "test_strategy_evil", &owner.pubkey());
    let strategy = pda::strategy(&EVIL_ID);
    prefund(&mut env, &strategy, SOL);

    let ix = client::register_strategy(
        &owner.pubkey(),
        &owner.pubkey(),
        &EVIL_ID,
        &env.listing_fee_recipient,
        "https://example.com/strategy.json",
        u64::MAX,
    );
    env.send(&[ix], &[&owner])
        .expect("H036: a pre-funded Strategy PDA must not block register_strategy");
    let account = env.svm.get_account(&strategy).unwrap();
    assert_eq!(account.owner, client::PROGRAM_ID);
    assert!(client::decode_strategy(&account.data).is_some());
}

/// Every router-derived address on a market's path, pre-funded before anything exists,
/// and the market still creates, takes fee accounts, trades and sweeps.
#[test]
fn prefunded_everything_still_creates_trades_and_sweeps() {
    let mut env = Env::new();
    env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN_2022, 9, None, None);

    let pool = cp_pool(&mint_a, &mint_b, 30);
    let market_key = pda::market(&CP_PROGRAM_ID, &pool);
    let fee_authority = pda::fee_authority(&market_key);
    let min0 = env.svm.minimum_balance_for_rent_exemption(0);
    // The strategy's pool state, the vault ATAs the strategy creates, the router's
    // Market, setup payer and fee authority, and both fee ATAs (one per token program).
    let targets = [
        (pool, SOL),
        (pda::associated_token(&pool, &mint_a, &TOKEN), SOL),
        (pda::associated_token(&pool, &mint_b, &TOKEN_2022), min0),
        (market_key, SOL),
        (pda::setup_payer(&market_key), SOL),
        (fee_authority, SOL),
        (pda::fee_account(&market_key, &mint_a, &TOKEN), SOL),
        (pda::fee_account(&market_key, &mint_b, &TOKEN_2022), min0),
    ];
    for (address, lamports) in targets {
        prefund(&mut env, &address, lamports);
    }

    let market = setup_cp_market_with_mints(
        &mut env,
        mint_a,
        TOKEN,
        mint_b,
        TOKEN_2022,
        1_000_000_000,
        2_000_000_000,
        30,
        vec![],
    );
    assert_eq!(market.market, market_key);
    let recorded = env.market(&market_key);
    assert_eq!(recorded.mints(), &[mint_a, mint_b]);
    for fee_account in [
        pda::fee_account(&market_key, &mint_a, &TOKEN),
        pda::fee_account(&market_key, &mint_b, &TOKEN_2022),
    ] {
        let account = env.svm.get_account(&fee_account).unwrap();
        assert!(
            account.owner == TOKEN || account.owner == TOKEN_2022,
            "fee ATA is a real token account"
        );
    }

    // A third party deposits tokens into the fee account before any swap (modelled by
    // minting straight into it: any holder of the mint can transfer in).
    let fee_a = pda::fee_account(&market_key, &mint_a, &TOKEN);
    env.mint_to(&mint_a, &TOKEN, &fee_a, 1_000);
    assert_eq!(env.token_balance(&fee_a), 1_000);

    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN_2022);
    let amount_in = 1_000_000;
    let expected_out = cp_out(1_000_000_000, 2_000_000_000, amount_in, 30);
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &mint_a,
        &trader_a,
        &mint_b,
        &trader_b,
    );
    let swap_ix = client::swap(&accounts, &exact_in(amount_in, expected_out));
    env.send(&[swap_ix], &[&trader])
        .expect("H036: swap with pre-funded fee ATA / market PDAs must work");
    let fee_balance = env.token_balance(&fee_a);
    assert!(
        fee_balance > 1_000,
        "fee was collected on top of the deposit"
    );

    // Sweep: the treasury ATA does not exist yet, so it fails; anyone can create it.
    let sweep = client::sweep_fees(&market_key, &mint_a, &TOKEN, &env.treasury_owner, vec![]);
    assert!(
        env.send(std::slice::from_ref(&sweep), &[]).is_err(),
        "setup: sweep needs the treasury ATA"
    );
    assert_eq!(
        env.token_balance(&fee_a),
        fee_balance,
        "setup: nothing moved"
    );
    let treasury_owner = env.treasury_owner;
    let treasury = env.create_ata(&treasury_owner, &mint_a, &TOKEN);
    env.send(&[sweep], &[])
        .expect("H036: sweep must work once anyone has created the treasury ATA");
    assert_eq!(env.token_balance(&fee_a), 0);
    assert_eq!(env.token_balance(&treasury), fee_balance);

    // And the Token-2022 side: fees in mint_b can be swept too.
    let fee_b = pda::fee_account(&market_key, &mint_b, &TOKEN_2022);
    env.mint_to(&mint_b, &TOKEN_2022, &fee_b, 777);
    let treasury_b = env.create_ata(&treasury_owner, &mint_b, &TOKEN_2022);
    let sweep_b = client::sweep_fees(&market_key, &mint_b, &TOKEN_2022, &treasury_owner, vec![]);
    env.send(&[sweep_b], &[])
        .expect("H036: Token-2022 sweep with pre-funded fee ATA must work");
    assert_eq!(env.token_balance(&treasury_b), 777);
}

struct Evil {
    mint_a: Pubkey,
    mint_b: Pubkey,
    state: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
}

fn evil_accounts(env: &mut Env) -> Evil {
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    Evil {
        mint_a,
        mint_b,
        state,
        vault_a,
        vault_b,
    }
}

fn evil_strategy_accounts(evil: &Evil) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(evil.mint_a, false),
        AccountMeta::new_readonly(evil.mint_b, false),
        AccountMeta::new(evil.vault_a, false),
        AccountMeta::new(evil.vault_b, false),
    ]
}

fn evil_create_ix(creator: &Pubkey, evil: &Evil) -> Instruction {
    client::create_market(&client::CreateMarket {
        creator: *creator,
        strategy_program: EVIL_ID,
        market_state: evil.state,
        market_state_signs: false,
        init_data: vec![0],
        max_creator_lamports: SOL,
        strategy_accounts: evil_strategy_accounts(evil),
    })
}

/// A direct call to the evil strategy's init, bypassing the router: an unregistered
/// strategy-owned state, as a graduation flow would leave.
fn evil_direct_init_ix(payer: &Pubkey, evil: &Evil) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new_readonly(*payer, false),
        AccountMeta::new(evil.state, false),
        AccountMeta::new(*payer, true),
        AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
    ];
    accounts.extend(evil_strategy_accounts(evil));
    Instruction::new_with_bytes(
        EVIL_ID,
        &psi::instruction_data(psi::discriminator::INIT_MARKET, &vec![0u8]),
        accounts,
    )
}

#[test]
fn prefunded_market_pda_does_not_block_register_existing_market() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let evil = evil_accounts(&mut env);
    let payer = env.admin.pubkey();
    env.send(&[evil_direct_init_ix(&payer, &evil)], &[])
        .expect("setup: direct init");
    let market = pda::market(&EVIL_ID, &evil.state);
    assert!(
        env.svm.get_account(&market).is_none(),
        "setup: unregistered"
    );
    prefund(&mut env, &market, SOL);

    let ix = client::register_existing_market(
        &env.admin.pubkey(),
        &EVIL_ID,
        &evil.state,
        vec![
            AccountMeta::new_readonly(evil.mint_a, false),
            AccountMeta::new_readonly(evil.mint_b, false),
            AccountMeta::new(evil.vault_a, false),
            AccountMeta::new(evil.vault_b, false),
        ],
    );
    env.send(&[ix], &[])
        .expect("H036: a pre-funded Market PDA must not block register_existing_market");
    assert_eq!(env.market(&market).mints(), &[evil.mint_a, evil.mint_b]);
}

#[test]
fn prefunded_setup_payer_leftover_returns_to_creator() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let evil = evil_accounts(&mut env);
    let market = pda::market(&EVIL_ID, &evil.state);
    let setup_payer = pda::setup_payer(&market);
    prefund(&mut env, &setup_payer, 5 * SOL);

    let creator = env.funded_keypair();
    let before = env.lamports(&creator.pubkey());
    env.send(&[evil_create_ix(&creator.pubkey(), &evil)], &[&creator])
        .expect("H036: a pre-funded setup payer must not block create_market");
    assert!(env.svm.get_account(&market).is_some());
    assert_eq!(env.lamports(&setup_payer), 0, "setup payer is emptied");
    // The creator paid the Market rent and the state rent but received the 5 SOL
    // donation back through the leftover sweep.
    assert!(
        env.lamports(&creator.pubkey()) > before.saturating_sub(SOL),
        "creator got the donation back"
    );
}

/// Documents the strategy-side hazard: the evil strategy creates its deterministic
/// state with a raw `create_account`, so pre-funding that PDA blocks that one state
/// address for that strategy. Not a router defect: the router forwards the market
/// PDA's signature, and a strategy that wants to survive pre-funding uses
/// transfer + allocate + assign, or Anchor `init` (the playground pool).
#[test]
fn prefunded_state_pda_blocks_only_raw_create_account_strategies() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let evil = evil_accounts(&mut env);
    prefund(&mut env, &evil.state, SOL);

    let creator = env.funded_keypair();
    let result = env.send(&[evil_create_ix(&creator.pubkey(), &evil)], &[&creator]);
    let failed = result.expect_err("evil's raw create_account refuses a pre-funded state");
    let logs = failed.meta.logs.join("\n");
    assert!(logs.contains("already in use"), "{logs}");
    // The failure is atomic: the router left no Market behind.
    assert!(env
        .svm
        .get_account(&pda::market(&EVIL_ID, &evil.state))
        .is_none());
    // Other markets of the same strategy are unaffected.
    let other = evil_accounts(&mut env);
    env.send(&[evil_create_ix(&creator.pubkey(), &other)], &[&creator])
        .expect("another state address of the same strategy still creates");
}
