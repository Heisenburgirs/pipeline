//! Swaps built with `pipeline_client::wallet::swap_with_setup`, as a front-end builds
//! them: a trader with only native SOL buys and sells, and never ends up holding
//! wrapped SOL.

mod common;

use common::*;
use pipeline_client::wallet::{swap_with_setup, NATIVE_MINT};
use pipeline_client::{self as client, pda, Pubkey};
use pipeline_strategy_interface::{fees, TradeMode};
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const FEE_BPS: u16 = 30;

#[test]
fn trader_swaps_native_sol_in_and_out() {
    let mut env = Env::new();
    let token = env.create_mint(&TOKEN, 6, None, None);
    let (sol_reserve, token_reserve) = (10 * SOL, 1_000_000_000_000u64);
    let market = setup_cp_market_with_mints(
        &mut env,
        NATIVE_MINT,
        TOKEN,
        token,
        TOKEN,
        sol_reserve,
        token_reserve,
        FEE_BPS,
        vec![],
    );
    // A trader with native SOL and no token accounts at all.
    let trader = env.funded_keypair();
    let trader_wsol = pda::associated_token(&trader.pubkey(), &NATIVE_MINT, &TOKEN);
    let trader_token = pda::associated_token(&trader.pubkey(), &token, &TOKEN);
    let token_account_rent = env.svm.minimum_balance_for_rent_exemption(165);

    // Buy with 1 SOL. The trader pays the SOL, Pipeline's fee and the rent for the new
    // token account; the temporary wrapped-SOL account is closed again.
    let before = env.lamports(&trader.pubkey());
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &NATIVE_MINT,
        &trader_wsol,
        &token,
        &trader_token,
    );
    env.send(
        &swap_with_setup(&accounts, &exact_in(SOL, 1), true),
        &[&trader],
    )
    .expect("buy with SOL");
    let bought = cp_out(sol_reserve, token_reserve, SOL, FEE_BPS);
    assert_eq!(env.token_balance(&trader_token), bought);
    assert_eq!(
        before - env.lamports(&trader.pubkey()),
        SOL + fees::pipeline_fee(SOL) + token_account_rent
    );
    assert!(env.svm.get_account(&trader_wsol).is_none());

    // Exact-out buy with a generous maximum: the unused SOL comes back as native SOL.
    let (sol_now, token_now) = (
        env.token_balance(&market.vault(&NATIVE_MINT)),
        env.token_balance(&market.vault(&token)),
    );
    let want = 1_000_000;
    let needed = cp_in(sol_now, token_now, want, FEE_BPS);
    let before = env.lamports(&trader.pubkey());
    let args = client::SwapArgs {
        mode: TradeMode::ExactOut,
        amount_in: needed * 2,
        amount_out: want,
        min_amount_out: want,
        integrator_fee_bps: 0,
        input_hook_accounts: 0,
        extra: vec![],
    };
    env.send(&swap_with_setup(&accounts, &args, true), &[&trader])
        .expect("exact-out buy with SOL");
    assert_eq!(env.token_balance(&trader_token), bought + want);
    assert_eq!(
        before - env.lamports(&trader.pubkey()),
        needed + fees::pipeline_fee(needed)
    );
    assert!(env.svm.get_account(&trader_wsol).is_none());

    // Sell half the tokens. The SOL arrives as native SOL.
    let (sol_now, token_now) = (
        env.token_balance(&market.vault(&NATIVE_MINT)),
        env.token_balance(&market.vault(&token)),
    );
    let sell = env.token_balance(&trader_token) / 2;
    let before = env.lamports(&trader.pubkey());
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &token,
        &trader_token,
        &NATIVE_MINT,
        &trader_wsol,
    );
    env.send(
        &swap_with_setup(&accounts, &exact_in(sell, 1), true),
        &[&trader],
    )
    .expect("sell for SOL");
    assert_eq!(
        env.lamports(&trader.pubkey()) - before,
        cp_out(token_now, sol_now, sell, FEE_BPS)
    );
    assert!(env.svm.get_account(&trader_wsol).is_none());
}

#[test]
fn without_wrapping_the_trader_uses_existing_wrapped_sol() {
    let mut env = Env::new();
    let token = env.create_mint(&TOKEN, 6, None, None);
    let market = setup_cp_market_with_mints(
        &mut env,
        NATIVE_MINT,
        TOKEN,
        token,
        TOKEN,
        10 * SOL,
        1_000_000_000_000,
        FEE_BPS,
        vec![],
    );
    let (trader, trader_wsol) = env.trader_with(&NATIVE_MINT, &TOKEN, 2 * SOL);
    let trader_token = pda::associated_token(&trader.pubkey(), &token, &TOKEN);
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &NATIVE_MINT,
        &trader_wsol,
        &token,
        &trader_token,
    );
    env.send(
        &swap_with_setup(&accounts, &exact_in(SOL, 1), false),
        &[&trader],
    )
    .expect("swap from wrapped SOL");
    assert_eq!(
        env.token_balance(&trader_wsol),
        SOL - fees::pipeline_fee(SOL)
    );
}
