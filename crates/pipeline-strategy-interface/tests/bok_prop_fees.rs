//! Book of Knowledge property tests for the fee maths and `credited_input`.
//! Invariants: `.bok/confirmed-invariants/02-fees.md` and `03-credited-input.md`.
//! Fast random layer alongside the Kani proofs in `src/bok_kani/`.

use pipeline_strategy_interface::credited_input;
use pipeline_strategy_interface::fees::{
    amount_for_budget, ceil_bps, max_total_spend, pipeline_fee,
};
use proptest::prelude::*;

const D: u128 = 10_000;
const M: u128 = u64::MAX as u128;
/// Largest amount whose 65_535-bps fee fits in a u64: `⌊D·M / 65_535⌋`.
const THRESHOLD_65535: u64 = 2_814_792_717_434_890_000;

/// u64 amounts weighted towards edges: 0, 1, small, around the 65_535-bps threshold,
/// around `u64::MAX`, and uniform.
fn amount() -> impl Strategy<Value = u64> {
    prop_oneof![
        1 => Just(0u64),
        1 => Just(1u64),
        1 => Just(u64::MAX),
        1 => Just(u64::MAX - 1),
        1 => Just(THRESHOLD_65535),
        1 => Just(THRESHOLD_65535 + 1),
        2 => (THRESHOLD_65535 - 1_000)..=(THRESHOLD_65535 + 1_000),
        2 => (u64::MAX - 1_000_000)..=u64::MAX,
        3 => 0u64..100_000,
        3 => 0u64..(1u64 << 40),
        6 => any::<u64>(),
    ]
}

/// u16 integrator rates weighted towards 0, 1, 25, 10_000, 10_001, 65_535.
fn bps16() -> impl Strategy<Value = u16> {
    prop_oneof![
        1 => Just(0u16),
        1 => Just(1u16),
        1 => Just(25u16),
        1 => Just(9_999u16),
        1 => Just(10_000u16),
        1 => Just(10_001u16),
        1 => Just(65_535u16),
        4 => any::<u16>(),
    ]
}

/// u64 rates: the u16 edges plus very large rates.
fn bps64() -> impl Strategy<Value = u64> {
    prop_oneof![
        4 => bps16().prop_map(u64::from),
        1 => Just(u64::MAX),
        1 => 65_536u64..1_000_000,
        2 => any::<u64>(),
    ]
}

/// A split of some total into `1..=40` pieces whose sum fits in a u64.
fn split() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(amount(), 1..=40).prop_map(|mut pieces| {
        // Scale pieces down until the sum fits.
        let n = pieces.len() as u64;
        let mut sum: u128 = pieces.iter().map(|&p| p as u128).sum();
        if sum > M {
            for p in pieces.iter_mut() {
                *p /= n;
            }
            sum = pieces.iter().map(|&p| p as u128).sum();
        }
        assert!(sum <= M);
        pieces
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// INV-FEE-1 (sanity): `ceil_bps` is the exact ceiling over u64 × u64.
    #[test]
    fn prop_inv_fee_1_ceil_exact(a in amount(), b in bps64()) {
        let n = a as u128 * b as u128;
        match ceil_bps(a, b) {
            Some(f) => {
                let f = f as u128;
                prop_assert!(f * D >= n);
                prop_assert!(f * D < n + D);
                prop_assert_eq!(f, n / D + (!n.is_multiple_of(D)) as u128);
            }
            // INV-FEE-2: None exactly on overflow.
            None => prop_assert!(n > D * M),
        }
    }

    /// INV-FEE-2: `None ⇔ a·b > D·M`; rates up to 100% never overflow.
    #[test]
    fn prop_inv_fee_2_none_iff_overflow(a in amount(), b in bps64()) {
        let n = a as u128 * b as u128;
        prop_assert_eq!(ceil_bps(a, b).is_none(), n > D * M);
        if b <= 10_000 {
            prop_assert!(ceil_bps(a, b).is_some());
        }
    }

    /// INV-FEE-3: zero exactly when the amount or the rate is zero.
    #[test]
    fn prop_inv_fee_3_zero_iff(a in amount(), b in bps64()) {
        if let Some(f) = ceil_bps(a, b) {
            prop_assert_eq!(f == 0, a == 0 || b == 0);
        }
        prop_assert_eq!(pipeline_fee(a) == 0, a == 0);
    }

    /// INV-FEE-4: rates up to 100% never charge more than the amount.
    #[test]
    fn prop_inv_fee_4_le_amount(a in amount(), b in 0u64..=10_000) {
        prop_assert!(ceil_bps(a, b).unwrap() <= a);
        prop_assert_eq!(ceil_bps(a, 10_000), Some(a));
    }

    /// INV-FEE-5: monotone in amount and rate.
    #[test]
    fn prop_inv_fee_5_monotone(a1 in amount(), a2 in amount(), b1 in bps64(), b2 in bps64()) {
        let (lo, hi) = (a1.min(a2), a1.max(a2));
        let (blo, bhi) = (b1.min(b2), b1.max(b2));
        if let Some(f2) = ceil_bps(hi, blo) {
            prop_assert!(ceil_bps(lo, blo).unwrap() <= f2);
        }
        if let Some(f2) = ceil_bps(lo, bhi) {
            prop_assert!(ceil_bps(lo, blo).unwrap() <= f2);
        }
        prop_assert!(pipeline_fee(lo) <= pipeline_fee(hi));
    }

    /// INV-FEE-6 / INV-SWAP-8: `pipeline_fee(a) = ceil(a / 400)`, bounded, `≤ a`.
    #[test]
    fn prop_inv_fee_6_pipeline_fee(a in amount()) {
        let fee = pipeline_fee(a);
        prop_assert_eq!(fee as u128, (a as u128).div_ceil(400));
        prop_assert!(fee <= 46_116_860_184_273_880);
        prop_assert!(fee <= a);
        prop_assert_eq!(fee == a, a <= 1);
    }

    /// INV-FEE-7 / INV-SWAP-9: an explicit N-way split pays between `fee(A)` and
    /// `fee(A) + N − 1`, for `pipeline_fee` and for `ceil_bps` at rates up to 100%.
    #[test]
    fn prop_inv_fee_7_n_way_split(pieces in split(), b in 0u64..=10_000) {
        let n = pieces.len() as u128;
        let total: u64 = pieces.iter().sum();
        let whole_pf = pipeline_fee(total) as u128;
        let parts_pf: u128 = pieces.iter().map(|&p| pipeline_fee(p) as u128).sum();
        prop_assert!(whole_pf <= parts_pf);
        prop_assert!(parts_pf < whole_pf + n);
        let whole = ceil_bps(total, b).unwrap() as u128;
        let parts: u128 = pieces.iter().map(|&p| ceil_bps(p, b).unwrap() as u128).sum();
        prop_assert!(whole <= parts);
        prop_assert!(parts < whole + n);
    }

    /// INV-FEE-7: the attack case of H015: equal pieces of 1..=400 units.
    #[test]
    fn prop_inv_fee_7_dust_split(piece in 1u64..=400, count in 1usize..=1_000) {
        let pieces = vec![piece; count];
        let total: u64 = pieces.iter().sum();
        let parts: u128 = pieces.iter().map(|&p| pipeline_fee(p) as u128).sum();
        prop_assert!(parts >= pipeline_fee(total) as u128);
        // Every non-zero piece pays at least one unit.
        prop_assert!(parts >= count as u128);
    }

    /// INV-FEE-8: `max_total_spend` is the exact sum or None.
    #[test]
    fn prop_inv_fee_8_mts_exact(a in amount(), b in bps16()) {
        let a128 = a as u128;
        let s = a128 + (a128 * 25).div_ceil(D) + (a128 * b as u128).div_ceil(D);
        let expected = if s <= M { Some(s as u64) } else { None };
        prop_assert_eq!(max_total_spend(a, b), expected);
    }

    /// INV-FEE-9: never below the router docs' former single-ceiling formula, at most one above.
    #[test]
    fn prop_inv_fee_9_mts_vs_doc(a in amount(), b in bps16()) {
        if let Some(t) = max_total_spend(a, b) {
            let doc = a as u128 + (a as u128 * (25 + b as u128)).div_ceil(D);
            prop_assert!(t as u128 >= doc);
            prop_assert!(t as u128 <= doc + 1);
        }
    }

    /// INV-FEE-10: strictly increasing in amount, non-decreasing in rate.
    #[test]
    fn prop_inv_fee_10_mts_monotone(a in amount(), b1 in bps16(), b2 in bps16()) {
        if a < u64::MAX {
            if let Some(t1) = max_total_spend(a + 1, b1) {
                prop_assert!(t1 > max_total_spend(a, b1).unwrap());
            }
        }
        let (lo, hi) = (b1.min(b2), b1.max(b2));
        if let Some(t2) = max_total_spend(a, hi) {
            prop_assert!(max_total_spend(a, lo).unwrap() <= t2);
        }
    }

    /// INV-FEE-12: `amount_for_budget` never overspends.
    #[test]
    fn prop_inv_fee_12_afb_no_overspend(budget in amount(), b in bps16()) {
        let x = amount_for_budget(budget, b);
        prop_assert!(max_total_spend(x, b).unwrap() <= budget);
    }

    /// INV-FEE-13: `amount_for_budget` is maximal, and inverts `max_total_spend`.
    #[test]
    fn prop_inv_fee_13_afb_maximal_round_trip(budget in amount(), a in amount(), b in bps16()) {
        let x = amount_for_budget(budget, b);
        if x < u64::MAX {
            if let Some(t) = max_total_spend(x + 1, b) {
                prop_assert!(t > budget);
            }
        }
        if let Some(t) = max_total_spend(a, b) {
            prop_assert_eq!(amount_for_budget(t, b), a);
        }
    }

    /// INV-FEE-14: the result is the exact-rate estimate or one below it.
    #[test]
    fn prop_inv_fee_14_afb_near_estimate(budget in amount(), b in bps16()) {
        let rate = D + 25 + b as u128;
        let estimate = budget as u128 * D / rate;
        prop_assert!(estimate <= budget as u128);
        let x = amount_for_budget(budget, b) as u128;
        prop_assert!(x == estimate || x + 1 == estimate);
    }

    /// INV-FEE-15: a bigger budget never sends less; a higher rate never sends more.
    #[test]
    fn prop_inv_fee_15_afb_monotone(b1 in amount(), b2 in amount(), r1 in bps16(), r2 in bps16()) {
        let (lo, hi) = (b1.min(b2), b1.max(b2));
        prop_assert!(amount_for_budget(lo, r1) <= amount_for_budget(hi, r1));
        let (rlo, rhi) = (r1.min(r2), r1.max(r2));
        prop_assert!(amount_for_budget(hi, rlo) >= amount_for_budget(hi, rhi));
    }

    /// INV-FEE-15: neighbouring budgets (the slider case).
    #[test]
    fn prop_inv_fee_15_afb_monotone_neighbours(budget in amount(), b in bps16()) {
        if budget < u64::MAX {
            prop_assert!(amount_for_budget(budget, b) <= amount_for_budget(budget + 1, b));
        }
        if b < u16::MAX {
            prop_assert!(amount_for_budget(budget, b) >= amount_for_budget(budget, b + 1));
        }
    }

    /// INV-FEE-20: each hop's fee depends only on that hop's input and rate: the route
    /// total is the same in any order, with or without other hops, and repeat calls agree.
    #[test]
    fn prop_inv_fee_20_every_hop_pays(hops in prop::collection::vec((amount(), bps16()), 1..=6)) {
        let hop_fee = |(x, b): (u64, u16)| -> Option<u128> {
            Some(pipeline_fee(x) as u128 + ceil_bps(x, b as u64)? as u128)
        };
        let forward: Option<u128> = hops.iter().map(|&h| hop_fee(h)).sum();
        let reverse: Option<u128> = hops.iter().rev().map(|&h| hop_fee(h)).sum();
        prop_assert_eq!(forward, reverse);
        for &(x, b) in &hops {
            // Deterministic: no hidden state between calls.
            prop_assert_eq!(pipeline_fee(x), pipeline_fee(x));
            prop_assert_eq!(ceil_bps(x, b as u64), ceil_bps(x, b as u64));
            // Interleaving other hops does not change this hop's fee.
            let alone = hop_fee((x, b));
            for &other in &hops {
                let _ = hop_fee(other);
                prop_assert_eq!(hop_fee((x, b)), alone);
            }
            // Every hop with input pays.
            if x > 0 {
                prop_assert!(pipeline_fee(x) >= 1);
            }
        }
    }

    /// INV-FEE-16: never credits more than sent or arrived.
    #[test]
    fn prop_inv_fee_16_credit_bounds(v in amount(), r in amount(), s in amount()) {
        let c = credited_input(v, r, s);
        prop_assert_eq!(c, v.saturating_sub(r).min(s));
        prop_assert!(c <= s);
        if v <= r {
            prop_assert_eq!(c, 0);
        }
    }

    /// INV-FEE-17: a donation never raises the credit or its fee above what was sent.
    #[test]
    fn prop_inv_fee_17_donation_immunity(v in amount(), r in amount(), s in amount(), d in amount()) {
        if let Some(vd) = v.checked_add(d) {
            let c = credited_input(vd, r, s);
            prop_assert!(c <= s);
            prop_assert!(pipeline_fee(c) <= pipeline_fee(s));
        }
    }

    /// INV-FEE-18: exact on shortfalls; monotone in each argument.
    #[test]
    fn prop_inv_fee_18_exact_and_monotone(v1 in amount(), v2 in amount(), r1 in amount(), r2 in amount(), s1 in amount(), s2 in amount()) {
        let (v, vh) = (v1.min(v2), v1.max(v2));
        let (r, rh) = (r1.min(r2), r1.max(r2));
        let (s, sh) = (s1.min(s2), s1.max(s2));
        if r <= v && v - r <= s {
            prop_assert_eq!(credited_input(v, r, s), v - r);
        }
        prop_assert!(credited_input(v, r, s) <= credited_input(vh, r, s));
        prop_assert!(credited_input(v, r, s) <= credited_input(v, r, sh));
        prop_assert!(credited_input(v, r, s) >= credited_input(v, rh, s));
    }
}

/// INV-FEE-19 / INV-FEE-2 edges at the 65_535-bps overflow threshold and other fixed points.
#[test]
fn prop_inv_fee_19_threshold_edges() {
    assert_eq!((D * M) / 65_535, THRESHOLD_65535 as u128);
    assert!(ceil_bps(THRESHOLD_65535, 65_535).is_some());
    assert!(ceil_bps(THRESHOLD_65535 + 1, 65_535).is_none());
    assert_eq!(ceil_bps(u64::MAX, 10_000), Some(u64::MAX));
    assert!(ceil_bps(u64::MAX, 10_001).is_none());
    assert_eq!(ceil_bps(1, 10_001), Some(2));
    assert_eq!(pipeline_fee(u64::MAX), 46_116_860_184_273_880);
    // INV-FEE-9 witness: code 3, doc 2.
    assert_eq!(max_total_spend(1, 1), Some(3));
    assert_eq!(max_total_spend(u64::MAX, 0), None);
    assert_eq!(amount_for_budget(0, 65_535), 0);
}
