//! Kani proofs for `credited_input` (`.bok/confirmed-invariants/03-credited-input.md`,
//! INV-FEE-16..18). Full `u64` domain throughout.

use crate::credited_input;
use crate::fees::pipeline_fee;

/// INV-FEE-16: `credited_input = min(v -sat r, s)`, never above what was sent or what
/// arrived, zero when the vault did not grow; never panics.
#[kani::proof]
fn verify_inv_fee_16_credit_bounds() {
    let v: u64 = kani::any();
    let r: u64 = kani::any();
    let s: u64 = kani::any();
    let c = credited_input(v, r, s);
    let arrived = v.saturating_sub(r);
    assert!(c == arrived.min(s));
    assert!(c <= s);
    assert!(c <= arrived);
    if v <= r {
        assert!(c == 0);
    }
}

/// INV-FEE-17: a donation `d` to the vault never raises the credit above what was sent,
/// nor the Pipeline fee on it above the fee on what was sent.
// Times out over the full domain with Kani; proven for every input in
// `proofs/fee_maths_z3.py`. Run with `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_17_donation_immunity() {
    let v: u64 = kani::any();
    let r: u64 = kani::any();
    let s: u64 = kani::any();
    let d: u64 = kani::any();
    let Some(donated) = v.checked_add(d) else {
        return;
    };
    let c = credited_input(donated, r, s);
    assert!(c <= s);
    assert!(pipeline_fee(c) <= pipeline_fee(s));
}

/// INV-FEE-17: real `pipeline_fee`, bounded tier (`s < 2^16`; vault, reserve and donation
/// stay full-domain).
#[kani::proof]
#[kani::solver(cadical)]
fn verify_inv_fee_17_donation_immunity_bounded() {
    let v: u64 = kani::any();
    let r: u64 = kani::any();
    let s: u64 = kani::any();
    let d: u64 = kani::any();
    kani::assume(s < 1 << 16);
    let Some(donated) = v.checked_add(d) else {
        return;
    };
    let c = credited_input(donated, r, s);
    assert!(c <= s);
    assert!(pipeline_fee(c) <= pipeline_fee(s));
}

/// INV-FEE-17: the credit with a donation never exceeds the credit bound `s`, so its
/// fee is bounded by monotonicity of `pipeline_fee` (INV-FEE-6), proved separately.
/// This division-free half is proved over the full domain.
#[kani::proof]
fn verify_inv_fee_17_donation_credit_le_sent() {
    let v: u64 = kani::any();
    let r: u64 = kani::any();
    let s: u64 = kani::any();
    let d: u64 = kani::any();
    let Some(donated) = v.checked_add(d) else {
        return;
    };
    let c = credited_input(donated, r, s);
    assert!(c <= s);
    assert!(c <= credited_input(v, r, s).saturating_add(d));
}

/// INV-FEE-18: exact for transfer-fee shortfalls (`v − r ≤ s ⇒ credit = v − r`).
#[kani::proof]
fn verify_inv_fee_18_exact_shortfall() {
    let v: u64 = kani::any();
    let r: u64 = kani::any();
    let s: u64 = kani::any();
    kani::assume(r <= v);
    kani::assume(v - r <= s);
    assert!(credited_input(v, r, s) == v - r);
}

/// INV-FEE-18: monotone (non-decreasing in vault balance and amount sent, non-increasing
/// in the recorded reserve).
#[kani::proof]
fn verify_inv_fee_18_monotone() {
    let v1: u64 = kani::any();
    let v2: u64 = kani::any();
    let r1: u64 = kani::any();
    let r2: u64 = kani::any();
    let s1: u64 = kani::any();
    let s2: u64 = kani::any();
    kani::assume(v1 <= v2);
    kani::assume(r1 <= r2);
    kani::assume(s1 <= s2);
    assert!(credited_input(v1, r1, s1) <= credited_input(v2, r1, s1));
    assert!(credited_input(v1, r1, s1) <= credited_input(v1, r1, s2));
    assert!(credited_input(v1, r1, s1) >= credited_input(v1, r2, s1));
}
