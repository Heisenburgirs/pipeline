//! BOK runtime evidence for the router's `swap` fee base and fee bounds, measured over
//! the whole instruction: INV-SWAP-1, INV-FEE-11 / INV-SWAP-5 and INV-FEE-19 /
//! INV-SWAP-10 (`.bok/confirmed-invariants/01-settle.md`).

#![allow(clippy::result_large_err)]

mod bok_swap_util;
mod common;

use bok_swap_util::*;
use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use pipeline_strategy_interface::fees;
use solana_signer::Signer;

const RESERVE: u64 = 1_000_000_000;
const BPS_CASES: [u16; 5] = [0, 100, 10_000, 10_001, 65_535];

/// A CP market selling an SPL `mint_in` for a refill-hooked Token-2022 mint, where the
/// hook's target is chosen by `target_of(hook_mint)`. Returns the market and the hook.
fn hooked_cp_market(
    env: &mut Env,
    mint_in: Pubkey,
    target_of: impl FnOnce(&Pubkey) -> Pubkey,
) -> (CpMarket, RefillHook) {
    let hook = refill_hook(env, &mint_in, target_of);
    let m = setup_cp_market_with_mints(
        env,
        mint_in,
        TOKEN,
        hook.mint,
        TOKEN_2022,
        RESERVE,
        RESERVE,
        30,
        hook.accounts.clone(),
    );
    (m, hook)
}

// -------------------------------------------------------------------- INV-SWAP-1

/// INV-SWAP-1 (in-place regression): the fee base never exceeds `amount_in`. The output
/// mint's hook tops up the market's input vault with a stash of 100 × `amount_in` during
/// the strategy call, so the vault "kept" far more than was sent. Exact-in and exact-out
/// (with a refund), with and without an integrator fee: `net_sent = amount_in`,
/// Δfee_account = `pipeline_fee(amount_in)`, and the trader's debit stays within
/// `max_total_spend(amount_in, bps)` (exactly `trader_spent + both fees`).
#[test]
fn bok_inv_swap_1_vault_topup_cannot_raise_fee_base_above_amount_in() {
    for exact_out_mode in [false, true] {
        for bps in [0u16, 100, 65_535] {
            let label = format!("exact_out={exact_out_mode} bps={bps}");
            let mut env = Env::new();
            let mint_in = env.create_mint(&TOKEN, 6, None, None);
            let (m, hook) = hooked_cp_market(&mut env, mint_in, |mint| {
                pda::associated_token(&cp_pool(&mint_in, mint, 30), &mint_in, &TOKEN)
            });
            assert_eq!(hook.accounts[4].pubkey, m.vault_a, "hook targets the vault");
            let (trader, trader_in) = env.trader_with(&mint_in, &TOKEN, 100_000_000);
            let trader_out = env.create_ata(&trader.pubkey(), &hook.mint, &TOKEN_2022);
            let integrator = env.create_ata(&Pubkey::new_unique(), &mint_in, &TOKEN);
            let fee_account = pda::fee_account(&m.market, &mint_in, &TOKEN);

            let want = 1_000_000u64;
            let need = cp_in(RESERVE, RESERVE, want, 30);
            let (mut args, amount_in, trader_spent) = if exact_out_mode {
                let max_in = 3 * need;
                (exact_out(max_in, want, want), max_in, need)
            } else {
                (exact_in(want, 0), want, want)
            };
            args.integrator_fee_bps = bps;
            let stash = 100 * amount_in;
            env.mint_to(&mint_in, &TOKEN, &hook.stash, stash);

            let mut accounts = cp_swap_accounts(
                &m,
                &trader.pubkey(),
                &mint_in,
                &trader_in,
                &hook.mint,
                &trader_out,
            );
            accounts.strategy_accounts.extend(hook.accounts.clone());
            if bps > 0 {
                accounts.integrator_fee_account = Some(integrator);
            }
            let watched = [trader_in, m.vault_a, fee_account, integrator, hook.stash];
            let before = balances(&env, &watched);
            let meta = send_swap(&mut env, client::swap(&accounts, &args), &trader)
                .unwrap_or_else(|e| panic!("{label}: {:?}", e.err));
            let after = balances(&env, &watched);
            let (r, _) = reported(&meta);

            assert_eq!(after[4], 0, "{label}: the stash was emptied into the vault");
            let vault_gain = after[1] - before[1];
            assert!(
                vault_gain >= trader_spent + stash,
                "{label}: vault top-up happened"
            );
            assert_eq!(
                r.net_sent, amount_in,
                "{label}: fee base capped at amount_in"
            );
            assert!(r.net_sent <= amount_in);
            assert_eq!(
                after[2] - before[2],
                fees::pipeline_fee(amount_in),
                "{label}"
            );
            assert_eq!(r.pipeline_fee, fees::pipeline_fee(amount_in), "{label}");
            assert_eq!(
                after[3] - before[3],
                fees::ceil_bps(amount_in, bps as u64).unwrap(),
                "{label}: integrator"
            );
            let debit = before[0] - after[0];
            assert_eq!(
                debit,
                trader_spent + r.pipeline_fee + r.integrator_fee,
                "{label}"
            );
            assert!(
                debit <= fees::max_total_spend(amount_in, bps).unwrap(),
                "{label}: debit {debit} above the signed maximum"
            );
        }
    }
}

// ------------------------------------------------------ INV-FEE-11 / INV-SWAP-5

#[derive(Clone, Copy, Debug)]
enum Kind {
    ExactIn,
    ExactOutRefund,
    TraderInputRefill,
}

/// INV-FEE-11 / INV-SWAP-5 (merged) and INV-FEE-19 / INV-SWAP-10 (bps above 100%):
/// over the whole instruction, the trader's input account falls by exactly
/// `trader_spent + pipeline_fee + integrator_fee` and never by more than
/// `max_total_spend(amount_in, bps)`; exact-in debits exactly that maximum. Run for
/// integrator bps 0 / 100 / 10_000 / 10_001 / 65_535 across exact-in, exact-out with a
/// refund, and an output-mint hook that refills the trader's input account during the
/// call. Fees are `pipeline_fee(net_sent)` and `ceil_bps(net_sent, bps)`, and above
/// 10_000 bps the integrator fee exceeds `net_sent`.
#[test]
fn bok_inv_fee_11_debit_is_exact_and_within_max_total_spend() {
    for kind in [Kind::ExactIn, Kind::ExactOutRefund, Kind::TraderInputRefill] {
        for bps in BPS_CASES {
            let label = format!("{kind:?} bps={bps}");
            let mut env = Env::new();
            let mint_in = env.create_mint(&TOKEN, 6, None, None);
            let trader = env.funded_keypair();
            let trader_in = env.create_ata(&trader.pubkey(), &mint_in, &TOKEN);
            env.mint_to(&mint_in, &TOKEN, &trader_in, 100_000_000);
            let amount = 1_000_000u64;
            let refill = amount / 2;

            let (m, out_mint, out_tp, hook) = match kind {
                Kind::TraderInputRefill => {
                    let (m, hook) = hooked_cp_market(&mut env, mint_in, |_| trader_in);
                    env.mint_to(&mint_in, &TOKEN, &hook.stash, refill);
                    let mint = hook.mint;
                    (m, mint, TOKEN_2022, Some(hook))
                }
                _ => {
                    let mint_out = env.create_mint(&TOKEN, 6, None, None);
                    let m = setup_cp_market_with_mints(
                        &mut env,
                        mint_in,
                        TOKEN,
                        mint_out,
                        TOKEN,
                        RESERVE,
                        RESERVE,
                        30,
                        vec![],
                    );
                    (m, mint_out, TOKEN, None)
                }
            };
            let trader_out = env.create_ata(&trader.pubkey(), &out_mint, &out_tp);
            let integrator = env.create_ata(&Pubkey::new_unique(), &mint_in, &TOKEN);
            let fee_account = pda::fee_account(&m.market, &mint_in, &TOKEN);
            let mut accounts = cp_swap_accounts(
                &m,
                &trader.pubkey(),
                &mint_in,
                &trader_in,
                &out_mint,
                &trader_out,
            );
            if let Some(hook) = &hook {
                accounts.strategy_accounts.extend(hook.accounts.clone());
            }
            if bps > 0 {
                accounts.integrator_fee_account = Some(integrator);
            }
            let (mut args, amount_in, trader_spent) = match kind {
                Kind::ExactIn => (exact_in(amount, 0), amount, amount),
                Kind::ExactOutRefund => {
                    let want = 1_000_000u64;
                    let need = cp_in(RESERVE, RESERVE, want, 30);
                    (exact_out(2 * need, want, want), 2 * need, need)
                }
                Kind::TraderInputRefill => (exact_in(amount, 1), amount, amount - refill),
            };
            args.integrator_fee_bps = bps;

            let watched = [trader_in, fee_account, integrator];
            let before = balances(&env, &watched);
            let meta = send_swap(&mut env, client::swap(&accounts, &args), &trader)
                .unwrap_or_else(|e| panic!("{label}: {:?}", e.err));
            let after = balances(&env, &watched);
            let (r, _) = reported(&meta);
            let debit = before[0] - after[0];
            let max = fees::max_total_spend(amount_in, bps).unwrap();

            assert_eq!(
                debit,
                trader_spent + r.pipeline_fee + r.integrator_fee,
                "{label}: debit = trader_spent + fees"
            );
            assert!(
                debit <= max,
                "{label}: debit {debit} > max_total_spend {max}"
            );
            if matches!(kind, Kind::ExactIn) {
                assert_eq!(debit, max, "{label}: exact-in debits exactly the maximum");
            }
            let expected_base = match kind {
                Kind::ExactOutRefund => trader_spent,
                _ => amount_in,
            };
            assert_eq!(r.net_sent, expected_base, "{label}: net_sent");
            assert_eq!(r.pipeline_fee, fees::pipeline_fee(r.net_sent), "{label}");
            assert_eq!(
                r.integrator_fee,
                fees::ceil_bps(r.net_sent, bps as u64).unwrap(),
                "{label}"
            );
            if bps > 10_000 {
                assert!(
                    r.integrator_fee > r.net_sent,
                    "{label}: > 100% integrator fee"
                );
            }
            assert_eq!(
                after[1] - before[1],
                r.pipeline_fee,
                "{label}: Δfee_account"
            );
            assert_eq!(
                after[2] - before[2],
                r.integrator_fee,
                "{label}: Δintegrator"
            );
        }
    }
}

// --------------------------------------------------- INV-FEE-19 / INV-SWAP-10

/// INV-FEE-19 / INV-SWAP-10: an integrator fee without an integrator account is refused
/// with `MissingIntegratorFeeAccount` before any transfer, for any bps > 0.
#[test]
fn bok_inv_fee_19_missing_integrator_account_is_refused() {
    let mut env = Env::new();
    let m = setup_cp_market(&mut env, TOKEN, TOKEN, RESERVE, RESERVE, 30);
    let (trader, trader_a) = env.trader_with(&m.mint_a, &TOKEN, 100_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &m.mint_b, &TOKEN);
    let fee_a = pda::fee_account(&m.market, &m.mint_a, &TOKEN);
    let watched = [trader_a, trader_b, m.vault_a, m.vault_b, fee_a];
    let accounts = cp_swap_accounts(
        &m,
        &trader.pubkey(),
        &m.mint_a,
        &trader_a,
        &m.mint_b,
        &trader_b,
    );
    assert!(accounts.integrator_fee_account.is_none());
    for bps in [1u16, 75, 10_000, 10_001, 65_535] {
        let mut args = exact_in(1_000_000, 0);
        args.integrator_fee_bps = bps;
        let before = balances(&env, &watched);
        assert_fails_with(
            send_swap(&mut env, client::swap(&accounts, &args), &trader),
            "MissingIntegratorFeeAccount",
        );
        assert_eq!(balances(&env, &watched), before, "bps {bps}");
    }
}

/// `floor(1e4 × u64::MAX / bps)`: the largest base whose `ceil_bps` fits in a u64.
fn overflow_threshold(bps: u16) -> u64 {
    (10_000u128 * u64::MAX as u128 / bps as u128) as u64
}

/// An evil-strategy swap of `amount` A for B (1:1) with `bps`, where the trader holds
/// `amount + extra` A and vault B holds `amount` B. Returns the result and whether any
/// watched balance changed.
fn huge_swap(amount: u64, extra: u64, bps: u16) -> (TxResult, bool) {
    let mut env = Env::new();
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let m = evil_market(&mut env, mint_a, TOKEN, mint_b, None, amount);
    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN, amount + extra);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let integrator = env.create_ata(&Pubkey::new_unique(), &mint_a, &TOKEN);
    let fee_a = pda::fee_account(&m.market, &mint_a, &TOKEN);
    let mut accounts = evil_swap_accounts(&m, &trader.pubkey(), &trader_a, &trader_b);
    accounts.integrator_fee_account = Some(integrator);
    let watched = [trader_a, trader_b, m.vault_a, m.vault_b, fee_a, integrator];
    let before = balances(&env, &watched);
    let result = send_swap(
        &mut env,
        client::swap(&accounts, &evil_args(HONEST, amount, bps)),
        &trader,
    );
    let moved = balances(&env, &watched) != before;
    (result, moved)
}

/// INV-FEE-19 / INV-SWAP-10: a `net_sent` above `floor(1e4 × u64::MAX / bps)` makes the
/// integrator fee unrepresentable; the router refuses with `MathOverflow` (not a panic)
/// and nothing moves. Shown for 65_535 bps (threshold 2_814_792_717_434_890_000, with
/// a mint supply above 2.8e18) and 10_001 bps. At exactly the 65_535-bps threshold the
/// fee is representable, so the swap gets past `settle` and fails later for want of
/// funds, not with `MathOverflow`.
#[test]
fn bok_inv_fee_19_net_sent_above_threshold_is_math_overflow() {
    let threshold = overflow_threshold(65_535);
    assert_eq!(threshold, 2_814_792_717_434_890_000);
    assert!(fees::ceil_bps(threshold, 65_535).is_some());
    assert!(fees::ceil_bps(threshold + 1, 65_535).is_none());

    let (result, moved) = huge_swap(threshold + 1, 10, 65_535);
    assert_fails_with(result, "MathOverflow");
    assert!(!moved, "nothing moves on MathOverflow");

    let threshold_10001 = overflow_threshold(10_001);
    assert!(fees::ceil_bps(threshold_10001 + 1, 10_001).is_none());
    let (result, moved) = huge_swap(threshold_10001 + 1, 10, 10_001);
    assert_fails_with(result, "MathOverflow");
    assert!(!moved);

    // At the threshold: settle succeeds; the integrator transfer (≈ 6.55 × net_sent)
    // cannot be funded by any u64 balance.
    let (result, moved) = huge_swap(threshold, fees::pipeline_fee(threshold) + 10, 65_535);
    let failed = result.expect_err("an unfundable integrator fee cannot succeed");
    let logs = failed.meta.logs.join("\n");
    assert!(
        !logs.contains("MathOverflow"),
        "the threshold itself is representable:\n{logs}"
    );
    assert!(!moved);
}
