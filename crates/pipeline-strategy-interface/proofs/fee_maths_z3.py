"""Z3 proofs of the router's fee maths for every 64-bit input.

Kani proves most of the router's properties (see `src/bok_kani/`), but its SAT back end
can't relate a division's output to its inputs over the full 64-bit range, so its
proofs of the fee rounding finish only for small amounts. Here Z3 proves the same
statements over the integers, for every input, by modelling `fees::ceil_bps`,
`fees::max_total_spend` and `fees::amount_for_budget` step by step, including each
checked operation's overflow (a value above u64::MAX means `None`).

The link between this model and the compiled Rust is checked by tests: Kani proves the
Rust matches the same statements for small amounts and at fixed rates, Proptest checks
them on random inputs, and `ceil_bps_matches_the_128_bit_definition` checks the Rust
against the 128-bit definition on edge cases and 200,000 random pairs.

Run: `pip install z3-solver` (in a virtualenv), then `python fee_maths_z3.py`.
"""

import sys

from z3 import And, If, Implies, Int, IntVal, Not, Or, Solver, sat, unsat

M = 2**64 - 1  # u64::MAX
D = 10_000  # BPS_DENOMINATOR
PIPELINE_BPS = 25


def u64(name):
    v = Int(name)
    return v, And(v >= 0, v <= M)


def ceil_div(n, d):
    return (n + d - 1) / d  # Z3 integer division is floor for non-negative values


def ceil_bps(amount, bps):
    """`fees::ceil_bps` as written: returns (is_some, value)."""
    # Plain Python integers would make `/` float division; keep everything in Z3.
    amount, bps = IntVal(amount) if isinstance(amount, int) else amount, (
        IntVal(bps) if isinstance(bps, int) else bps
    )
    q, r = amount / D, amount % D
    bq, br = bps / D, bps % D
    whole = q * bps  # checked_mul
    middle = r * bq  # checked_mul
    rest = ceil_div(r * br, D)  # r * br < D², never overflows
    sum1 = whole + middle  # checked_add
    total = sum1 + rest  # checked_add
    some = And(whole <= M, middle <= M, sum1 <= M, total <= M)
    return some, total


def spend_of(amount, bps):
    """The exact value `fees::max_total_spend` returns (when it returns Some)."""
    return amount + ceil_div(PIPELINE_BPS * amount, D) + ceil_div(bps * amount, D)


def prove(name, claim, *assumptions):
    solver = Solver()
    solver.set("timeout", 600_000)
    solver.add(*assumptions)
    solver.add(Not(claim))
    result = solver.check()
    if result == unsat:
        print(f"PROVEN  {name}")
        return True
    if result == sat:
        print(f"FAILED  {name}: counterexample {solver.model()}")
    else:
        print(f"UNKNOWN {name} ({solver.reason_unknown()})")
    return False


def main():
    ok = True
    a, a_ok = u64("amount")
    b, b_ok = u64("bps")

    # ceil_bps returns exactly ceil(a·b / 10,000), and None exactly when that exceeds u64.
    some, value = ceil_bps(a, b)
    exact = ceil_div(a * b, D)
    ok &= prove(
        "ceil_bps is the exact ceiling, None exactly on overflow (every u64 × u64)",
        And(some == (exact <= M), Implies(some, value == exact)),
        a_ok,
        b_ok,
    )

    # At most 100% the fee always fits and never exceeds the amount.
    ok &= prove(
        "ceil_bps ≤ amount and is Some for bps ≤ 10,000",
        Implies(b <= D, And(some, value <= a)),
        a_ok,
        b_ok,
    )

    # Pipeline's fee: ceil(25a / 10,000), at least 1 for any non-zero amount.
    pf_some, pf = ceil_bps(a, PIPELINE_BPS)
    ok &= prove(
        "pipeline_fee = ceil(a/400), ≥ 1 iff a ≥ 1, never panics",
        And(pf_some, pf == ceil_div(a, 400), (pf >= 1) == (a >= 1), pf <= a),
        a_ok,
    )

    # Splitting a swap never pays less: fee(x + y) ≤ fee(x) + fee(y).
    x, x_ok = u64("x")
    y, y_ok = u64("y")
    fx_some, fx = ceil_bps(x, PIPELINE_BPS)
    fy_some, fy = ceil_bps(y, PIPELINE_BPS)
    fxy_some, fxy = ceil_bps(x + y, PIPELINE_BPS)
    ok &= prove(
        "pipeline_fee(x + y) ≤ pipeline_fee(x) + pipeline_fee(y) ≤ pipeline_fee(x + y) + 1",
        And(fxy <= fx + fy, fx + fy <= fxy + 1),
        x_ok,
        y_ok,
        x + y <= M,
    )

    # max_total_spend(a, bps) = a + pipeline_fee(a) + ceil_bps(a, bps), None on overflow.
    r, r_ok = u64("integrator_bps")
    r_is_u16 = r <= 65_535
    pf_a_some, pf_a = ceil_bps(a, PIPELINE_BPS)
    int_some, int_fee = ceil_bps(a, r)
    mts_some = And(int_some, a + pf_a <= M, a + pf_a + int_fee <= M)
    mts = a + pf_a + int_fee
    exact_mts = a + ceil_div(25 * a, D) + ceil_div(r * a, D)
    ok &= prove(
        "max_total_spend = a + ceil(25a/D) + ceil(bps·a/D), None exactly on overflow",
        And(mts_some == (exact_mts <= M), Implies(mts_some, mts == exact_mts)),
        a_ok,
        r_ok,
        r_is_u16,
    )

    # amount_for_budget: start at floor(budget·D / (D + 25 + bps)) and step down while the
    # total spend exceeds the budget. Prove the start is at most one above the answer and
    # the answer is the largest amount within budget.
    budget, budget_ok = u64("budget")
    start = budget * D / (D + 25 + r)

    def spend(amount):
        return amount + ceil_div(25 * amount, D) + ceil_div(r * amount, D)

    ok &= prove(
        "amount_for_budget: the start is within budget, or one step down is",
        Or(spend(start) <= budget, And(start >= 1, spend(start - 1) <= budget)),
        budget_ok,
        r_ok,
        r_is_u16,
    )
    ok &= prove(
        "amount_for_budget: one above the start is over budget (the result is maximal)",
        spend(start + 1) > budget,
        budget_ok,
        r_ok,
        r_is_u16,
    )

    # Monotonicity: a larger amount or rate never gives a smaller fee (INV-FEE-5).
    a2, a2_ok = u64("amount2")
    b2, b2_ok = u64("bps2")
    s_hi_a, v_hi_a = ceil_bps(a2, b)
    s_hi_b, v_hi_b = ceil_bps(a, b2)
    ok &= prove(
        "ceil_bps is monotone in the amount and in the rate",
        And(
            Implies(And(a <= a2, s_hi_a), And(some, value <= v_hi_a)),
            Implies(And(b <= b2, s_hi_b), And(some, value <= v_hi_b)),
        ),
        a_ok,
        a2_ok,
        b_ok,
        b2_ok,
    )

    # Splitting never pays less, at any rate (INV-FEE-7 / INV-SWAP-9).
    sx, vx = ceil_bps(x, b)
    sy, vy = ceil_bps(y, b)
    sxy, vxy = ceil_bps(x + y, b)
    ok &= prove(
        "ceil_bps(x + y) ≤ ceil_bps(x) + ceil_bps(y) ≤ ceil_bps(x + y) + 1, any rate",
        Implies(sxy, And(sx, sy, vxy <= vx + vy, vx + vy <= vxy + 1)),
        x_ok,
        y_ok,
        x + y <= M,
        b_ok,
    )

    # max_total_spend is monotone in the amount and the rate (INV-FEE-10).
    r2, r2_ok = u64("integrator_bps2")
    ok &= prove(
        "max_total_spend is monotone in the amount and the rate",
        And(
            Implies(a <= a2, spend_of(a, r) <= spend_of(a2, r)),
            Implies(r <= r2, spend_of(a, r) <= spend_of(a, r2)),
        ),
        a_ok,
        a2_ok,
        r_ok,
        r2_ok,
        r_is_u16,
        r2 <= 65_535,
    )

    # The trader's total debit in a swap is within max_total_spend(amount_in) (INV-FEE-11).
    # `settle` charges both fees on net_sent, with trader_spent ≤ net_sent ≤ amount_in
    # (both proven by Kani for every input). The fees are the exact ceilings (proven
    # above), so the debit is at most max_total_spend(net_sent); and max_total_spend is
    # monotone (proven above), so that is at most max_total_spend(amount_in).
    amount_in, amount_in_ok = u64("amount_in")
    net, net_ok = u64("net_sent")
    spent, spent_ok = u64("trader_spent")
    debit = spent + ceil_div(PIPELINE_BPS * net, D) + ceil_div(r * net, D)
    ok &= prove(
        "trader debit = spent + fees(net_sent) ≤ max_total_spend(net_sent)",
        debit <= spend_of(net, r),
        net_ok,
        spent_ok,
        spent <= net,
        r_ok,
        r_is_u16,
    )
    ok &= prove(
        "max_total_spend(net_sent) ≤ max_total_spend(amount_in) when net_sent ≤ amount_in",
        spend_of(net, r) <= spend_of(amount_in, r),
        amount_in_ok,
        net_ok,
        net <= amount_in,
        r_ok,
        r_is_u16,
    )

    # credited_input never charges more than the amount sent (INV-FEE-17): the credit is
    # min(vault − reserve, sent), so a pre-deposit can't raise the fee base.
    vault, vault_ok = u64("vault")
    reserve, reserve_ok = u64("reserve")
    sent, sent_ok = u64("sent")
    surplus = If(vault > reserve, vault - reserve, 0)
    credited = If(surplus < sent, surplus, sent)
    s_c, f_c = ceil_bps(credited, PIPELINE_BPS)
    s_s, f_s = ceil_bps(sent, PIPELINE_BPS)
    ok &= prove(
        "credited_input ≤ sent, and its fee ≤ the fee on what was sent",
        And(credited <= sent, s_c, s_s, f_c <= f_s),
        vault_ok,
        reserve_ok,
        sent_ok,
    )

    print("all proven" if ok else "NOT ALL PROVEN")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
