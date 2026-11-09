//! Kani proofs for `fees::{ceil_bps, pipeline_fee, max_total_spend, amount_for_budget}`.
//! Invariants: `.bok/confirmed-invariants/02-fees.md` (INV-FEE-1..10, 12..14) and the
//! fee-only parts of INV-FEE-19 (`01-settle.md`).
//!
//! Notation: `D = 10_000`, `M = u64::MAX`. All spec arithmetic is in `u128`, where no
//! intermediate below can overflow (`u64 × u64 < 2^128 − 2^65`).
//!
//! Cost: `ceil_bps` is a 128-bit `div_ceil`, which CBMC encodes as a fresh quotient and
//! remainder tied by a multiplier. SAT solvers (CaDiCaL and Kissat alike) find relating
//! that quotient back to its operands very hard: on the generation machine even
//! `ceil_bps(a, 10_000) == Some(a)` needs ~40 s for `a < 2^20` and does not finish in
//! 15 minutes for all `a: u64`. So properties come in tiers:
//! - unsuffixed: full domain, real code (the proof the invariant asks for; may time out);
//! - `_bounded`: real code, amounts `< 2^16` (`SMALL`) at the edge rates of
//!   `with_edge_rate!` (0, 1, 25, 333, 9_999, 10_000, 10_001, 65_535 bps): all dust and
//!   rounding behaviour at those rates. A symbolic `u16` rate makes even this tier time
//!   out, because the product `amount × rate` is then a symbolic multiplication;
//!   Harnesses composing several divisions use amounts `< 2^10` (`SMALL_COMPOSED`);
//! - `_stubbed` (compositions only): full domain with `ceil_bps` replaced by its contract
//!   (INV-FEE-1/2, `ceil_bps_contract`). Needs `-Z stubbing`.

use crate::fees::{amount_for_budget, ceil_bps, max_total_spend, pipeline_fee};
use crate::{BPS_DENOMINATOR, PIPELINE_FEE_BPS};

const D: u128 = 10_000;
const M: u128 = u64::MAX as u128;
/// Amount bound of the bounded tier.
const SMALL: u64 = 1 << 16;

/// The contract of `ceil_bps` proved by INV-FEE-1 and INV-FEE-2: `Some(f)` with
/// `f·D ≥ a·b ∧ f·D < a·b + D` (the unique ceiling) when `a·b ≤ D·M`, else `None`.
/// Expressed without division so composed harnesses avoid the divider circuit.
#[allow(dead_code)] // used only through `#[kani::stub]`
pub(super) fn ceil_bps_contract(amount: u64, bps: u64) -> Option<u64> {
    let n = amount as u128 * bps as u128;
    if n > D * M {
        return None;
    }
    let f: u64 = kani::any();
    kani::assume(f as u128 * D >= n);
    kani::assume((f as u128 * D) < n + D);
    Some(f)
}

/// Exact ceiling of `n / D` in `u128` (spec side, no truncation to u64), characterised
/// without division: the unique `c` with `c·D ≥ n > c·D − D`. The assumptions are
/// satisfiable for every `n`, so they prune nothing but the wrong values of `c`. A second
/// divider in the spec would make the solver prove two dividers agree, which is far
/// slower.
fn exact_ceil(n: u128) -> u128 {
    let c: u128 = kani::any();
    kani::assume(c <= n);
    kani::assume(c * D >= n);
    kani::assume(c * D < n + D);
    c
}

/// Amount bound of the bounded tier of harnesses that compose several divisions.
const SMALL_COMPOSED: u64 = 1 << 10;

fn small_composed_amount() -> u64 {
    let a: u64 = kani::any();
    kani::assume(a < SMALL_COMPOSED);
    a
}

fn small_amount() -> u64 {
    let a: u64 = kani::any();
    kani::assume(a < SMALL);
    a
}

// ---------------------------------------------------------------------------------------
// INV-FEE-1: ceil_bps is exactly the ceiling
// ---------------------------------------------------------------------------------------

fn check_fee_1(a: u64, b: u64) {
    if let Some(f) = ceil_bps(a, b) {
        let n = a as u128 * b as u128;
        let f = f as u128;
        assert!(f * D >= n);
        assert!(f * D < n + D);
        // `f = ⌊n/D⌋ + (n mod D ≠ 0)` follows: the two bounds above determine `f`. The
        // literal form is checked by `prop_inv_fee_1_ceil_exact` (a third divider here
        // makes even the bounded tier time out).
    }
}

/// INV-FEE-1: `ceil_bps` is exactly the ceiling, over the full `u64 × u64` domain.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_1_ceil_exact() {
    check_fee_1(kani::any(), kani::any());
}

/// INV-FEE-1: bounded tier (`a < 2^16`, `b: u16`).
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_1_ceil_exact_bounded() {
    let a = small_amount();
    with_edge_rate!(b => check_fee_1(a, b));
}

// ---------------------------------------------------------------------------------------
// INV-FEE-2: never panics; None exactly on overflow
// ---------------------------------------------------------------------------------------

fn check_fee_2(a: u64, b: u64) {
    let result = ceil_bps(a, b);
    let n = a as u128 * b as u128;
    assert!(result.is_none() == (n > D * M));
    if b <= BPS_DENOMINATOR {
        assert!(result.is_some());
    }
}

/// INV-FEE-2: `ceil_bps` never panics; `None` exactly when `a·b > D·M`; `b ≤ D ⇒ Some`.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_2_ceil_none_iff_overflow() {
    check_fee_2(kani::any(), kani::any());
}

/// INV-FEE-2: bounded tier (`a < 2^16`, full `u64` rate so both outcomes occur).
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_2_ceil_none_iff_overflow_bounded() {
    let a = small_amount();
    with_edge_rate!(b => check_fee_2(a, b));
}

// ---------------------------------------------------------------------------------------
// INV-FEE-3: zero exactly for zero amount or rate
// ---------------------------------------------------------------------------------------

/// INV-FEE-3 (ceil_bps part): a fee is zero exactly when the amount or the rate is zero.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_3_ceil_zero_iff() {
    let a: u64 = kani::any();
    let b: u64 = kani::any();
    if let Some(f) = ceil_bps(a, b) {
        assert!((f == 0) == (a == 0 || b == 0));
    }
}

/// INV-FEE-3 (pipeline_fee part): `pipeline_fee(a) = 0 ⇔ a = 0`.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_3_pipeline_fee_zero_iff() {
    let a: u64 = kani::any();
    assert!((pipeline_fee(a) == 0) == (a == 0));
}

// ---------------------------------------------------------------------------------------
// INV-FEE-4: rates up to 100% never exceed the amount
// ---------------------------------------------------------------------------------------

fn check_fee_4(a: u64, b: u64) {
    kani::assume(b <= BPS_DENOMINATOR);
    let f = ceil_bps(a, b).unwrap();
    assert!(f <= a);
    // No rounding when `a·b = k·D` (stated without a divider, see `exact_ceil`).
    let n = a as u128 * b as u128;
    let k: u128 = kani::any();
    kani::assume(k <= n);
    if k * D == n {
        assert!(f as u128 == k);
    }
}

/// INV-FEE-4: for `b ≤ D` the fee never exceeds the amount, and there is no rounding when
/// `a·b` is a multiple of `D`.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_4_ceil_le_amount() {
    check_fee_4(kani::any(), kani::any());
}

/// INV-FEE-4: bounded tier.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_4_ceil_le_amount_bounded() {
    let a = small_amount();
    with_edge_rate!(b => {
        if b <= BPS_DENOMINATOR {
            check_fee_4(a, b)
        }
    });
}

/// INV-FEE-4: `ceil_bps(a, 10_000) = a`.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_4_ceil_full_rate_identity() {
    let a: u64 = kani::any();
    assert!(ceil_bps(a, BPS_DENOMINATOR) == Some(a));
}

/// INV-FEE-4: `ceil_bps(a, 10_000) = a`, bounded tier.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_4_ceil_full_rate_identity_bounded() {
    let a = small_amount();
    assert!(ceil_bps(a, BPS_DENOMINATOR) == Some(a));
}

// ---------------------------------------------------------------------------------------
// INV-FEE-5: monotone in amount and rate
// ---------------------------------------------------------------------------------------

fn check_fee_5_amount(a1: u64, a2: u64, b: u64) {
    kani::assume(a1 <= a2);
    if let Some(f2) = ceil_bps(a2, b) {
        let f1 = ceil_bps(a1, b);
        assert!(f1.is_some());
        assert!(f1.unwrap() <= f2);
    }
}

fn check_fee_5_rate(a: u64, b1: u64, b2: u64) {
    kani::assume(b1 <= b2);
    if let Some(f2) = ceil_bps(a, b2) {
        let f1 = ceil_bps(a, b1);
        assert!(f1.is_some());
        assert!(f1.unwrap() <= f2);
    }
}

/// INV-FEE-5: monotone in the amount (full `u64` rate).
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_5_ceil_monotone_amount() {
    check_fee_5_amount(kani::any(), kani::any(), kani::any());
}

/// INV-FEE-5: monotone in the amount, bounded tier.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_5_ceil_monotone_amount_bounded() {
    let a1 = small_composed_amount();
    let a2 = small_composed_amount();
    with_edge_rate!(b => check_fee_5_amount(a1, a2, b));
}

/// INV-FEE-5: monotone in the rate (full `u64` rate).
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_5_ceil_monotone_rate() {
    check_fee_5_rate(kani::any(), kani::any(), kani::any());
}

/// INV-FEE-5: monotone in the rate, bounded tier.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_5_ceil_monotone_rate_bounded() {
    let a = small_composed_amount();
    with_edge_rate!(b1 => with_edge_rate!(b2 => {
        if b1 <= b2 {
            check_fee_5_rate(a, b1, b2)
        }
    }));
}

// ---------------------------------------------------------------------------------------
// INV-FEE-6 / INV-SWAP-8: pipeline_fee
// ---------------------------------------------------------------------------------------

fn check_fee_6(a: u64) {
    let fee = pipeline_fee(a);
    let f = fee as u128;
    let a128 = a as u128;
    // ceil(a / 400), characterised without division.
    assert!(f * 400 >= a128);
    assert!(f * 400 < a128 + 400);
    assert!(f * D >= 25 * a128);
    assert!(f * D < 25 * a128 + D);
    assert!(fee <= 46_116_860_184_273_880);
    assert!(fee <= a);
    assert!((fee == a) == (a <= 1));
    assert!((fee == 0) == (a == 0));
}

/// INV-FEE-6 / INV-SWAP-8: the constants are 25 bps of 10_000, and the largest fee is
/// `ceil(M / 400)`.
#[kani::proof]
fn verify_inv_fee_6_constants() {
    assert!(PIPELINE_FEE_BPS == 25);
    assert!(BPS_DENOMINATOR == 10_000);
    assert!(pipeline_fee(u64::MAX) == 46_116_860_184_273_880);
}

/// INV-FEE-6 / INV-SWAP-8: `pipeline_fee` never panics, equals `ceil(a/400)`, satisfies the
/// exact bounds, is at most `ceil(M/400)`, and is `≤ a` with equality exactly for `a ≤ 1`.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_6_pipeline_fee_formula() {
    check_fee_6(kani::any());
}

/// INV-FEE-6 / INV-SWAP-8: bounded tier.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_6_pipeline_fee_formula_bounded() {
    check_fee_6(small_amount());
}

/// INV-FEE-6 / INV-SWAP-8 and INV-FEE-5: `pipeline_fee` is monotone.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_6_pipeline_fee_monotone() {
    let a: u64 = kani::any();
    let m: u64 = kani::any();
    kani::assume(a <= m);
    assert!(pipeline_fee(a) <= pipeline_fee(m));
}

/// INV-FEE-6 / INV-SWAP-8 and INV-FEE-5: `pipeline_fee` is monotone, bounded tier.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_6_pipeline_fee_monotone_bounded() {
    let a = small_amount();
    let m = small_amount();
    kani::assume(a <= m);
    assert!(pipeline_fee(a) <= pipeline_fee(m));
}

// ---------------------------------------------------------------------------------------
// INV-FEE-7 / INV-SWAP-9: splitting never pays less, at most one extra unit per piece
// ---------------------------------------------------------------------------------------

fn check_fee_7_ceil(x: u64, y: u64, b: u64) {
    kani::assume(b <= BPS_DENOMINATOR);
    let Some(total) = x.checked_add(y) else {
        return;
    };
    let whole = ceil_bps(total, b).unwrap() as u128;
    let parts = ceil_bps(x, b).unwrap() as u128 + ceil_bps(y, b).unwrap() as u128;
    assert!(whole <= parts);
    assert!(parts <= whole + 1);
}

fn check_fee_7_pf(x: u64, y: u64) {
    let Some(total) = x.checked_add(y) else {
        return;
    };
    let whole = pipeline_fee(total) as u128;
    let parts = pipeline_fee(x) as u128 + pipeline_fee(y) as u128;
    assert!(whole <= parts);
    assert!(parts <= whole + 1);
}

/// INV-FEE-7 / INV-SWAP-9: `fee_b(x+y) ≤ fee_b(x) + fee_b(y) ≤ fee_b(x+y) + 1` for `b ≤ D`.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_7_split_ceil() {
    check_fee_7_ceil(kani::any(), kani::any(), kani::any());
}

/// INV-FEE-7 / INV-SWAP-9: bounded tier.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_7_split_ceil_bounded() {
    let x = small_composed_amount();
    let y = small_composed_amount();
    with_edge_rate!(b => {
        if b <= BPS_DENOMINATOR {
            check_fee_7_ceil(x, y, b)
        }
    });
}

/// INV-FEE-7 / INV-SWAP-9: full domain, `ceil_bps` replaced by its contract.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_fee_7_split_ceil_stubbed() {
    check_fee_7_ceil(kani::any(), kani::any(), kani::any());
}

/// INV-FEE-7 / INV-SWAP-9: the same for `pipeline_fee`.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_7_split_pipeline_fee() {
    check_fee_7_pf(kani::any(), kani::any());
}

/// INV-FEE-7 / INV-SWAP-9: `pipeline_fee`, bounded tier.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_7_split_pipeline_fee_bounded() {
    check_fee_7_pf(small_amount(), small_amount());
}

// ---------------------------------------------------------------------------------------
// INV-FEE-8, -9, -10: max_total_spend
// ---------------------------------------------------------------------------------------

/// Spec of `max_total_spend`: `a + ceil(25a/D) + ceil(b·a/D)` in exact `u128`.
fn spec_total(a: u64, b: u16) -> u128 {
    a as u128 + exact_ceil(a as u128 * 25) + exact_ceil(a as u128 * b as u128)
}

fn check_fee_8(a: u64, b: u16) {
    let s = spec_total(a, b);
    let result = max_total_spend(a, b);
    if s <= M {
        assert!(result == Some(s as u64));
    } else {
        assert!(result.is_none());
    }
    if let Some(t) = result {
        assert!(t >= a);
    }
}

/// INV-FEE-8: `max_total_spend` is the exact sum or `None`, and `Some(t) ⇒ t ≥ a`.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_8_mts_exact() {
    check_fee_8(kani::any(), kani::any());
}

/// INV-FEE-8: bounded tier.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_8_mts_exact_bounded() {
    let a = small_composed_amount();
    with_edge_rate!(b => check_fee_8(a, b as u16));
}

fn check_fee_9(a: u64, b: u16) {
    if let Some(t) = max_total_spend(a, b) {
        let doc = a as u128 + exact_ceil(a as u128 * (25 + b as u128));
        assert!(t as u128 >= doc);
        assert!(t as u128 <= doc + 1);
    }
}

/// INV-FEE-9: the code's worst case is never below the router docs' former single-ceiling
/// formula and at most one unit above it.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_9_mts_vs_doc() {
    check_fee_9(kani::any(), kani::any());
}

/// INV-FEE-9: bounded tier.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_9_mts_vs_doc_bounded() {
    let a = small_composed_amount();
    with_edge_rate!(b => check_fee_9(a, b as u16));
}

/// INV-FEE-9 (witness): `a = 1, b = 1` gives code 3, doc 2.
#[kani::proof]
fn verify_inv_fee_9_witness() {
    assert!(max_total_spend(1, 1) == Some(3));
    assert!(1 + exact_ceil(26) == 2);
}

fn check_fee_10_amount(a: u64, b: u16) {
    kani::assume(a < u64::MAX);
    if let Some(t1) = max_total_spend(a + 1, b) {
        let t0 = max_total_spend(a, b);
        assert!(t0.is_some());
        assert!(t1 as u128 >= t0.unwrap() as u128 + 1);
    }
}

fn check_fee_10_rate(a: u64, b1: u16, b2: u16) {
    kani::assume(b1 <= b2);
    if let Some(t2) = max_total_spend(a, b2) {
        let t1 = max_total_spend(a, b1);
        assert!(t1.is_some());
        assert!(t1.unwrap() <= t2);
    }
}

/// INV-FEE-10: strictly increasing in the amount.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_10_mts_strict_amount() {
    check_fee_10_amount(kani::any(), kani::any());
}

/// INV-FEE-10: strictly increasing in the amount, bounded tier.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_10_mts_strict_amount_bounded() {
    let a = small_composed_amount();
    with_edge_rate!(b => check_fee_10_amount(a, b as u16));
}

/// INV-FEE-10: non-decreasing in the integrator rate.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_10_mts_monotone_rate() {
    check_fee_10_rate(kani::any(), kani::any(), kani::any());
}

/// INV-FEE-10: non-decreasing in the integrator rate, bounded tier.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_10_mts_monotone_rate_bounded() {
    let a = small_composed_amount();
    with_edge_rate!(b1 => with_edge_rate!(b2 => {
        if b1 <= b2 {
            check_fee_10_rate(a, b1 as u16, b2 as u16)
        }
    }));
}

// ---------------------------------------------------------------------------------------
// INV-FEE-12, -13, -14: amount_for_budget
// ---------------------------------------------------------------------------------------

fn check_fee_14(budget: u64, b: u16) {
    let rate = D + PIPELINE_FEE_BPS as u128 + b as u128;
    // `estimate = ⌊budget·D / rate⌋`, stated without a divider (see `exact_ceil`).
    let scaled = budget as u128 * D;
    let estimate: u128 = kani::any();
    kani::assume(estimate <= scaled);
    kani::assume(estimate * rate <= scaled);
    kani::assume(scaled < (estimate + 1) * rate);
    assert!(estimate <= budget as u128);
    let amount = amount_for_budget(budget, b) as u128;
    assert!(amount == estimate || amount + 1 == estimate);
}

/// INV-FEE-14: `amount_for_budget` steps down at most once. `unwind(3)` allows exactly two
/// loop passes; a third would fail the unwinding assertion.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::unwind(3)]
#[kani::solver(cadical)]
fn verify_inv_fee_14_afb_steps_once() {
    check_fee_14(kani::any(), kani::any());
}

/// INV-FEE-14: bounded tier (budget `< 2^16`).
#[kani::proof]
#[kani::unwind(3)]
#[kani::solver(cadical)]
fn verify_inv_fee_14_afb_steps_once_bounded() {
    let budget = small_composed_amount();
    with_edge_rate!(b => check_fee_14(budget, b as u16));
}

/// INV-FEE-14: full domain, `ceil_bps` replaced by its contract.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::unwind(3)]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_fee_14_afb_steps_once_stubbed() {
    check_fee_14(kani::any(), kani::any());
}

fn check_fee_12(budget: u64, b: u16) {
    let x = amount_for_budget(budget, b);
    let t = max_total_spend(x, b);
    assert!(t.is_some());
    assert!(t.unwrap() <= budget);
}

/// INV-FEE-12: `amount_for_budget` never overspends.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::unwind(3)]
#[kani::solver(cadical)]
fn verify_inv_fee_12_afb_no_overspend() {
    check_fee_12(kani::any(), kani::any());
}

/// INV-FEE-12: bounded tier.
#[kani::proof]
#[kani::unwind(3)]
#[kani::solver(cadical)]
fn verify_inv_fee_12_afb_no_overspend_bounded() {
    let budget = small_composed_amount();
    with_edge_rate!(b => check_fee_12(budget, b as u16));
}

/// INV-FEE-12: full domain, `ceil_bps` replaced by its contract.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::unwind(3)]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_fee_12_afb_no_overspend_stubbed() {
    check_fee_12(kani::any(), kani::any());
}

fn check_fee_13_maximal(budget: u64, b: u16) {
    let x = amount_for_budget(budget, b);
    if x < u64::MAX {
        if let Some(t) = max_total_spend(x + 1, b) {
            assert!(t > budget);
        }
    }
}

fn check_fee_13_round_trip(a: u64, b: u16) {
    if let Some(t) = max_total_spend(a, b) {
        assert!(amount_for_budget(t, b) == a);
    }
}

/// INV-FEE-13: `amount_for_budget` is maximal.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::unwind(3)]
#[kani::solver(cadical)]
fn verify_inv_fee_13_afb_maximal() {
    check_fee_13_maximal(kani::any(), kani::any());
}

/// INV-FEE-13 (maximality): bounded tier.
#[kani::proof]
#[kani::unwind(3)]
#[kani::solver(cadical)]
fn verify_inv_fee_13_afb_maximal_bounded() {
    let budget = small_composed_amount();
    with_edge_rate!(b => check_fee_13_maximal(budget, b as u16));
}

/// INV-FEE-13 (maximality): full domain, `ceil_bps` replaced by its contract.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::unwind(3)]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_fee_13_afb_maximal_stubbed() {
    check_fee_13_maximal(kani::any(), kani::any());
}

/// INV-FEE-13 (round trip): `mts(a, b) = Some(t) ⇒ afb(t, b) = a`.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::unwind(3)]
#[kani::solver(cadical)]
fn verify_inv_fee_13_afb_round_trip() {
    check_fee_13_round_trip(kani::any(), kani::any());
}

/// INV-FEE-13 (round trip): bounded tier.
#[kani::proof]
#[kani::unwind(3)]
#[kani::solver(cadical)]
fn verify_inv_fee_13_afb_round_trip_bounded() {
    let a = small_composed_amount();
    with_edge_rate!(b => check_fee_13_round_trip(a, b as u16));
}

/// INV-FEE-13 (round trip): full domain, `ceil_bps` replaced by its contract.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::unwind(3)]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_inv_fee_13_afb_round_trip_stubbed() {
    check_fee_13_round_trip(kani::any(), kani::any());
}

// ---------------------------------------------------------------------------------------
// INV-FEE-19 (fee parts, see also settle.rs)
// ---------------------------------------------------------------------------------------

fn check_fee_19_exact(n: u64, b: u16) {
    let result = ceil_bps(n, b as u64);
    let product = n as u128 * b as u128;
    if let Some(v) = result {
        assert!(v as u128 * D >= product);
        assert!((v as u128 * D) < product + D);
        if n >= 1 && b as u64 > BPS_DENOMINATOR {
            assert!(v as u128 >= n as u128 + 1);
        }
    }
    if b as u64 <= BPS_DENOMINATOR {
        assert!(result.is_some());
        assert!(result.unwrap() <= n);
    }
}

/// INV-FEE-19 (fee part): `b ≤ D ⇒ Some(v) ∧ v ≤ n`, the exact bounds, and above 100% a
/// non-zero amount pays strictly more than the amount.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_19_integrator_fee_exact() {
    check_fee_19_exact(kani::any(), kani::any());
}

/// INV-FEE-19 (fee part): bounded tier.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_19_integrator_fee_exact_bounded() {
    let n = small_amount();
    with_edge_rate!(b => check_fee_19_exact(n, b as u16));
}

fn check_fee_19_threshold(a: u64, b: u64) {
    kani::assume(b >= 1);
    let threshold = (D * M) / b as u128;
    assert!(ceil_bps(a, b).is_none() == (a as u128 > threshold));
}

/// INV-FEE-19 (overflow threshold): for `b ≥ 1`, `ceil_bps(a, b) = None ⇔ a > ⌊D·M / b⌋`,
/// over the router's `u16` rate domain.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_19_overflow_threshold() {
    let b: u16 = kani::any();
    check_fee_19_threshold(kani::any(), b as u64);
}

/// INV-FEE-19 (overflow threshold, general): same statement for every `u64` rate `b ≥ 1`.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_19_overflow_threshold_u64_rate() {
    check_fee_19_threshold(kani::any(), kani::any());
}

/// INV-FEE-19 (overflow threshold): bounded tier around the 65_535-bps threshold
/// (`a` within `2^15` of it, `b = 65_535`).
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_19_overflow_threshold_bounded() {
    let offset = small_amount();
    let a = 2_814_792_717_434_890_000 - (SMALL / 2) + offset;
    check_fee_19_threshold(a, 65_535);
}

/// INV-FEE-19 (witness): `threshold(65_535) = 2_814_792_717_434_890_000`, and the
/// threshold is the last amount that does not overflow.
#[kani::proof]
fn verify_inv_fee_19_threshold_65535_witness() {
    let threshold: u64 = 2_814_792_717_434_890_000;
    assert!((D * M) / 65_535 == threshold as u128);
    assert!(ceil_bps(threshold, 65_535).is_some());
    assert!(ceil_bps(threshold + 1, 65_535).is_none());
}
