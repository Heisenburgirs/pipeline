//! SOS audit #2, H031: native-SOL accounts, `SyncNative` and unsynced lamports.
//!
//! The router reads balances from the token account's `amount` field. On a wrapped-SOL
//! account that field lags the lamports until `SyncNative` (permissionless) runs. So:
//!
//! - lamports that were never synced are invisible to `settle`;
//! - code that runs inside the swap window (the strategy; a transfer hook of the input
//!   mint during the router's transfer-in; a hook of the output mint during the
//!   strategy's payment) can raise `amount` of a wSOL account. `SyncNative` and an SPL
//!   `Transfer` into the account change `amount` the same way, so the refill hook with a
//!   wSOL stash stands in for a strategy or hook that wraps lamports and syncs.
//!
//! No fixture strategy issues `SyncNative` (the evil strategy has no behaviour for it),
//! hence the hook stand-in; the strategy-side variant is settled by code reading
//! (`call_strategy_swap` passes the strategy `trader_output` writable and both token
//! programs, so a CPI to `SyncNative` is possible).
//!
//! Tests:
//! 1. unsynced lamports on every watched wSOL account are invisible and never charged;
//! 2. (a) a wSOL `trader_output` credited inside the window counts toward `received` and
//!    so toward `min_amount_out`: pinned as the router's behaviour (the credited tokens
//!    are the trader's own);
//! 3. (b) a wSOL `trader_input` refilled inside the window: fee base stays `amount_in`;
//! 4. (c) a wSOL `input_vault` credited inside the window: fee base capped at `amount_in`.

#![allow(clippy::result_large_err)]

mod bok_swap_util;
mod common;

use bok_swap_util::*;
use common::*;
use pipeline_client::wallet::{system_transfer, NATIVE_MINT};
use pipeline_client::{self as client, pda, Pubkey};
use pipeline_strategy_interface::fees;
use solana_signer::Signer;

const RESERVE: u64 = 1_000_000_000;
const AMOUNT: u64 = 1_000_000;
const UNSYNCED: u64 = 700_000;
const REFILL: u64 = 600_000;
const POOL_FEE_BPS: u16 = 30;

fn lamport_gift(env: &mut Env, from: &solana_keypair::Keypair, to: &Pubkey, lamports: u64) {
    env.send(&[system_transfer(&from.pubkey(), to, lamports)], &[from])
        .expect("setup: lamports");
}

/// 1. Honest CP swap SPL X -> wSOL. The trader's wSOL output account holds `UNSYNCED`
///    lamports that were never synced. `received` is the strategy's payout only; the
///    unsynced lamports are untouched and do not count toward `min_amount_out`.
#[test]
fn h031_unsynced_lamports_on_the_output_are_not_received() {
    let mut env = Env::new();
    let mint_x = env.create_mint(&TOKEN, 9, None, None);
    let m = setup_cp_market_with_mints(
        &mut env,
        mint_x,
        TOKEN,
        NATIVE_MINT,
        TOKEN,
        RESERVE,
        RESERVE,
        POOL_FEE_BPS,
        vec![],
    );
    let (trader, trader_in) = env.trader_with(&mint_x, &TOKEN, 100 * AMOUNT);
    let trader_out = env.create_ata(&trader.pubkey(), &NATIVE_MINT, &TOKEN);
    lamport_gift(&mut env, &trader, &trader_out, UNSYNCED);
    assert_eq!(
        env.token_balance(&trader_out),
        0,
        "setup: unsynced, amount still 0"
    );
    let lamports_before = env.lamports(&trader_out);
    let payout = cp_out(RESERVE, RESERVE, AMOUNT, POOL_FEE_BPS);

    let accounts = cp_swap_accounts(
        &m,
        &trader.pubkey(),
        &mint_x,
        &trader_in,
        &NATIVE_MINT,
        &trader_out,
    );
    // The unsynced lamports must not satisfy a min_out above the strategy's payout.
    let too_high = send_swap(
        &mut env,
        client::swap(&accounts, &exact_in(AMOUNT, payout + 1)),
        &trader,
    );
    assert!(
        too_high.is_err(),
        "H031: unsynced lamports counted toward min_amount_out"
    );

    let meta = send_swap(
        &mut env,
        client::swap(&accounts, &exact_in(AMOUNT, payout)),
        &trader,
    )
    .unwrap_or_else(|e| panic!("setup: swap must succeed: {:?}", e.err));
    let (r, _) = reported(&meta);
    assert_eq!(r.received, payout, "H031: received is the strategy payout");
    assert_eq!(env.token_balance(&trader_out), payout);
    assert_eq!(
        env.lamports(&trader_out) - lamports_before,
        payout,
        "the unsynced lamports stay where they were"
    );
    assert_eq!(r.net_sent, AMOUNT);
}

/// 1b. wSOL input with unsynced lamports on both the trader's input account and the
///     market's input vault. They are invisible: the fee base is exactly `amount_in`.
#[test]
fn h031_unsynced_lamports_on_input_and_vault_do_not_move_the_fee_base() {
    let mut env = Env::new();
    let mint_out = env.create_mint(&TOKEN, 9, None, None);
    let m = setup_cp_market_with_mints(
        &mut env,
        NATIVE_MINT,
        TOKEN,
        mint_out,
        TOKEN,
        RESERVE,
        RESERVE,
        POOL_FEE_BPS,
        vec![],
    );
    let (trader, trader_in) = env.trader_with(&NATIVE_MINT, &TOKEN, 100 * AMOUNT);
    let trader_out = env.create_ata(&trader.pubkey(), &mint_out, &TOKEN);
    lamport_gift(&mut env, &trader, &trader_in, UNSYNCED);
    lamport_gift(&mut env, &trader, &m.vault_a, UNSYNCED);
    let accounts = cp_swap_accounts(
        &m,
        &trader.pubkey(),
        &NATIVE_MINT,
        &trader_in,
        &mint_out,
        &trader_out,
    );
    let fee_account = pda::fee_account(&m.market, &NATIVE_MINT, &TOKEN);
    let watched = [trader_in, m.vault_a, fee_account];
    let before = balances(&env, &watched);
    let meta = send_swap(
        &mut env,
        client::swap(&accounts, &exact_in(AMOUNT, 1)),
        &trader,
    )
    .unwrap_or_else(|e| panic!("setup: swap must succeed: {:?}", e.err));
    let after = balances(&env, &watched);
    let (r, _) = reported(&meta);
    assert_eq!(r.net_sent, AMOUNT, "H031: unsynced lamports moved net_sent");
    assert_eq!(r.pipeline_fee, fees::pipeline_fee(AMOUNT), "H031");
    assert_eq!(after[2] - before[2], fees::pipeline_fee(AMOUNT), "H031");
    assert_eq!(before[0] - after[0], AMOUNT + r.pipeline_fee);
    assert_eq!(after[1] - before[1], AMOUNT);
}

/// 2. (a) In-window credit of a wSOL `trader_output`. The *input* mint is a Token-2022
///    mint whose hook, during the router's transfer-in, moves a wSOL stash (the trader's
///    own tokens, parked beforehand) into `trader_output`, the same effect on `amount`
///    as wrapping lamports and running `SyncNative` there. The strategy pays only its
///    quote `P`; `received = P + REFILL`, so a `min_amount_out` of `P + REFILL` passes.
///
///    This is pinned, not asserted as a bug: the credited tokens are the trader's own
///    (the hook can only move what the trader parked or approved), `min_amount_out` is
///    "the trader's output balance must rise by this much", and that holds.
#[test]
fn h031_in_window_credit_of_wsol_output_counts_as_received() {
    let mut env = Env::new();
    let trader = env.funded_keypair();
    let trader_out = env.create_ata(&trader.pubkey(), &NATIVE_MINT, &TOKEN);
    let hook = refill_hook(&mut env, &NATIVE_MINT, |_| trader_out);
    let m = setup_cp_market_with_mints(
        &mut env,
        hook.mint,
        TOKEN_2022,
        NATIVE_MINT,
        TOKEN,
        RESERVE,
        RESERVE,
        POOL_FEE_BPS,
        hook.accounts.clone(),
    );
    let trader_in = env.create_ata(&trader.pubkey(), &hook.mint, &TOKEN_2022);
    env.mint_to(&hook.mint, &TOKEN_2022, &trader_in, 100 * AMOUNT);
    // The trader's own wSOL, parked in the hook's stash.
    env.mint_to(&NATIVE_MINT, &TOKEN, &hook.stash, REFILL);
    assert_eq!(
        env.token_balance(&hook.stash),
        REFILL,
        "setup: stash funded"
    );
    assert_eq!(env.token_balance(&trader_out), 0, "setup: output empty");

    let payout = cp_out(RESERVE, RESERVE, AMOUNT, POOL_FEE_BPS);
    let mut accounts = cp_swap_accounts(
        &m,
        &trader.pubkey(),
        &hook.mint,
        &trader_in,
        &NATIVE_MINT,
        &trader_out,
    );
    accounts.input_hook_accounts = hook.accounts.clone();
    let meta = send_swap(
        &mut env,
        client::swap(&accounts, &exact_in(AMOUNT, payout + REFILL)),
        &trader,
    )
    .unwrap_or_else(|e| panic!("setup: swap must succeed: {:?}", e.err));
    let (r, _) = reported(&meta);
    assert_eq!(env.token_balance(&hook.stash), 0, "setup: hook fired");
    println!(
        "[wSOL output credited in window] strategy payout {payout}, credited {REFILL}, \
         received {}, trader_output {}",
        r.received,
        env.token_balance(&trader_out)
    );
    // Pinned behaviour: received counts the in-window credit, and matches the balance.
    assert_eq!(r.received, payout + REFILL);
    assert_eq!(env.token_balance(&trader_out), payout + REFILL);
    // The input side is unaffected.
    assert_eq!(r.net_sent, AMOUNT);
    assert_eq!(r.pipeline_fee, fees::pipeline_fee(AMOUNT));
}

/// Shared setup for (b) and (c): honest CP swap, wSOL input, refill-hooked Token-2022
/// output whose hook empties a `REFILL` wSOL stash into `target(trader_in, vault)` during
/// the strategy's output payment. Returns (`net_sent`, `pipeline_fee`, fee account
/// delta, trader_in delta, vault delta).
fn wsol_input_with_hook_credit(
    target: impl FnOnce(&Pubkey, &Pubkey) -> Pubkey,
) -> (u64, u64, u64, u64, u64) {
    let mut env = Env::new();
    let trader = env.funded_keypair();
    let trader_in = env.create_ata(&trader.pubkey(), &NATIVE_MINT, &TOKEN);
    env.mint_to(&NATIVE_MINT, &TOKEN, &trader_in, 100 * AMOUNT);
    let hook = refill_hook(&mut env, &NATIVE_MINT, |mint| {
        // The vault is the CP pool's ATA of the wSOL mint; the pool address depends on
        // the hook mint, which is only known here.
        let pool = cp_pool(&NATIVE_MINT, mint, POOL_FEE_BPS);
        let vault = pda::associated_token(&pool, &NATIVE_MINT, &TOKEN);
        target(&trader_in, &vault)
    });
    let m = setup_cp_market_with_mints(
        &mut env,
        NATIVE_MINT,
        TOKEN,
        hook.mint,
        TOKEN_2022,
        RESERVE,
        RESERVE,
        POOL_FEE_BPS,
        // The stash is still empty during the deposit, so the hook does nothing.
        hook.accounts.clone(),
    );
    let trader_out = env.create_ata(&trader.pubkey(), &hook.mint, &TOKEN_2022);
    env.mint_to(&NATIVE_MINT, &TOKEN, &hook.stash, REFILL);
    assert_eq!(
        env.token_balance(&hook.stash),
        REFILL,
        "setup: stash funded"
    );

    let mut accounts = cp_swap_accounts(
        &m,
        &trader.pubkey(),
        &NATIVE_MINT,
        &trader_in,
        &hook.mint,
        &trader_out,
    );
    accounts.strategy_accounts.extend(hook.accounts.clone());
    let fee_account = pda::fee_account(&m.market, &NATIVE_MINT, &TOKEN);
    let watched = [trader_in, m.vault_a, fee_account, hook.stash];
    let before = balances(&env, &watched);
    let meta = send_swap(
        &mut env,
        client::swap(&accounts, &exact_in(AMOUNT, 1)),
        &trader,
    )
    .unwrap_or_else(|e| panic!("setup: swap must succeed: {:?}", e.err));
    let after = balances(&env, &watched);
    let (r, _) = reported(&meta);
    assert_eq!(
        after[3], 0,
        "setup: the hook credited its stash in the window"
    );
    (
        r.net_sent,
        r.pipeline_fee,
        after[2] - before[2],
        (after[0] as i128 - before[0] as i128) as u64,
        (after[1] as i128 - before[1] as i128) as u64,
    )
}

/// 3. (b) wSOL `trader_input` refilled in the window. `trader_spent = AMOUNT - REFILL`
///    but the vault kept `AMOUNT`: the fee base is `AMOUNT` and Pipeline is paid in full.
#[test]
fn h031_refill_of_wsol_trader_input_still_pays_full_fee() {
    let (net_sent, fee, fee_delta, _trader_delta, vault_delta) =
        wsol_input_with_hook_credit(|trader_in, _| *trader_in);
    assert_eq!(vault_delta, AMOUNT, "setup: the vault kept AMOUNT");
    assert_eq!(net_sent, AMOUNT, "H031: refill lowered the fee base");
    assert_eq!(fee, fees::pipeline_fee(AMOUNT), "H031");
    assert_eq!(fee_delta, fees::pipeline_fee(AMOUNT), "H031");
}

/// 4. (c) wSOL `input_vault` credited in the window (as `SyncNative` after a lamport
///    credit would). `vault_kept = AMOUNT + REFILL`, capped at `amount_in`: the fee base
///    is `AMOUNT`, not more.
#[test]
fn h031_credit_of_wsol_input_vault_is_capped_at_amount_in() {
    let (net_sent, fee, fee_delta, _trader_delta, vault_delta) =
        wsol_input_with_hook_credit(|_, vault| *vault);
    assert_eq!(
        vault_delta,
        AMOUNT + REFILL,
        "setup: the vault was credited in the window"
    );
    assert_eq!(net_sent, AMOUNT, "H031: vault credit raised the fee base");
    assert_eq!(fee, fees::pipeline_fee(AMOUNT), "H031");
    assert_eq!(fee_delta, fees::pipeline_fee(AMOUNT), "H031");
}
