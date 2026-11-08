//! SOS H014: playground constant-product pricing.
//!
//! Part 1 (pure Rust): `out_given_in`, `in_given_out` and `mul_div` are copied verbatim
//! from `programs/test-strategy-cp/src/lib.rs` lines 415-474 (the test crate cannot
//! import the program) and checked against an independent exact 256-bit reference built
//! from u32 limbs (schoolbook multiply and compare, no division, so it shares no code
//! with the long division under test).
//!
//! Part 2 (LiteSVM): round trips through the real router at extreme reserve ratios, with
//! and without Token-2022 transfer fees, checking after every swap that the constant
//! product never falls and recorded reserves never exceed the vaults.
//!
//! Tests that confirm a defence holds pass.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, Pubkey};
use pipeline_strategy_interface::{fees, TradeMode};
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const BPS: u128 = 10_000;

// ------------------------------------------------------------------------------------
// Verbatim copy of programs/test-strategy-cp/src/lib.rs:415-474
// ------------------------------------------------------------------------------------
/// `floor(r_out × in × (1 − fee) / (r_in + in × (1 − fee)))`.
pub fn out_given_in(reserve_in: u64, reserve_out: u64, amount_in: u64, fee_bps: u16) -> u64 {
    let in_after_fee = amount_in as u128 * (BPS - fee_bps as u128);
    let denominator = reserve_in as u128 * BPS + in_after_fee;
    if denominator == 0 {
        return 0;
    }
    // At most `reserve_out`, so it fits in a u64.
    mul_div(reserve_out as u128, in_after_fee, denominator).0 as u64
}

/// The least input for which [`out_given_in`] returns at least `amount_out`, or `None`
/// if the pool cannot provide it.
pub fn in_given_out(
    reserve_in: u64,
    reserve_out: u64,
    amount_out: u64,
    fee_bps: u16,
) -> Option<u64> {
    if amount_out == 0 {
        return Some(0);
    }
    if amount_out >= reserve_out {
        return None;
    }
    let denominator = (reserve_out - amount_out) as u128 * (BPS - fee_bps as u128);
    let (quotient, remainder) = mul_div(reserve_in as u128 * BPS, amount_out as u128, denominator);
    let rounded_up = quotient.checked_add((remainder > 0) as u128)?;
    u64::try_from(rounded_up).ok()
}

/// `a × b / d` as (quotient, remainder), with a 256-bit product so large pools and
/// trades can't overflow. The quotient saturates at `u128::MAX`; `d` must not be 0.
fn mul_div(a: u128, b: u128, d: u128) -> (u128, u128) {
    let mask = u64::MAX as u128;
    let (a1, a0) = (a >> 64, a & mask);
    let (b1, b0) = (b >> 64, b & mask);
    let (mid, mid_carry) = (a1 * b0).overflowing_add(a0 * b1);
    let (lo, lo_carry) = (a0 * b0).overflowing_add(mid << 64);
    let hi = a1 * b1 + (mid >> 64) + ((mid_carry as u128) << 64) + lo_carry as u128;
    if hi == 0 {
        return (lo / d, lo % d);
    }
    if hi >= d {
        return (u128::MAX, 0);
    }
    // Long division of (hi, lo) by d. The running remainder stays below 2d, so a carry
    // bit plus a wrapping subtraction keeps it exact.
    let mut remainder = hi;
    let mut quotient = 0u128;
    for bit in (0..128).rev() {
        let carry = remainder >> 127;
        remainder = (remainder << 1) | ((lo >> bit) & 1);
        if carry == 1 || remainder >= d {
            remainder = remainder.wrapping_sub(d);
            quotient |= 1 << bit;
        }
    }
    (quotient, remainder)
}

// ------------------------------------------------------------------------------------
// Exact reference arithmetic: little-endian u32 limbs
// ------------------------------------------------------------------------------------

type Big = Vec<u32>;

fn big(x: u128) -> Big {
    (0..4).map(|i| (x >> (32 * i)) as u32).collect()
}

fn mul(a: &Big, b: &Big) -> Big {
    let mut out = vec![0u32; a.len() + b.len() + 1];
    for (i, &x) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, &y) in b.iter().enumerate() {
            let cur = out[i + j] as u64 + x as u64 * y as u64 + carry;
            out[i + j] = cur as u32;
            carry = cur >> 32;
        }
        let mut k = i + b.len();
        while carry > 0 {
            let cur = out[k] as u64 + carry;
            out[k] = cur as u32;
            carry = cur >> 32;
            k += 1;
        }
    }
    out
}

fn add(a: &Big, b: &Big) -> Big {
    let n = a.len().max(b.len()) + 1;
    let mut out = vec![0u32; n];
    let mut carry = 0u64;
    for (i, digit) in out.iter_mut().enumerate() {
        let cur = *a.get(i).unwrap_or(&0) as u64 + *b.get(i).unwrap_or(&0) as u64 + carry;
        *digit = cur as u32;
        carry = cur >> 32;
    }
    out
}

fn cmp(a: &Big, b: &Big) -> std::cmp::Ordering {
    let n = a.len().max(b.len());
    for i in (0..n).rev() {
        let (x, y) = (*a.get(i).unwrap_or(&0), *b.get(i).unwrap_or(&0));
        if x != y {
            return x.cmp(&y);
        }
    }
    std::cmp::Ordering::Equal
}

fn shl128(a: &Big) -> Big {
    let mut out = vec![0u32; 4];
    out.extend_from_slice(a);
    out
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Roughly log-uniform in `[1, hi]`, biased to include the extremes.
    fn log(&mut self, hi: u64) -> u64 {
        match self.next() % 16 {
            0 => return 1,
            1 => return hi,
            2 => return hi.saturating_sub(1).max(1),
            _ => {}
        }
        let bits = 1 + self.next() % 64;
        let top = if bits >= 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        let v = self.next() & top;
        v.clamp(1, hi.max(1))
    }

    fn fee(&mut self) -> u16 {
        match self.next() % 8 {
            0 => 0,
            1 => 1,
            2 => 9_999,
            3 => 30,
            4 => 9_998,
            _ => (self.next() % 10_000) as u16,
        }
    }
}

// ------------------------------------------------------------------------------------
// Part 1: pure maths
// ------------------------------------------------------------------------------------

/// mul_div returns the exact quotient and remainder (q*d + r == a*b, r < d), or
/// saturates only when the true quotient is at least 2^128.
#[test]
fn mul_div_is_exact_and_saturates_only_when_quotient_overflows() {
    let mut rng = Rng::new(1);
    let mut saturated = 0u64;
    let mut long_division = 0u64;
    for _ in 0..300_000 {
        // Operands of widely varying size, including near-2^128.
        let wide = |rng: &mut Rng| -> u128 {
            let bits = 1 + rng.next() % 128;
            let v = ((rng.next() as u128) << 64) | rng.next() as u128;
            let v = if bits == 128 {
                v
            } else {
                v & ((1u128 << bits) - 1)
            };
            match rng.next() % 12 {
                0 => u128::MAX,
                1 => u128::MAX - 1,
                2 => 1 << 64,
                3 => (1 << 64) - 1,
                _ => v,
            }
        };
        let (a, b) = (wide(&mut rng), wide(&mut rng));
        let d = wide(&mut rng).max(1);
        let (q, r) = mul_div(a, b, d);
        let ab = mul(&big(a), &big(b));
        let overflow = cmp(&ab, &shl128(&big(d))) != std::cmp::Ordering::Less;
        if overflow {
            saturated += 1;
            assert_eq!((q, r), (u128::MAX, 0), "saturation value for {a} {b} {d}");
        } else {
            assert!(r < d, "remainder {r} >= divisor {d} for {a} {b}");
            let rebuilt = add(&mul(&big(q), &big(d)), &big(r));
            assert_eq!(
                cmp(&rebuilt, &ab),
                std::cmp::Ordering::Equal,
                "q*d+r != a*b for a={a} b={b} d={d} q={q} r={r}"
            );
            if (a >> 64) * (b >> 64) != 0 || ((a >> 64) * (b & u64::MAX as u128)) >> 64 != 0 {
                long_division += 1;
            }
        }
    }
    println!("mul_div: 300000 cases, {saturated} saturating, ~{long_division} 256-bit");
    assert!(saturated > 1_000 && long_division > 1_000);
}

/// out_given_in = floor(N/D) exactly, never exceeds reserve_out, never reaches it, and
/// the constant product (including the fee kept in the pool) never falls.
#[test]
fn out_given_in_floors_exactly_and_k_never_falls() {
    let mut rng = Rng::new(2);
    let mut nonzero = 0u64;
    for _ in 0..400_000 {
        let rin = rng.log(u64::MAX);
        let rout = rng.log(u64::MAX);
        let x = rng.log(u64::MAX);
        let fee = rng.fee();
        let out = out_given_in(rin, rout, x, fee);
        assert!(out < rout, "output {out} reaches reserve {rout}");
        // N = rout * x * (B - fee); D = rin * B + x * (B - fee)
        let xf = big(x as u128 * (BPS - fee as u128));
        let n = mul(&big(rout as u128), &xf);
        let d = add(&big(rin as u128 * BPS), &xf);
        let lo = mul(&big(out as u128), &d);
        let hi = mul(&big(out as u128 + 1), &d);
        assert!(
            cmp(&lo, &n).is_le(),
            "out too large: {rin} {rout} {x} {fee}"
        );
        assert!(
            cmp(&n, &hi).is_lt(),
            "out not the floor: {rin} {rout} {x} {fee}"
        );
        // k: (rin + x) * (rout - out) >= rin * rout
        let k_before = mul(&big(rin as u128), &big(rout as u128));
        let k_after = mul(&big(rin as u128 + x as u128), &big((rout - out) as u128));
        assert!(
            cmp(&k_after, &k_before).is_ge(),
            "k fell on ExactIn: {rin} {rout} {x} {fee} -> {out}"
        );
        if out > 0 {
            nonzero += 1;
        }
    }
    println!("out_given_in: 400000 cases ok, {nonzero} non-zero outputs");
    assert!(nonzero > 10_000);
}

/// in_given_out is the least input whose out_given_in covers the request: exact ceil,
/// `None` exactly when the pool cannot provide it or the input would not fit a u64, and
/// the constant product never falls.
#[test]
fn in_given_out_ceils_exactly_and_k_never_falls() {
    let mut rng = Rng::new(3);
    let (mut some, mut none_liquidity, mut none_overflow) = (0u64, 0u64, 0u64);
    for _ in 0..400_000 {
        let rin = rng.log(u64::MAX);
        let rout = rng.log(u64::MAX);
        let y = match rng.next() % 6 {
            0 => rout,
            1 => rout.saturating_sub(1),
            2 => 0,
            _ => rng.log(rout),
        };
        let fee = rng.fee();
        let got = in_given_out(rin, rout, y, fee);
        if y == 0 {
            assert_eq!(got, Some(0));
            continue;
        }
        if y >= rout {
            assert_eq!(got, None, "must refuse draining the reserve");
            none_liquidity += 1;
            continue;
        }
        // N = rin * B * y ; D = (rout - y) * (B - fee); want least x with x*D >= N.
        let n = mul(&big(rin as u128 * BPS), &big(y as u128));
        let d = big((rout - y) as u128 * (BPS - fee as u128));
        match got {
            Some(x) => {
                some += 1;
                assert!(
                    cmp(&mul(&big(x as u128), &d), &n).is_ge(),
                    "under-charge: {rin} {rout} {y} {fee} -> {x}"
                );
                if x > 0 {
                    assert!(
                        cmp(&mul(&big(x as u128 - 1), &d), &n).is_lt(),
                        "not the least input: {rin} {rout} {y} {fee} -> {x}"
                    );
                }
                // The pool pays exactly y for x, so x really does cover y.
                assert!(out_given_in(rin, rout, x, fee) >= y);
                if x > 0 {
                    assert!(out_given_in(rin, rout, x - 1, fee) < y);
                }
                // k never falls on ExactOut.
                let k_before = mul(&big(rin as u128), &big(rout as u128));
                let k_after = mul(&big(rin as u128 + x as u128), &big((rout - y) as u128));
                assert!(
                    cmp(&k_after, &k_before).is_ge(),
                    "k fell on ExactOut: {rin} {rout} {y} {fee} -> {x}"
                );
            }
            None => {
                none_overflow += 1;
                // The true ceiling must exceed u64::MAX.
                assert!(
                    cmp(&mul(&big(u64::MAX as u128), &d), &n).is_lt(),
                    "None although a u64 input suffices: {rin} {rout} {y} {fee}"
                );
            }
        }
    }
    println!(
        "in_given_out: some={some} none(liquidity)={none_liquidity} none(u64 overflow)={none_overflow}"
    );
    assert!(some > 10_000 && none_overflow > 100);
}

/// The only way `mul_div` saturates is in `in_given_out`; the saturated quotient never
/// reaches an `as u64` cast. Pin the boundary cases by hand.
#[test]
fn saturation_boundaries_are_refused_not_wrapped() {
    // d = 1: quotient = rin*B*y, hugely above u64, must be None.
    assert_eq!(in_given_out(u64::MAX, u64::MAX, u64::MAX - 1, 9_999), None);
    // quotient fits u64 exactly at the boundary region: rin = 1, B*y/(rout-y)(B-f)
    assert!(in_given_out(1, 2, 1, 0).is_some());
    // amount_out == reserve_out refused, reserve_out == 0 refused.
    assert_eq!(in_given_out(5, 7, 7, 30), None);
    assert_eq!(in_given_out(5, 0, 1, 30), None);
    // out_given_in with reserve_in = 0 and amount 0: denominator zero returns 0 (and the
    // strategy rejects one-sided reserves before calling it).
    assert_eq!(out_given_in(0, 100, 0, 30), 0);
    // Reserve_in = 0, amount > 0, fee < 100%: whole reserve minus nothing: floor(r*in*f/(in*f)) = r.
    // (Unreachable: `Pool::quote` requires reserve_in > 0.)
    println!(
        "out_given_in(0, 100, 5, 0) = {} (reserve_in=0 is blocked by Pool::quote)",
        out_given_in(0, 100, 5, 0)
    );
    // Max inputs do not overflow the u128 intermediates.
    let _ = out_given_in(u64::MAX, u64::MAX, u64::MAX, 0);
    let _ = in_given_out(u64::MAX, u64::MAX, u64::MAX - 1, 0);
}

// ------------------------------------------------------------------------------------
// Part 2: LiteSVM round trips through the router
// ------------------------------------------------------------------------------------

fn cp_reserves(env: &Env, pool: &Pubkey) -> (u64, u64) {
    let data = env.account_data(pool);
    let read = |offset: usize| u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
    (read(136), read(144))
}

struct Harness {
    env: Env,
    market: CpMarket,
    trader: solana_keypair::Keypair,
    ta: Pubkey,
    tb: Pubkey,
}

impl Harness {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ra: u64,
        rb: u64,
        fee_bps: u16,
        transfer_fees: Option<((u16, u64), (u16, u64))>,
        trader_a: u64,
        trader_b: u64,
    ) -> Self {
        let mut env = Env::new();
        let (tp, fa, fb) = match transfer_fees {
            Some((a, b)) => (TOKEN_2022, Some(a), Some(b)),
            None => (TOKEN, None, None),
        };
        let mint_a = env.create_mint(&tp, 6, fa, None);
        let mint_b = env.create_mint(&tp, 9, fb, None);
        let market =
            setup_cp_market_with_mints(&mut env, mint_a, tp, mint_b, tp, ra, rb, fee_bps, vec![]);
        let trader = env.funded_keypair();
        let ta = env.create_ata(&trader.pubkey(), &mint_a, &tp);
        let tb = env.create_ata(&trader.pubkey(), &mint_b, &tp);
        env.mint_to(&mint_a, &tp, &ta, trader_a);
        env.mint_to(&mint_b, &tp, &tb, trader_b);
        Harness {
            env,
            market,
            trader,
            ta,
            tb,
        }
    }

    fn reserves(&self) -> (u64, u64) {
        cp_reserves(&self.env, &self.market.pool)
    }

    fn check_invariants(&self, before: (u64, u64), label: &str) {
        let (ra, rb) = self.reserves();
        let k_before = before.0 as u128 * before.1 as u128;
        let k_after = ra as u128 * rb as u128;
        println!("  {label}: reserves {before:?} -> ({ra}, {rb})");
        assert!(
            k_after >= k_before,
            "{label}: k fell {k_before} -> {k_after}"
        );
        assert!(
            ra <= self.env.token_balance(&self.market.vault_a)
                && rb <= self.env.token_balance(&self.market.vault_b),
            "{label}: reserves ({ra}, {rb}) exceed vaults"
        );
        assert!(ra > 0 && rb > 0, "{label}: a reserve was emptied");
    }

    /// Returns (spent, received) or the failure text.
    fn swap(
        &mut self,
        a_to_b: bool,
        mode: TradeMode,
        amount: u64,
        max_in: u64,
    ) -> Result<(u64, u64), String> {
        let m = &self.market;
        let (im, om, ti, to) = if a_to_b {
            (m.mint_a, m.mint_b, self.ta, self.tb)
        } else {
            (m.mint_b, m.mint_a, self.tb, self.ta)
        };
        let accounts = cp_swap_accounts(m, &self.trader.pubkey(), &im, &ti, &om, &to);
        let args = match mode {
            TradeMode::ExactIn => exact_in(amount, 0),
            TradeMode::ExactOut => client::SwapArgs {
                mode,
                amount_in: max_in,
                amount_out: amount,
                min_amount_out: amount,
                integrator_fee_bps: 0,
                input_hook_accounts: 0,
                extra: vec![],
            },
        };
        let (i0, o0) = (self.env.token_balance(&ti), self.env.token_balance(&to));
        let ix = client::swap(&accounts, &args);
        let r = self.env.send(&[ix], &[&self.trader]);
        match r {
            Ok(_) => Ok((
                i0 - self.env.token_balance(&ti),
                self.env.token_balance(&to) - o0,
            )),
            Err(f) => Err(format!("{:?}: {}", f.err, f.meta.logs.join(" | "))),
        }
    }
}

/// reserve_in tiny, reserve_out huge (and the reverse), SPL tokens: quotes match the
/// verbatim maths, k never falls, reserves stay within the vaults.
#[test]
fn extreme_ratio_round_trips_through_the_router() {
    for fee in [0u16, 30, 9_999] {
        // A tiny, B huge.
        let mut h = Harness::new(
            1,
            4_000_000_000_000_000_000,
            fee,
            None,
            1_000_000_000,
            8 * 10u64.pow(18),
        );
        println!("fee {fee} bps: A=1, B=4e18");
        // A -> B ExactIn 1
        let before = h.reserves();
        let (rin, rout) = before;
        let expected = out_given_in(rin, rout, 1, fee);
        let (spent, got) = h.swap(true, TradeMode::ExactIn, 1, 0).expect("swap 1");
        assert_eq!(got, expected);
        println!("  A->B exact-in 1: spent {spent} got {got}");
        h.check_invariants(before, "a->b in 1");

        // B -> A ExactIn large: output of the tiny side rounds down.
        let before = h.reserves();
        let amt = 1_000_000_000_000_000_000u64;
        let expected = out_given_in(before.1, before.0, amt, fee);
        let (_, got) = h.swap(false, TradeMode::ExactIn, amt, 0).expect("swap 2");
        assert_eq!(got, expected);
        h.check_invariants(before, "b->a in 1e18");

        // B -> A ExactOut: ask for (reserve_a - 1) of the tiny side; may be unaffordable.
        let before = h.reserves();
        let want = before.0.saturating_sub(1);
        if want > 0 {
            let need = in_given_out(before.1, before.0, want, fee);
            println!("  b->a exact-out {want}: need {need:?}");
            if let Some(need) = need {
                if need < 3_000_000_000_000_000_000 {
                    let (spent, got) = h
                        .swap(false, TradeMode::ExactOut, want, need + need / 100 + 10)
                        .expect("exact-out");
                    assert_eq!(got, want);
                    assert_eq!(spent, need + fees::pipeline_fee(need));
                    h.check_invariants(before, "b->a out all-but-one");
                }
            }
        }
        // Asking for the whole reserve is refused.
        let before = h.reserves();
        let e = h.swap(true, TradeMode::ExactOut, before.1, 2).unwrap_err();
        assert!(
            e.contains("InsufficientLiquidity") || e.contains("Liquidity"),
            "{e}"
        );
        h.check_invariants(before, "refused drain");
    }
}

/// reserve_in huge, reserve_out tiny: exact-out of the single unit is refused, exact-in
/// pays input and returns zero without lowering k.
#[test]
fn huge_input_tiny_output_pool() {
    for fee in [0u16, 30, 9_999] {
        let mut h = Harness::new(
            4_000_000_000_000_000_000,
            1,
            fee,
            None,
            4 * 10u64.pow(18),
            10,
        );
        println!("fee {fee}: A=4e18, B=1");
        let before = h.reserves();
        // Exact-out of the only unit must be refused.
        let e = h
            .swap(true, TradeMode::ExactOut, 1, 3_000_000_000_000_000_000)
            .unwrap_err();
        assert!(e.contains("Liquidity"), "{e}");
        h.check_invariants(before, "exact-out whole reserve");
        // Exact-in of a huge amount: output stays 0 since out < reserve_out = 1.
        let (spent, got) = h
            .swap(true, TradeMode::ExactIn, 1_000_000_000_000_000_000, 0)
            .expect("exact-in");
        println!("  a->b exact-in 1e18: spent {spent} got {got}");
        assert_eq!(got, 0);
        h.check_invariants(before, "huge in");
        // B -> A with B=1 reserve and a small input.
        let before = h.reserves();
        let (_, got) = h.swap(false, TradeMode::ExactIn, 5, 0).expect("b->a");
        assert_eq!(got, out_given_in(before.1, before.0, 5, fee));
        h.check_invariants(before, "b->a 5");
    }
}

/// Both reserves near the top of the u64 range: the 256-bit paths run on chain, the
/// product never falls, and an input that would overflow the reserve fails cleanly.
#[test]
fn near_u64_max_reserves() {
    let big_r = 4_000_000_000_000_000_000u64;
    let mut h = Harness::new(big_r, big_r, 30, None, 4 * 10u64.pow(18), 4 * 10u64.pow(18));
    let before = h.reserves();
    let x = 2_000_000_000_000_000_000u64;
    let exp = out_given_in(before.0, before.1, x, 30);
    let (_, got) = h.swap(true, TradeMode::ExactIn, x, 0).expect("big swap");
    assert_eq!(got, exp);
    h.check_invariants(before, "huge a->b");
    let before = h.reserves();
    let want = before.0 / 2;
    let need = in_given_out(before.1, before.0, want, 30).unwrap();
    let (spent, got) = h
        .swap(false, TradeMode::ExactOut, want, need + need / 100 + 10)
        .expect("big exact-out");
    assert_eq!((got, spent), (want, need + fees::pipeline_fee(need)));
    h.check_invariants(before, "huge b->a exact-out");
    // Pushing reserve_a past u64::MAX must not wrap or succeed.
    let before = h.reserves();
    let r = h.swap(true, TradeMode::ExactIn, u64::MAX / 2, 0);
    println!(
        "  overflowing exact-in: {:?}",
        r.as_ref().map(|_| ()).map_err(|e| &e[..e.len().min(120)])
    );
    h.check_invariants(before, "after overflow attempt");
}

/// Both mints charge Token-2022 transfer fees; random swaps in both directions and modes,
/// including a 100% fee mint with a max-fee cap, never lower k or exceed the vaults.
#[test]
fn transfer_fee_mints_keep_reserves_within_vaults() {
    for (fees_cfg, ra, rb) in [
        (
            ((150u16, u64::MAX), (37u16, 5_000u64)),
            5_000_000_000u64,
            7_000_000_000u64,
        ),
        (
            ((10_000u16, u64::MAX), (9_999u16, u64::MAX)),
            1_000_000_000u64,
            1_000_000_000u64,
        ),
        (((500u16, 1u64), (1u16, u64::MAX)), 1u64 << 40, 3),
    ] {
        let mut h = Harness::new(ra, rb, 30, Some(fees_cfg), 1u64 << 60, 1u64 << 60);
        println!("transfer fees {fees_cfg:?}, reserves ({ra},{rb})");
        let mut rng = Rng::new(ra ^ rb);
        let mut ok = 0;
        for step in 0..40 {
            let before = h.reserves();
            let a_to_b = rng.next().is_multiple_of(2);
            let (rin, rout) = if a_to_b { before } else { (before.1, before.0) };
            let exact_in = rng.next().is_multiple_of(2);
            let result = if exact_in {
                let x = rng.log(rin.saturating_mul(2).max(1));
                h.swap(a_to_b, TradeMode::ExactIn, x, 0)
            } else {
                let y = rng.log(rout.saturating_sub(1).max(1));
                // Generous max: the strategy refunds the unused part.
                let max_in = in_given_out(rin, rout, y, 30)
                    .map(|n| n.saturating_mul(3).saturating_add(1_000))
                    .unwrap_or(1_000);
                h.swap(a_to_b, TradeMode::ExactOut, y, max_in)
            };
            match result {
                Ok(_) => {
                    ok += 1;
                    h.check_invariants(
                        before,
                        &format!("step {step} a_to_b={a_to_b} in={exact_in}"),
                    );
                }
                Err(e) => {
                    // A failed swap must leave state untouched.
                    assert_eq!(h.reserves(), before);
                    if step < 3 {
                        println!("  step {step} refused: {}", &e[..e.len().min(160)]);
                    }
                }
            }
        }
        println!("  {ok}/40 swaps executed");
    }
}
