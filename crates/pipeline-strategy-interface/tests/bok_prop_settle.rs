//! Book of Knowledge property tests for `settlement::{settle, hook_split}`.
//! Invariants: `.bok/confirmed-invariants/01-settle.md` and INV-SWAP-16 from
//! `04-swap-handler.md`. Fast random layer alongside the Kani proofs in `src/bok_kani/`.

use pipeline_strategy_interface::fees::{ceil_bps, max_total_spend, pipeline_fee};
use pipeline_strategy_interface::settlement::{
    hook_split, settle, Measured, SettleError, Settlement,
};
use proptest::prelude::*;

const D: u128 = 10_000;
const THRESHOLD_65535: u64 = 2_814_792_717_434_890_000;

/// u64 values weighted towards edges.
fn amount() -> impl Strategy<Value = u64> {
    prop_oneof![
        2 => Just(0u64),
        1 => Just(1u64),
        1 => Just(u64::MAX),
        1 => Just(THRESHOLD_65535),
        1 => Just(THRESHOLD_65535 + 1),
        2 => (u64::MAX - 1_000)..=u64::MAX,
        3 => 0u64..1_000,
        3 => 0u64..(1u64 << 40),
        5 => any::<u64>(),
    ]
}

fn bps() -> impl Strategy<Value = u16> {
    prop_oneof![
        2 => Just(0u16),
        1 => Just(1u16),
        1 => Just(100u16),
        1 => Just(10_000u16),
        1 => Just(10_001u16),
        1 => Just(65_535u16),
        4 => any::<u16>(),
    ]
}

/// Arbitrary independent before/after balances.
fn measured() -> impl Strategy<Value = Measured> {
    (amount(), amount(), amount(), amount(), amount(), amount()).prop_map(
        |(input_before, input_after, output_before, output_after, vault_before, vault_after)| {
            Measured {
                input_before,
                input_after,
                output_before,
                output_after,
                vault_before,
                vault_after,
            }
        },
    )
}

/// Balances built from deltas, so successful swaps are common: the trader spends
/// `spent`, the vault keeps `kept`, the trader receives `recv`; `amount_in` and
/// `min_out` are drawn around them.
fn plausible() -> impl Strategy<Value = (Measured, u64, u64, u16)> {
    (
        amount(),
        amount(),
        amount(),
        amount(),
        amount(),
        amount(),
        0u64..3,
        0u64..3,
        bps(),
    )
        .prop_map(|(ib, ob, vb, spent, kept, recv, slack_in, slack_out, b)| {
            let spent = spent.min(ib);
            let recv = recv.min(u64::MAX - ob);
            let kept = kept.min(u64::MAX - vb);
            let m = Measured {
                input_before: ib,
                input_after: ib - spent,
                output_before: ob,
                output_after: ob + recv,
                vault_before: vb,
                vault_after: vb + kept,
            };
            // amount_in near spent/kept, sometimes one below (over-spend).
            let base = spent.max(kept);
            let amount_in = match slack_in {
                0 => base,
                1 => base.saturating_add(slack_out),
                _ => base.saturating_sub(1),
            };
            let min_out = match slack_out {
                0 => recv,
                1 => recv.saturating_add(1),
                _ => recv / 2,
            };
            (m, amount_in, min_out, b)
        })
}

fn deltas(m: &Measured) -> (u64, u64, u64) {
    (
        m.input_before.saturating_sub(m.input_after),
        m.output_after.saturating_sub(m.output_before),
        m.vault_after.saturating_sub(m.vault_before),
    )
}

/// Reference model of `settle` from the confirmed invariants (01-settle.md).
fn model(m: &Measured, amount_in: u64, min_out: u64, b: u16) -> Result<Settlement, SettleError> {
    let (spent, recv, kept) = deltas(m);
    if recv < min_out {
        return Err(SettleError::InsufficientOutput);
    }
    if spent > amount_in {
        return Err(SettleError::SpendLimitExceeded);
    }
    let net_sent = spent.max(kept).min(amount_in);
    if net_sent == 0 {
        return Err(if recv > 0 {
            SettleError::OutputWithoutInput
        } else {
            SettleError::NothingSwapped
        });
    }
    let n = net_sent as u128;
    let pf = (n * 25).div_ceil(D) as u64;
    let integ = (n * b as u128).div_ceil(D);
    let integrator_fee = u64::try_from(integ).map_err(|_| SettleError::MathOverflow)?;
    Ok(Settlement {
        trader_spent: spent,
        received: recv,
        vault_kept: kept,
        net_sent,
        pipeline_fee: pf,
        integrator_fee,
    })
}

/// Every settle property on one input.
fn check_all(m: &Measured, amount_in: u64, min_out: u64, b: u16) -> Result<(), TestCaseError> {
    let (spent, recv, kept) = deltas(m);
    let result = settle(m, amount_in, min_out, b);
    let base = spent.max(kept).min(amount_in);

    // INV-SWAP-20: total (a panic would abort the test); MathOverflow only above 100%.
    if result == Err(SettleError::MathOverflow) {
        prop_assert!(b > 10_000);
    }
    // INV-SWAP-6: the output check runs first.
    if recv < min_out {
        prop_assert_eq!(result, Err(SettleError::InsufficientOutput));
    }
    // INV-SWAP-4: over-spend is refused.
    if spent > amount_in && recv >= min_out {
        prop_assert_eq!(result, Err(SettleError::SpendLimitExceeded));
    }
    // INV-SWAP-7 / INV-SWAP-19: zero fee base is refused.
    if base == 0 && recv >= min_out && spent <= amount_in {
        let expected = if recv > 0 {
            SettleError::OutputWithoutInput
        } else {
            SettleError::NothingSwapped
        };
        prop_assert_eq!(result, Err(expected));
    }
    // INV-FEE-19 / INV-SWAP-10: MathOverflow exactly when the checks pass and the
    // integrator fee overflows.
    let checks_pass = recv >= min_out && spent <= amount_in && base > 0;
    prop_assert_eq!(
        result == Err(SettleError::MathOverflow),
        checks_pass && ceil_bps(base, b as u64).is_none()
    );

    if let Ok(s) = result {
        // INV-SWAP-1
        prop_assert!(s.net_sent <= amount_in);
        // INV-SWAP-2
        prop_assert_eq!(s.trader_spent, spent);
        prop_assert!(s.net_sent >= s.trader_spent);
        // INV-SWAP-3
        prop_assert_eq!(s.vault_kept, kept);
        prop_assert!(s.net_sent >= kept.min(amount_in));
        prop_assert_eq!(s.net_sent, base);
        // INV-SWAP-4
        prop_assert!(spent <= amount_in);
        // INV-SWAP-6
        prop_assert_eq!(s.received, recv);
        prop_assert!(s.received >= min_out);
        // INV-SWAP-7 / INV-SWAP-19
        prop_assert!(s.net_sent >= 1);
        prop_assert!(s.pipeline_fee >= 1);
        // INV-FEE-19 / INV-SWAP-10
        prop_assert_eq!(Some(s.integrator_fee), ceil_bps(s.net_sent, b as u64));
        prop_assert_eq!(s.pipeline_fee, pipeline_fee(s.net_sent));
        if b <= 10_000 {
            prop_assert!(s.integrator_fee <= s.net_sent);
        }
        // INV-FEE-11 / INV-SWAP-5
        let lhs = s.trader_spent as u128 + s.pipeline_fee as u128 + s.integrator_fee as u128;
        let a = amount_in as u128;
        let rhs = a + (25 * a).div_ceil(D) + (b as u128 * a).div_ceil(D);
        prop_assert!(lhs <= rhs);
        if let Some(mts) = max_total_spend(amount_in, b) {
            prop_assert!(lhs <= mts as u128);
        }
        // INV-SWAP-15: transfer-fee input (vault kept no more than the trader spent).
        if kept <= spent {
            prop_assert_eq!(s.net_sent, spent);
        }
    }
    // The whole result agrees with the reference model.
    prop_assert_eq!(result, model(m, amount_in, min_out, b));
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// INV-SWAP-1..7, 15, 19, 20, INV-FEE-11/SWAP-5, INV-FEE-19/SWAP-10 on arbitrary
    /// balances.
    #[test]
    fn prop_inv_swap_all_arbitrary_balances(m in measured(), amount_in in amount(), min_out in amount(), b in bps()) {
        check_all(&m, amount_in, min_out, b)?;
    }

    /// The same properties on balances built from plausible deltas (mostly successes).
    #[test]
    fn prop_inv_swap_all_plausible((m, amount_in, min_out, b) in plausible()) {
        check_all(&m, amount_in, min_out, b)?;
    }

    /// INV-SWAP-20: totality and the MathOverflow implication, arbitrary inputs.
    #[test]
    fn prop_inv_swap_20_total(m in measured(), amount_in in amount(), min_out in amount(), b in bps()) {
        if let Err(SettleError::MathOverflow) = settle(&m, amount_in, min_out, b) {
            prop_assert!(b > 10_000);
        }
    }

    /// INV-SWAP-6: InsufficientOutput precedes every other error.
    #[test]
    fn prop_inv_swap_6_min_out_first(m in measured(), amount_in in amount(), extra in 1u64..1_000, b in bps()) {
        let (_, recv, _) = deltas(&m);
        let min_out = recv.saturating_add(extra);
        if min_out > recv {
            prop_assert_eq!(settle(&m, amount_in, min_out, b), Err(SettleError::InsufficientOutput));
        }
    }

    /// INV-SWAP-3: a hook topping up `trader_input` during the call cannot shrink the fee
    /// base below what the vault kept.
    #[test]
    fn prop_inv_swap_3_refill_does_not_shrink_base(ib in 0u64..(1 << 60), vb in 0u64..(1 << 60), amount_in in 1u64..(1 << 40), topup in 0u64..(1 << 41), b in bps()) {
        prop_assume!(ib >= amount_in);
        let m = Measured {
            input_before: ib,
            input_after: ib - amount_in + topup,
            output_before: 0,
            output_after: 1,
            vault_before: vb,
            vault_after: vb + amount_in,
        };
        let s = settle(&m, amount_in, 1, b).unwrap();
        prop_assert_eq!(s.net_sent, amount_in);
        prop_assert_eq!(s.trader_spent, amount_in.saturating_sub(topup));
    }

    /// INV-SWAP-14: exact-out refund model. The trader sends `amount_in`, the strategy
    /// refunds `r` from the vault to `trader_input` (no transfer fee, no top-up):
    /// `spent = kept = amount_in − r`, so the base is `amount_in − r`; a full refund is
    /// refused (nothing kept), and the router checks `received ≥ min_amount_out` only.
    #[test]
    fn prop_inv_swap_14_exact_out_refund(ib in amount(), vb in amount(), ob in amount(), amount_in in amount(), r in amount(), recv in amount(), amount_out in amount(), b in bps()) {
        // Clamp instead of rejecting: the trader holds amount_in, nothing overflows.
        let ib = ib.max(amount_in);
        let vb = vb.min(u64::MAX - amount_in);
        let ob = ob.min(u64::MAX - recv);
        let r = r.min(amount_in);
        let m = Measured {
            input_before: ib,
            input_after: ib - amount_in + r,
            output_before: ob,
            output_after: ob + recv,
            vault_before: vb,
            vault_after: vb + amount_in - r,
        };
        // min_amount_out = 0: no check against amount_out, even when less arrived.
        let result = settle(&m, amount_in, 0, b);
        let kept = amount_in - r;
        if kept == 0 {
            let expected = if recv > 0 { SettleError::OutputWithoutInput } else { SettleError::NothingSwapped };
            prop_assert_eq!(result, Err(expected));
        } else if ceil_bps(kept, b as u64).is_none() {
            prop_assert_eq!(result, Err(SettleError::MathOverflow));
        } else {
            let s = result.unwrap();
            prop_assert_eq!(s.net_sent, kept);
            prop_assert_eq!(s.trader_spent, kept);
            prop_assert_eq!(s.vault_kept, kept);
            prop_assert_eq!(s.pipeline_fee, pipeline_fee(kept));
            // Success does not depend on amount_out: a short delivery still settles.
            let _ = amount_out;
            prop_assert_eq!(s.received, recv);
        }
        // With min_amount_out = amount_out the check is exactly received ≥ amount_out.
        let with_min = settle(&m, amount_in, amount_out, b);
        if recv < amount_out {
            prop_assert_eq!(with_min, Err(SettleError::InsufficientOutput));
        } else {
            prop_assert_eq!(with_min, result);
        }
    }

    /// INV-SWAP-15: transfer-fee input. The trader spends `spent ≤ amount_in`, the vault
    /// keeps `spent − tfee`; the base is `spent`, with no gross-up.
    #[test]
    fn prop_inv_swap_15_transfer_fee_input(ib in amount(), vb in amount(), amount_in in 1u64..=u64::MAX, spent in amount(), tfee_bps in 0u64..=10_000, b in bps()) {
        let spent = spent.min(amount_in).max(1);
        let ib = ib.max(spent);
        let tfee = ceil_bps(spent, tfee_bps).unwrap();
        let kept = spent - tfee;
        let vb = vb.min(u64::MAX - kept);
        let m = Measured {
            input_before: ib,
            input_after: ib - spent,
            output_before: 0,
            output_after: 1,
            vault_before: vb,
            vault_after: vb + kept,
        };
        match settle(&m, amount_in, 1, b) {
            Ok(s) => {
                prop_assert_eq!(s.net_sent, spent);
                prop_assert_eq!(s.pipeline_fee, pipeline_fee(spent));
                prop_assert_eq!(Some(s.integrator_fee), ceil_bps(spent, b as u64));
            }
            Err(e) => {
                prop_assert_eq!(e, SettleError::MathOverflow);
                prop_assert!(ceil_bps(spent, b as u64).is_none());
            }
        }
    }

    /// INV-SWAP-16: `hook_split` selects exactly the trailing `k` accounts or refuses.
    #[test]
    fn prop_inv_swap_16_hook_split(len in prop_oneof![Just(0usize), Just(usize::MAX), 0usize..300, any::<usize>()], k in any::<u8>()) {
        match hook_split(len, k) {
            None => prop_assert!(k as usize > len),
            Some(i) => {
                prop_assert!(k as usize <= len);
                prop_assert_eq!(i, len - k as usize);
                prop_assert!(i <= len);
            }
        }
    }
}

/// INV-SWAP-19 / INV-FEE-19 fixed edge cases.
#[test]
fn prop_inv_swap_edges() {
    let zero = Measured {
        input_before: 10,
        input_after: 10,
        output_before: 0,
        output_after: 0,
        vault_before: 0,
        vault_after: 0,
    };
    assert_eq!(settle(&zero, 0, 0, 0), Err(SettleError::NothingSwapped));
    let free_output = Measured {
        output_after: 5,
        ..zero
    };
    assert_eq!(
        settle(&free_output, 0, 0, 0),
        Err(SettleError::OutputWithoutInput)
    );
    assert_eq!(
        settle(&free_output, 100, 0, 0),
        Err(SettleError::OutputWithoutInput)
    );
    let big = Measured {
        input_before: u64::MAX,
        input_after: u64::MAX - (THRESHOLD_65535 + 1),
        output_before: 0,
        output_after: 1,
        vault_before: 0,
        vault_after: THRESHOLD_65535 + 1,
    };
    assert_eq!(
        settle(&big, u64::MAX, 1, 65_535),
        Err(SettleError::MathOverflow)
    );
    assert_eq!(
        settle(&big, THRESHOLD_65535, 1, 65_535),
        Err(SettleError::SpendLimitExceeded)
    );
    let at = Measured {
        input_after: u64::MAX - THRESHOLD_65535,
        vault_after: THRESHOLD_65535,
        ..big
    };
    let s = settle(&at, THRESHOLD_65535, 1, 65_535).unwrap();
    assert_eq!(Some(s.integrator_fee), ceil_bps(THRESHOLD_65535, 65_535));
    assert_eq!(s.net_sent, THRESHOLD_65535);
}
