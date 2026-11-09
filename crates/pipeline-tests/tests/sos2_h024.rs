//! SOS2 H024 — exact-in `trade.amount` vs. what arrives in the vault (Token-2022 transfer fee).
//!
//! Status: fixed in the docs. The interface (`TradeRequest::amount`) and STRATEGY-GUIDE §1
//! now say exact-in `trade.amount` is what the router sent (`amount_in`, before any transfer
//! fee), so less may arrive. These tests pin that: `trade.amount == amount_in`, the vault
//! receives `amount_in` less the transfer fee, and the reference CP strategy credits what
//! arrived. The strategy CPI is observed through LiteSVM inner instructions.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Pubkey};
use pipeline_strategy_interface::{self as psi, discriminator};
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

fn transfer_fee(amount: u64, bps: u16, max: u64) -> u64 {
    psi::fees::ceil_bps(amount, bps as u64).unwrap().min(max)
}

/// The `SwapRequest` the router passed to the strategy, from the inner instructions.
fn strategy_swap_request(meta: &litesvm::types::TransactionMetadata) -> psi::SwapRequest {
    use borsh::BorshDeserialize;
    for group in &meta.inner_instructions {
        for inner in group {
            let data = &inner.instruction.data;
            if data.len() > 8 && data[..8] == discriminator::SWAP {
                return psi::SwapRequest::try_from_slice(&data[8..]).expect("decode SwapRequest");
            }
        }
    }
    panic!("no pipeline_swap CPI found in inner instructions");
}

struct EvilWorld {
    env: Env,
    market: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
}

fn evil_world(fee: Option<(u16, u64)>) -> EvilWorld {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let mint_a = env.create_mint(&TOKEN_2022, 6, fee, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN_2022);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    let creator = env.funded_keypair();
    let ix = client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
        strategy_program: EVIL_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![0],
        max_creator_lamports: SOL,
        strategy_accounts: vec![
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
        ],
    });
    env.send(&[ix], &[&creator]).expect("create evil market");
    let market = pda::market(&EVIL_ID, &state);
    let payer = env.admin.pubkey();
    for (mint, program) in [(&mint_a, &TOKEN_2022), (&mint_b, &TOKEN)] {
        let ix = client::init_fee_account(&payer, &market, mint, program);
        env.send(&[ix], &[]).expect("init fee account");
    }
    env.mint_to(&mint_b, &TOKEN, &vault_b, 1_000_000_000);
    EvilWorld {
        env,
        market: state,
        mint_a,
        mint_b,
        vault_a,
        vault_b,
    }
}

/// One honest exact-in swap A -> B of `amount`; returns (request seen by strategy, arrived
/// in vault, trader's input spend).
fn honest_swap(w: &mut EvilWorld, amount: u64) -> (psi::SwapRequest, u64, u64, u64) {
    let (trader, trader_a) = w.env.trader_with(&w.mint_a, &TOKEN_2022, 10_000_000);
    let trader_b = w.env.create_ata(&trader.pubkey(), &w.mint_b, &TOKEN);
    let vault_before = w.env.token_balance(&w.vault_a);
    let accounts = client::SwapAccounts {
        trader: trader.pubkey(),
        strategy_program: EVIL_ID,
        market_state: w.market,
        trader_input: trader_a,
        trader_output: trader_b,
        input_vault: w.vault_a,
        input_mint: w.mint_a,
        output_mint: w.mint_b,
        input_token_program: TOKEN_2022,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts: vec![AccountMeta::new(w.vault_b, false)],
        input_hook_accounts: vec![],
    };
    // Honest evil pays amount_sent out 1:1; min out of 1 keeps the swap permissive.
    let meta = w
        .env
        .send(&[client::swap(&accounts, &exact_in(amount, 1))], &[&trader])
        .expect("honest swap");
    let arrived = w.env.token_balance(&w.vault_a) - vault_before;
    let spent = 10_000_000 - w.env.token_balance(&trader_a);
    let request = strategy_swap_request(&meta);
    let event_net_sent = swap_events(&meta)[0].net_sent;
    (request, arrived, spent, event_net_sent)
}

#[test]
fn control_without_transfer_fee_trade_amount_equals_arrival() {
    let mut w = evil_world(None);
    let amount = 1_000_000;
    let (request, arrived, _, _) = honest_swap(&mut w, amount);
    assert_eq!(arrived, amount, "setup: no fee, full amount arrives");
    assert_eq!(request.trade.amount, arrived);
    assert_eq!(request.amount_sent, amount);
}

/// Documented (interface + STRATEGY-GUIDE §1) behaviour: exact-in `trade.amount` is the
/// `amount_in` the router sent, before the Token-2022 transfer fee; less arrives in the vault.
#[test]
fn h024_exact_in_trade_amount_is_amount_in_before_transfer_fee() {
    let (bps, max) = (100u16, u64::MAX);
    let mut w = evil_world(Some((bps, max)));
    let amount_in = 1_000_000u64;
    let fee = transfer_fee(amount_in, bps, max);
    let (request, arrived, _, _) = honest_swap(&mut w, amount_in);

    assert!(fee > 0);
    assert_eq!(
        request.trade.amount, amount_in,
        "trade.amount is what the router sent"
    );
    assert_eq!(
        request.amount_sent, amount_in,
        "amount_sent is the gross amount_in"
    );
    assert_eq!(
        arrived,
        amount_in - fee,
        "the vault receives amount_in less the transfer fee"
    );
    assert!(arrived < request.trade.amount);
}

#[test]
fn router_guarantees_unchanged_trader_pays_gross_plus_fee_and_event_net_sent_is_gross() {
    let (bps, max) = (100u16, u64::MAX);
    let mut w = evil_world(Some((bps, max)));
    let amount = 1_000_000u64;
    let (_, _, spent, event_net_sent) = honest_swap(&mut w, amount);
    assert_eq!(event_net_sent, amount);
    assert_eq!(spent, amount + psi::fees::pipeline_fee(amount));
}

#[test]
fn cp_reference_strategy_credits_what_arrived_not_trade_amount() {
    let mut env = Env::new();
    let (bps, max) = (100u16, u64::MAX);
    let mint_a = env.create_mint(&TOKEN_2022, 6, Some((bps, max)), None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let market = setup_cp_market_with_mints(
        &mut env,
        mint_a,
        TOKEN_2022,
        mint_b,
        TOKEN,
        1_000_000_000,
        1_000_000_000,
        30,
        vec![],
    );
    let reserve_a = env.token_balance(&market.vault_a);
    let reserve_b = env.token_balance(&market.vault_b);
    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN_2022, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let amount = 1_000_000;
    let arrived = amount - transfer_fee(amount, bps, max);
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &mint_a,
        &trader_a,
        &mint_b,
        &trader_b,
    );
    let meta = env
        .send(&[client::swap(&accounts, &exact_in(amount, 1))], &[&trader])
        .unwrap();
    let request = strategy_swap_request(&meta);
    assert_eq!(
        request.trade.amount, amount,
        "router passes gross as trade.amount"
    );
    // Output equals the price of the *arrived* amount, not of the gross trade.amount.
    assert_eq!(
        env.token_balance(&trader_b),
        cp_out(reserve_a, reserve_b, arrived, 30)
    );
    assert_ne!(
        cp_out(reserve_a, reserve_b, arrived, 30),
        cp_out(reserve_a, reserve_b, amount, 30)
    );
}
