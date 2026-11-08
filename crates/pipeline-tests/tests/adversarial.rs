//! A hostile strategy must never harm the trader or the fee accounts: every attack
//! either fails the whole transaction or is harmless.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_strategy_interface::{fees, TradeMode};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

const HONEST: u8 = 0;
const TAKE_MORE: u8 = 1;
const SHORT_OUTPUT: u8 = 2;
const REFUND_MORE: u8 = 3;
const REENTER: u8 = 4;
const WRONG_DESTINATION: u8 = 5;
const STEAL_FEES: u8 = 6;
const TAKE_LAMPORTS: u8 = 9;

struct EvilMarket {
    state: Pubkey,
    market: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
}

fn evil_state(mint_a: &Pubkey, mint_b: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0
}

fn create_evil_market_ix(
    env: &mut Env,
    creator: &Pubkey,
    info_mode: u8,
) -> (Instruction, EvilMarket) {
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state = evil_state(&mint_a, &mint_b);
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    let ix = client::create_market(&client::CreateMarket {
        creator: *creator,
        strategy_program: EVIL_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![info_mode],
        max_creator_lamports: MAX_SETUP_LAMPORTS,
        strategy_accounts: vec![
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
        ],
    });
    let market = EvilMarket {
        state,
        market: pda::market(&EVIL_ID, &state),
        mint_a,
        mint_b,
        vault_a,
        vault_b,
    };
    (ix, market)
}

/// An evil market with 1,000,000,000 of each token in its vaults and fee accounts.
fn setup() -> (Env, EvilMarket) {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let creator = env.funded_keypair();
    let (ix, market) = create_evil_market_ix(&mut env, &creator.pubkey(), 0);
    env.send(&[ix], &[&creator]).expect("create evil market");
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &market.market, &market.mint_a, &TOKEN),
            client::init_fee_account(&payer, &market.market, &market.mint_b, &TOKEN),
        ],
        &[],
    )
    .unwrap();
    env.mint_to(&market.mint_a, &TOKEN, &market.vault_a, 1_000_000_000);
    env.mint_to(&market.mint_b, &TOKEN, &market.vault_b, 1_000_000_000);
    (env, market)
}

struct Trader {
    keypair: Keypair,
    account_a: Pubkey,
    account_b: Pubkey,
}

fn trader(env: &mut Env, market: &EvilMarket) -> Trader {
    let (keypair, account_a) = env.trader_with(&market.mint_a, &TOKEN, 10_000_000);
    let account_b = env.create_ata(&keypair.pubkey(), &market.mint_b, &TOKEN);
    Trader {
        keypair,
        account_a,
        account_b,
    }
}

/// A → B swap of `amount` with the given behaviour and extra strategy accounts.
fn evil_swap(
    market: &EvilMarket,
    trader: &Trader,
    behaviour: u8,
    amount: u64,
    extra: Vec<AccountMeta>,
) -> Instruction {
    let mut strategy_accounts = vec![AccountMeta::new(market.vault_b, false)];
    strategy_accounts.extend(extra);
    let accounts = client::SwapAccounts {
        trader: trader.keypair.pubkey(),
        strategy_program: EVIL_ID,
        market_state: market.state,
        trader_input: trader.account_a,
        trader_output: trader.account_b,
        input_vault: market.vault_a,
        input_mint: market.mint_a,
        output_mint: market.mint_b,
        input_token_program: TOKEN,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts,
        input_hook_accounts: vec![],
    };
    let mut args = exact_in(amount, 0);
    args.extra = vec![behaviour];
    client::swap(&accounts, &args)
}

fn assert_trader_untouched(env: &Env, trader: &Trader) {
    assert_eq!(env.token_balance(&trader.account_a), 10_000_000);
    assert_eq!(env.token_balance(&trader.account_b), 0);
}

#[test]
fn honest_behaviour_baseline() {
    let (mut env, market) = setup();
    let t = trader(&mut env, &market);
    let meta = env
        .send(
            &[evil_swap(&market, &t, HONEST, 1_000, vec![])],
            &[&t.keypair],
        )
        .unwrap();
    assert_eq!(env.token_balance(&t.account_b), 1_000);
    assert_eq!(
        swap_events(&meta)[0].pipeline_fee,
        fees::pipeline_fee(1_000)
    );
}

#[test]
fn strategy_cannot_take_more_from_the_trader() {
    let (mut env, market) = setup();
    let t = trader(&mut env, &market);
    let result = env.send(
        &[evil_swap(&market, &t, TAKE_MORE, 1_000, vec![])],
        &[&t.keypair],
    );
    // The trader is passed to the strategy without its signature.
    assert_fails_with(result, "signer privilege escalated");
    assert_trader_untouched(&env, &t);
}

#[test]
fn short_output_fails_the_minimum() {
    let (mut env, market) = setup();
    let t = trader(&mut env, &market);
    let mut ix = evil_swap(&market, &t, SHORT_OUTPUT, 1_000, vec![]);
    // Raise the minimum to 1,000 (min_amount_out sits after mode, amount_in, amount_out).
    let offset = 8 + 1 + 8 + 8;
    ix.data[offset..offset + 8].copy_from_slice(&1_000u64.to_le_bytes());
    assert_fails_with(env.send(&[ix], &[&t.keypair]), "InsufficientOutput");
    assert_trader_untouched(&env, &t);
}

#[test]
fn output_without_input_is_rejected() {
    // Refunding more than was sent would make net_sent zero: output with no fee and a
    // meaningless price in the swap event. The router refuses it.
    let (mut env, market) = setup();
    let t = trader(&mut env, &market);
    let result = env.send(
        &[evil_swap(&market, &t, REFUND_MORE, 1_000, vec![])],
        &[&t.keypair],
    );
    assert_fails_with(result, "OutputWithoutInput");
    assert_trader_untouched(&env, &t);
}

#[test]
fn reentering_the_router_fails() {
    let (mut env, market) = setup();
    let t = trader(&mut env, &market);
    let router = AccountMeta::new_readonly(client::PROGRAM_ID, false);
    let result = env.send(
        &[evil_swap(&market, &t, REENTER, 1_000, vec![router])],
        &[&t.keypair],
    );
    assert_fails_with(result, "ReentrancyNotAllowed");
    assert_trader_untouched(&env, &t);
}

#[test]
fn output_sent_elsewhere_fails_the_minimum() {
    let (mut env, market) = setup();
    let t = trader(&mut env, &market);
    let thief = env.create_ata(&Pubkey::new_unique(), &market.mint_b, &TOKEN);
    let mut ix = evil_swap(
        &market,
        &t,
        WRONG_DESTINATION,
        1_000,
        vec![AccountMeta::new(thief, false)],
    );
    let offset = 8 + 1 + 8 + 8;
    ix.data[offset..offset + 8].copy_from_slice(&1u64.to_le_bytes());
    assert_fails_with(env.send(&[ix], &[&t.keypair]), "InsufficientOutput");
    assert_eq!(env.token_balance(&thief), 0);
    assert_trader_untouched(&env, &t);
}

#[test]
fn market_signer_cannot_move_collected_fees() {
    let (mut env, market) = setup();
    let t = trader(&mut env, &market);
    env.send(
        &[evil_swap(&market, &t, HONEST, 100_000, vec![])],
        &[&t.keypair],
    )
    .unwrap();
    let fee_account = pda::fee_account(&market.market, &market.mint_a, &TOKEN);
    let collected = env.token_balance(&fee_account);
    assert_eq!(collected, fees::pipeline_fee(100_000));

    let attack = evil_swap(
        &market,
        &t,
        STEAL_FEES,
        1_000,
        vec![AccountMeta::new(fee_account, false)],
    );
    let result = env.send(&[attack], &[&t.keypair]);
    // Fee accounts belong to the fee authority, not the market signer.
    assert_fails_with(result, "owner does not match");
    assert_eq!(env.token_balance(&fee_account), collected);
}

#[test]
fn strategy_cannot_spend_the_traders_lamports() {
    let (mut env, market) = setup();
    let t = trader(&mut env, &market);
    let before = env.lamports(&t.keypair.pubkey());
    let system = AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false);
    let result = env.send(
        &[evil_swap(&market, &t, TAKE_LAMPORTS, 1_000, vec![system])],
        &[&t.keypair],
    );
    // The trader's wallet is read-only and unsigned for the strategy.
    assert_fails_with(result, "privilege escalated");
    assert_eq!(env.lamports(&t.keypair.pubkey()), before);
    assert_trader_untouched(&env, &t);
}

#[test]
fn invalid_market_info_is_rejected() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    for info_mode in [1, 2, 3] {
        let creator = env.funded_keypair();
        let (ix, _) = create_evil_market_ix(&mut env, &creator.pubkey(), info_mode);
        assert_fails_with(env.send(&[ix], &[&creator]), "BadMarketInfo");
    }
}

#[test]
fn exact_out_is_refused_when_the_market_does_not_support_it() {
    let (mut env, market) = setup();
    let t = trader(&mut env, &market);
    let mut ix = evil_swap(&market, &t, HONEST, 1_000, vec![]);
    ix.data[8] = TradeMode::ExactOut as u8;
    assert_fails_with(env.send(&[ix], &[&t.keypair]), "ExactOutUnsupported");
}

#[test]
fn mismatched_mints_and_token_accounts_are_rejected() {
    let (mut env, market) = setup();
    let t = trader(&mut env, &market);

    // Output mint equal to input mint.
    let mut ix = evil_swap(&market, &t, HONEST, 1_000, vec![]);
    ix.accounts[9].pubkey = market.mint_a;
    assert_fails_with(env.send(&[ix], &[&t.keypair]), "SameMint");

    // A mint the market does not trade.
    let other = env.create_mint(&TOKEN, 6, None, None);
    let mut ix = evil_swap(&market, &t, HONEST, 1_000, vec![]);
    ix.accounts[9].pubkey = other;
    assert_fails_with(env.send(&[ix], &[&t.keypair]), "MintNotInMarket");

    // Trader "input" account that holds the output mint.
    let mut ix = evil_swap(&market, &t, HONEST, 1_000, vec![]);
    ix.accounts[5].pubkey = t.account_b;
    assert_fails_with(env.send(&[ix], &[&t.keypair]), "BadTokenAccount");
    assert_trader_untouched(&env, &t);
}

#[test]
fn register_existing_market_requires_strategy_ownership() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state = evil_state(&mint_a, &mint_b);
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    let mut data = vec![0u8; 130];
    for (i, key) in [mint_a, mint_b, vault_a, vault_b].iter().enumerate() {
        data[i * 32..(i + 1) * 32].copy_from_slice(key.as_ref());
    }

    // Owned by someone else: rejected.
    env.svm
        .set_account(
            state,
            Account {
                lamports: SOL,
                data: data.clone(),
                owner: client::SYSTEM_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    let payer = env.admin.pubkey();
    let ix = client::register_existing_market(&payer, &EVIL_ID, &state, vec![]);
    assert_fails_with(
        env.send(std::slice::from_ref(&ix), &[]),
        "MarketStateNotOwnedByStrategy",
    );

    // Created by the strategy itself (simulated by ownership): accepted.
    env.svm
        .set_account(
            state,
            Account {
                lamports: SOL,
                data,
                owner: EVIL_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    env.send(&[ix], &[]).unwrap();
    let market = env.market(&pda::market(&EVIL_ID, &state));
    assert_eq!(market.mints(), &[mint_a, mint_b]);
}

#[test]
fn immutable_strategy_registers_through_its_own_pda() {
    let mut env = Env::new();
    env.svm
        .add_program_from_file(EVIL_ID, sbf_dir().join("test_strategy_evil.so"))
        .unwrap();
    set_upgrade_authority(&mut env.svm, &EVIL_ID, None);

    let payer = env.admin.pubkey();
    let register_pda = pda::register(&EVIL_ID);
    let inner = client::register_strategy(
        &payer,
        &register_pda,
        &EVIL_ID,
        &env.listing_fee_recipient,
        "uri",
    );
    let mut accounts = vec![AccountMeta::new_readonly(client::PROGRAM_ID, false)];
    accounts.extend(inner.accounts.iter().map(|meta| AccountMeta {
        // The PDA cannot sign the outer transaction; the strategy signs for it.
        is_signer: meta.is_signer && meta.pubkey != register_pda,
        ..meta.clone()
    }));
    let mut data = b"evilfwd!".to_vec();
    data.extend_from_slice(&inner.data);
    let forward = Instruction::new_with_bytes(EVIL_ID, &data, accounts);
    env.send(&[forward], &[]).unwrap();
    let strategy = client::decode_strategy(&env.account_data(&pda::strategy(&EVIL_ID))).unwrap();
    assert_eq!(strategy.program_id, EVIL_ID);
}
