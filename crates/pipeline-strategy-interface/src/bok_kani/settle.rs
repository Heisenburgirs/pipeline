//! Kani proofs for `settlement::{settle, hook_split}`, proved directly on `settle`.
//! Invariants: `.bok/confirmed-invariants/01-settle.md` (INV-SWAP-1..4, 6, 7, 15, 19, 20,
//! INV-FEE-11/SWAP-5, INV-FEE-19/SWAP-10) and INV-SWAP-16 from `04-swap-handler.md`.
//!
//! Every input is a full-domain `u64` (balances, `amount_in`, `min_amount_out`) or `u16`
//! (integrator bps). Harnesses suffixed `_stubbed` replace `ceil_bps` (and so also
//! `pipeline_fee`, which calls it) by its contract proved in `fees.rs` (INV-FEE-1/2); the
//! unsuffixed ones run the real `ceil_bps`.

#[allow(unused_imports)] // used only through `#[kani::stub]`
use super::fees::ceil_bps_contract;
use crate::fees::{ceil_bps, max_total_spend, pipeline_fee};
use crate::settlement::{hook_split, settle, Measured, SettleError, Settlement};

const D: u128 = 10_000;

fn any_measured() -> Measured {
    Measured {
        input_before: kani::any(),
        input_after: kani::any(),
        output_before: kani::any(),
        output_after: kani::any(),
        vault_before: kani::any(),
        vault_after: kani::any(),
    }
}

/// The three measured deltas: `(spent, recv, kept)`.
fn deltas(m: &Measured) -> (u64, u64, u64) {
    (
        m.input_before.saturating_sub(m.input_after),
        m.output_after.saturating_sub(m.output_before),
        m.vault_after.saturating_sub(m.vault_before),
    )
}

/// Symbolic arguments for one `settle` call.
struct Args {
    m: Measured,
    amount_in: u64,
    min_out: u64,
    bps: u16,
}

fn any_args() -> Args {
    Args {
        m: any_measured(),
        amount_in: kani::any(),
        min_out: kani::any(),
        bps: kani::any(),
    }
}

fn run(a: &Args) -> Result<Settlement, SettleError> {
    settle(&a.m, a.amount_in, a.min_out, a.bps)
}

/// Exact ceiling of `n / D` as a `u128`, characterised without division (the unique
/// value with `c·D ≥ n > c·D − D`; the assumptions are satisfiable for every `n`).
fn exact_ceil(n: u128) -> u128 {
    let c: u128 = kani::any();
    kani::assume(c <= n);
    kani::assume(c * D >= n);
    kani::assume(c * D < n + D);
    c
}

// ---------------------------------------------------------------------------------------
// INV-SWAP-20: totality
// ---------------------------------------------------------------------------------------

fn check_swap_20(a: &Args) {
    // No panic is checked by Kani itself (overflow, `expect`, `unwrap`).
    if let Err(SettleError::MathOverflow) = run(a) {
        assert!(a.bps > 10_000);
    }
}

/// INV-SWAP-20: `settle` never panics on any input, and `MathOverflow ⇒ bps > 10_000`.
/// Real `ceil_bps`, so this also proves the `pipeline_fee` `expect` cannot fire inside
/// `settle`.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_swap_20_settle_total() {
    check_swap_20(&any_args());
}

/// INV-SWAP-20: real `ceil_bps`, bounded tier (`amount_in < 2^10`, edge rates).
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_swap_20_settle_total_bounded() {
    small_args_edge_rate(check_swap_20);
}

/// INV-SWAP-20: same with `ceil_bps` stubbed by its contract.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_swap_20_settle_total_stubbed() {
    check_swap_20(&any_args());
}

// ---------------------------------------------------------------------------------------
// INV-SWAP-1, -2, -3, -4, -6: the fee base and the checks
// ---------------------------------------------------------------------------------------

/// INV-SWAP-1: on success the fee base never exceeds `amount_in`.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_swap_1_net_le_amount_in_stubbed() {
    let a = any_args();
    if let Ok(s) = run(&a) {
        assert!(s.net_sent <= a.amount_in);
    }
}

/// INV-SWAP-2: on success the fee base is at least what the trader parted with.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_swap_2_net_ge_spent_stubbed() {
    let a = any_args();
    if let Ok(s) = run(&a) {
        let (spent, _, _) = deltas(&a.m);
        assert!(s.trader_spent == spent);
        assert!(s.net_sent >= s.trader_spent);
    }
}

/// INV-SWAP-3: refilling `trader_input` cannot shrink the base below what the vault
/// kept; the base is exactly `min(max(spent, kept), amount_in)`.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_swap_3_net_formula_stubbed() {
    let a = any_args();
    if let Ok(s) = run(&a) {
        let (spent, _, kept) = deltas(&a.m);
        assert!(s.vault_kept == kept);
        assert!(s.net_sent >= kept.min(a.amount_in));
        assert!(s.net_sent == s.trader_spent.max(s.vault_kept).min(a.amount_in));
        assert!(s.net_sent == spent.max(kept).min(a.amount_in));
    }
}

/// INV-SWAP-4: the trader's input never drops by more than `amount_in` on success, and an
/// over-spend that passes the output check is refused with `SpendLimitExceeded`.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_swap_4_spend_limit_stubbed() {
    let a = any_args();
    let (spent, recv, _) = deltas(&a.m);
    let result = run(&a);
    if result.is_ok() {
        assert!(spent <= a.amount_in);
    }
    if spent > a.amount_in && recv >= a.min_out {
        assert!(result == Err(SettleError::SpendLimitExceeded));
    }
}

/// INV-SWAP-6: minimum output on the measured delta, checked before everything else.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_swap_6_min_out_first_stubbed() {
    let a = any_args();
    let (_, recv, _) = deltas(&a.m);
    let result = run(&a);
    if let Ok(s) = result {
        assert!(s.received == recv);
        assert!(s.received >= a.min_out);
    }
    if recv < a.min_out {
        assert!(result == Err(SettleError::InsufficientOutput));
    }
}

// ---------------------------------------------------------------------------------------
// INV-SWAP-7, -19: nothing for nothing
// ---------------------------------------------------------------------------------------

/// INV-SWAP-7: every success has `net_sent ≥ 1` and pays `pipeline_fee ≥ 1`; output with
/// no input is refused with `OutputWithoutInput`.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_swap_7_no_output_without_input_stubbed() {
    let a = any_args();
    let (spent, recv, kept) = deltas(&a.m);
    let result = run(&a);
    if let Ok(s) = result {
        assert!(s.net_sent >= 1);
        assert!(s.pipeline_fee >= 1);
    }
    let base = spent.max(kept).min(a.amount_in);
    if recv >= a.min_out && spent <= a.amount_in && base == 0 && recv > 0 {
        assert!(result == Err(SettleError::OutputWithoutInput));
    }
}

/// INV-SWAP-7: `pipeline_fee ≥ 1` on success with the real `ceil_bps`, full domain.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_swap_7_fee_at_least_one() {
    let a = any_args();
    if let Ok(s) = run(&a) {
        assert!(s.net_sent >= 1);
        assert!(s.pipeline_fee >= 1);
    }
}

/// INV-SWAP-19 (adjusted): a swap whose fee base is zero is always refused, with
/// `OutputWithoutInput` if output arrived and `NothingSwapped` otherwise.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_swap_19_zero_base_refused_stubbed() {
    let a = any_args();
    let (spent, recv, kept) = deltas(&a.m);
    let result = run(&a);
    let base = spent.max(kept).min(a.amount_in);
    if base == 0 && recv >= a.min_out && spent <= a.amount_in {
        let expected = if recv > 0 {
            SettleError::OutputWithoutInput
        } else {
            SettleError::NothingSwapped
        };
        assert!(result == Err(expected));
    }
    if let Ok(s) = result {
        assert!(s.net_sent > 0);
    }
}

// ---------------------------------------------------------------------------------------
// INV-FEE-11 / INV-SWAP-5: never more than max_total_spend
// ---------------------------------------------------------------------------------------

fn check_fee_11(a: &Args) {
    if let Ok(s) = run(a) {
        let lhs = s.trader_spent as u128 + s.pipeline_fee as u128 + s.integrator_fee as u128;
        let amount = a.amount_in as u128;
        let rhs = amount + exact_ceil(25 * amount) + exact_ceil(a.bps as u128 * amount);
        assert!(lhs <= rhs);
        if let Some(m) = max_total_spend(a.amount_in, a.bps) {
            assert!(lhs <= m as u128);
        }
    }
}

/// Arguments with `amount_in < 2^10` (every fee base is then `< 2^10`); balances, the
/// minimum output and the rate stay full-domain.
fn small_args() -> Args {
    let a = any_args();
    kani::assume(a.amount_in < 1 << 10);
    a
}

/// `small_args` with the rate fixed to one of the `with_edge_rate!` constants.
fn small_args_edge_rate(check: impl Fn(&Args)) {
    let mut a = small_args();
    with_edge_rate!(b => {
        a.bps = b as u16;
        check(&a)
    });
}

/// INV-FEE-11 / INV-SWAP-5: `trader_spent + pipeline_fee + integrator_fee` never exceeds
/// `amount_in` plus both fees on `amount_in`, nor `max_total_spend(amount_in, bps)`.
/// Full domain, `ceil_bps` stubbed by its contract.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_fee_11_spend_bound_stubbed() {
    check_fee_11(&any_args());
}

/// INV-FEE-11 / INV-SWAP-5: full-domain balances and `amount_in`, `ceil_bps` stubbed by
/// its contract, rate fixed to each `with_edge_rate!` constant (a symbolic rate makes the
/// contract's products symbolic multiplications).
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_fee_11_spend_bound_stubbed_edge_rates() {
    let mut a = any_args();
    with_edge_rate!(b => {
        a.bps = b as u16;
        check_fee_11(&a)
    });
}

/// INV-FEE-11 / INV-SWAP-5: full domain, real `ceil_bps` (four 128-bit divisions).
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_11_spend_bound() {
    check_fee_11(&any_args());
}

/// INV-FEE-11 / INV-SWAP-5: real `ceil_bps`, bounded tier (`amount_in < 2^10`, edge rates).
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_11_spend_bound_bounded() {
    small_args_edge_rate(check_fee_11);
}

// ---------------------------------------------------------------------------------------
// INV-FEE-19 / INV-SWAP-10: the fees are computed on net_sent and fail safely
// ---------------------------------------------------------------------------------------

/// The fees satisfy the defining inequalities of `ceil(net·25/D)` and `ceil(net·b/D)`
/// (which pin them to `pipeline_fee(net)` and `ceil_bps(net, b)` by INV-FEE-1), and
/// `MathOverflow` happens exactly when the checks pass and `net·b > D·M`. With
/// `recompute`, the fees are also compared with fresh calls of the real functions.
fn check_fee_19(a: &Args, recompute: bool) {
    let (spent, recv, kept) = deltas(&a.m);
    let result = run(a);
    let base = spent.max(kept).min(a.amount_in);
    if let Ok(s) = result {
        let rate = (a.bps as u64) as u128;
        let n = s.net_sent as u128 * rate;
        assert!(s.integrator_fee as u128 * D >= n);
        assert!((s.integrator_fee as u128 * D) < n + D);
        let p = s.net_sent as u128 * 25;
        assert!(s.pipeline_fee as u128 * D >= p);
        assert!((s.pipeline_fee as u128 * D) < p + D);
        if a.bps <= 10_000 {
            assert!(s.integrator_fee <= s.net_sent);
        }
        if recompute {
            assert!(Some(s.integrator_fee) == ceil_bps(s.net_sent, a.bps as u64));
            assert!(s.pipeline_fee == pipeline_fee(s.net_sent));
        }
    }
    let checks_pass = recv >= a.min_out && spent <= a.amount_in && base > 0;
    let overflow = checks_pass && base as u128 * (a.bps as u64) as u128 > D * u64::MAX as u128;
    assert!((result == Err(SettleError::MathOverflow)) == overflow);
}

/// INV-FEE-19 / INV-SWAP-10: full domain, `ceil_bps` stubbed by its contract.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_fee_19_settle_fees_stubbed() {
    check_fee_19(&any_args(), false);
}

/// INV-FEE-19 / INV-SWAP-10: full-domain balances and `amount_in`, `ceil_bps` stubbed,
/// rate fixed to each `with_edge_rate!` constant.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_fee_19_settle_fees_stubbed_edge_rates() {
    let mut a = any_args();
    with_edge_rate!(b => {
        a.bps = b as u16;
        check_fee_19(&a, false)
    });
}

/// INV-FEE-19 / INV-SWAP-10: full domain, real `ceil_bps`, fees recomputed.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_19_settle_fees() {
    check_fee_19(&any_args(), true);
}

/// INV-FEE-19 / INV-SWAP-10: real `ceil_bps`, bounded tier (`amount_in < 2^10`, edge rates).
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_19_settle_fees_bounded() {
    small_args_edge_rate(|a| check_fee_19(a, true));
}

/// INV-FEE-19 / INV-SWAP-10: `MathOverflow` on the real code near the 65_535-bps
/// threshold (the only place it can happen), balances and checks otherwise symbolic.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_19_settle_overflow_near_threshold() {
    let mut a = any_args();
    let offset: u64 = kani::any();
    kani::assume(offset < 1 << 8);
    a.amount_in = 2_814_792_717_434_890_000 - 128 + offset;
    a.bps = 65_535;
    check_fee_19(&a, false);
}

// ---------------------------------------------------------------------------------------
// INV-SWAP-15: transfer-fee input
// ---------------------------------------------------------------------------------------

/// INV-SWAP-15: with a transfer-fee input (`kept ≤ spent ≤ amount_in`) the base is the
/// trader's spend: no gross-up, no discount to what the vault kept.
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_swap_15_transfer_fee_base_stubbed() {
    let a = any_args();
    let (spent, _, kept) = deltas(&a.m);
    kani::assume(kept <= spent);
    kani::assume(spent <= a.amount_in);
    if let Ok(s) = run(&a) {
        assert!(s.net_sent == spent);
    }
}

// ---------------------------------------------------------------------------------------
// INV-SWAP-16: hook_split
// ---------------------------------------------------------------------------------------

/// INV-SWAP-16: `hook_split` never panics, is `None` exactly when there are fewer remaining
/// accounts than hook accounts, and otherwise selects exactly the trailing `k`.
#[kani::proof]
fn verify_inv_swap_16_hook_split() {
    let len: usize = kani::any();
    let k: u8 = kani::any();
    let result = hook_split(len, k);
    assert!(result.is_none() == (k as usize > len));
    if let Some(i) = result {
        assert!(i == len - k as usize);
        assert!(i <= len);
        assert!(len - i == k as usize);
    }
}
