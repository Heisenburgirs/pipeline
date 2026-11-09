//! SOS audit #2, H075: output-side aliasing, with `trader_output` set to the strategy's
//! own output vault.
//!
//! The router checks only that `trader_output` is a token account of the output mint;
//! it is not tied to the trader. Named as the market's output vault, the strategy's
//! payout is a transfer from the vault to itself, which the token program accepts and
//! which moves nothing. So `received` (measured on that account) is 0. What these
//! tests show, against the real router, CP strategy and SPL Token:
//!
//! - with `min_amount_out = 0` the swap succeeds. The only movements are the trader's
//!   input debit (net input plus the Pipeline fee) into the input vault and the fee
//!   account. The output vault and the trader's real output account don't change;
//! - the event and the return data report `received = 0` and the input and fee that
//!   moved;
//! - the trader can't gain: they lose input and fee and receive nothing. The CP pool
//!   books the payout it made to itself, so its recorded output reserve falls while
//!   the vault keeps the tokens: a donation to the pool, which only the trader who
//!   chose the alias pays for;
//! - the same holds for exact-out, where the strategy also refunds unused input;
//! - with any `min_amount_out > 0` the swap fails and nothing moves.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use pipeline_strategy_interface::{fees, TradeMode};
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const RESERVE: u64 = 1_000_000_000;
const FEE_BPS: u16 = 30;
const SWAP: u64 = 1_000_000;

struct World {
    env: Env,
    market: CpMarket,
    trader: Keypair,
    trader_a: Pubkey,
    trader_b: Pubkey,
    fee_a: Pubkey,
}

fn world() -> World {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, RESERVE, RESERVE, FEE_BPS);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 10 * SWAP);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);
    let fee_a = pda::fee_account(&market.market, &market.mint_a, &TOKEN);
    World {
        env,
        market,
        trader,
        trader_a,
        trader_b,
        fee_a,
    }
}

/// Selling A with the output going to the market's own B vault.
fn aliased_accounts(w: &World) -> client::SwapAccounts {
    let m = &w.market;
    cp_swap_accounts(
        m,
        &w.trader.pubkey(),
        &m.mint_a,
        &w.trader_a,
        &m.mint_b,
        &m.vault_b,
    )
}

fn exact_out(max_in: u64, amount_out: u64) -> client::SwapArgs {
    client::SwapArgs {
        mode: TradeMode::ExactOut,
        amount_in: max_in,
        amount_out,
        min_amount_out: 0,
        integrator_fee_bps: 0,
        input_hook_accounts: 0,
        extra: vec![],
    }
}

/// Reserves recorded in the CP pool: after the 8-byte discriminator and four keys.
fn pool_reserves(env: &Env, pool: &Pubkey) -> (u64, u64) {
    let data = env.account_data(pool);
    let a = u64::from_le_bytes(data[136..144].try_into().unwrap());
    let b = u64::from_le_bytes(data[144..152].try_into().unwrap());
    (a, b)
}

#[derive(Debug, PartialEq)]
struct Balances {
    trader_a: u64,
    trader_b: u64,
    vault_a: u64,
    vault_b: u64,
    fee_a: u64,
}

fn balances(w: &World) -> Balances {
    let env = &w.env;
    Balances {
        trader_a: env.token_balance(&w.trader_a),
        trader_b: env.token_balance(&w.trader_b),
        vault_a: env.token_balance(&w.market.vault_a),
        vault_b: env.token_balance(&w.market.vault_b),
        fee_a: env.token_balance(&w.fee_a),
    }
}

/// Runs an aliased swap that must succeed and checks that only the input side moved,
/// by exactly what the event and return data report, with nothing received.
fn assert_only_input_side_moves(w: &mut World, args: client::SwapArgs, expected_net: u64) {
    let before = balances(w);
    let (reserve_a, reserve_b) = pool_reserves(&w.env, &w.market.pool);
    let ix = client::swap(&aliased_accounts(w), &args);
    let trader = w.trader.insecure_clone();
    let meta = w
        .env
        .send(&[ix], &[&trader])
        .expect("aliased swap with min_amount_out = 0");
    let after = balances(w);

    let fee = fees::pipeline_fee(expected_net);
    let events = swap_events(&meta);
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.net_sent, expected_net);
    assert_eq!(
        event.received, 0,
        "nothing arrived in the named output account"
    );
    assert_eq!(event.pipeline_fee, fee);
    assert_eq!(event.integrator_fee, 0);
    let result: client::SwapResult = borsh::from_slice(&meta.return_data.data).unwrap();
    assert_eq!(
        (result.net_sent, result.received, result.pipeline_fee),
        (expected_net, 0, fee)
    );

    // Only the input side moved, by what was reported.
    assert_eq!(after.trader_a, before.trader_a - expected_net - fee);
    assert_eq!(after.vault_a, before.vault_a + expected_net);
    assert_eq!(after.fee_a, before.fee_a + fee);
    assert_eq!(
        after.vault_b, before.vault_b,
        "the self-transfer moved nothing"
    );
    assert_eq!(
        after.trader_b, before.trader_b,
        "the trader received nothing"
    );

    // The pool booked the payout it made to itself: its output reserve fell, so the
    // vault now holds that much more than the pool records (a donation).
    let (new_reserve_a, new_reserve_b) = pool_reserves(&w.env, &w.market.pool);
    assert_eq!(new_reserve_a, reserve_a + expected_net);
    let booked_out = reserve_b - new_reserve_b;
    assert!(booked_out > 0, "the strategy believed it paid out");
    println!(
        "H075 {:?}: trader paid {} A (net {expected_net} + fee {fee}), received 0 B; \
         the pool booked {booked_out} B paid to its own vault",
        args.mode,
        expected_net + fee
    );
}

/// Exact-in, `min_amount_out = 0`: the swap succeeds, the trader pays input and fee and
/// receives nothing, and the event reports exactly that.
#[test]
fn h075_exact_in_to_own_output_vault_moves_only_the_input_side() {
    let mut w = world();
    assert_only_input_side_moves(&mut w, exact_in(SWAP, 0), SWAP);
}

/// Exact-out, `min_amount_out = 0`: the strategy refunds the unused input, and the
/// trader pays only what it kept plus the fee, again for nothing.
#[test]
fn h075_exact_out_to_own_output_vault_moves_only_the_input_side() {
    let mut w = world();
    let amount_out = SWAP / 2;
    let (reserve_a, reserve_b) = pool_reserves(&w.env, &w.market.pool);
    let used = cp_in(reserve_a, reserve_b, amount_out, FEE_BPS);
    assert!(used < SWAP, "the strategy refunds part of the input");
    assert_only_input_side_moves(&mut w, exact_out(SWAP, amount_out), used);
}

/// Any positive minimum makes the aliased swap fail, and nothing moves.
#[test]
fn h075_positive_minimum_to_own_output_vault_fails_and_moves_nothing() {
    let mut w = world();
    let before = balances(&w);
    let pool_before = w.env.account_data(&w.market.pool);
    let ix = client::swap(&aliased_accounts(&w), &exact_in(SWAP, 1));
    let trader = w.trader.insecure_clone();
    assert_fails_with(w.env.send(&[ix], &[&trader]), "InsufficientOutput");
    assert_eq!(balances(&w), before);
    assert_eq!(w.env.account_data(&w.market.pool), pool_before);
}

/// The trader can't gain over repeated aliased swaps: each one costs input and fee,
/// and the trader's real output account never moves.
#[test]
fn h075_repeated_aliased_swaps_never_credit_the_trader() {
    let mut w = world();
    let start = balances(&w);
    for _ in 0..3 {
        let ix = client::swap(&aliased_accounts(&w), &exact_in(SWAP, 0));
        let trader = w.trader.insecure_clone();
        w.env.send(&[ix], &[&trader]).expect("aliased swap");
    }
    let end = balances(&w);
    let per_swap = SWAP + fees::pipeline_fee(SWAP);
    assert_eq!(end.trader_a, start.trader_a - 3 * per_swap);
    assert_eq!(end.trader_b, start.trader_b);
    assert_eq!(end.vault_b, start.vault_b);
}
