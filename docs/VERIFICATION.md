# Verification of the router's maths and money movements

Before mainnet, the router was verified with
[Book of Knowledge](https://github.com/MetalLegBob/solana-vibes-kit) (a maths-verification
skill) and audited with Stronghold of Security (see [`SECURITY-AUDIT.md`](SECURITY-AUDIT.md)).
This page covers the verification: 47 properties (invariants) that the router's arithmetic
and money movements must always satisfy, and how each one is established.

**Result: no counterexample anywhere.** Every arithmetic property is proven for every
possible input; every account-level property passes its tests against the real router.

## How the properties are checked

| Tool | What it gives | Where |
|---|---|---|
| **Kani** (model checker) | A proof for every input of the Rust code itself | `crates/pipeline-strategy-interface/src/bok_kani/` |
| **Z3** (SMT solver) | A proof for every 64-bit input of the fee arithmetic, modelled step by step including each overflow check | `crates/pipeline-strategy-interface/proofs/fee_maths_z3.py` |
| **Proptest** | Thousands of random inputs against the real functions | `crates/pipeline-strategy-interface/tests/bok_prop_*.rs`, `crates/pipeline-tests/tests/bok_prop_*.rs` |
| **LiteSVM** | Transactions against the compiled router | `crates/pipeline-tests/tests/bok_*.rs` |

The swap's measurement and fee arithmetic lives in one pure function,
`pipeline_strategy_interface::settlement::settle`, which the router calls; that is what
makes it provable. Kani's SAT back end can't relate a division's output to its inputs over
the full 64-bit range, so the fee rounding is proven with Z3 over the integers instead, and
the Kani proofs of `settle` use the fee function's proven contract. Those full-domain Kani
harnesses stay in the code behind `--cfg kani_full`; their bounded versions run by default.

Run them:

```sh
cargo kani -p pipeline-strategy-interface -Z unstable-options -Z stubbing   # 45 proofs, ~2 min
python -m venv /tmp/z3 && /tmp/z3/bin/pip install z3-solver
/tmp/z3/bin/python crates/pipeline-strategy-interface/proofs/fee_maths_z3.py  # 13 proofs, ~6 s
cargo test -p pipeline-strategy-interface -p pipeline-tests                  # Proptest + LiteSVM
```

CI runs all of them on every push.

## The properties

**Settlement (`settle`, every input)**

| Property | How |
|---|---|
| The fee base `net_sent` is never more than `amount_in` | Kani |
| `net_sent` is never less than what the trader actually spent | Kani |
| `net_sent = min(amount_in, max(trader_spent, vault_kept))`: refilling the trader's account mid-swap can't lower it | Kani |
| The trader never spends more than `amount_in` before fees, else the swap fails | Kani |
| The output minimum is checked first and always enforced | Kani |
| Output with no input is refused; any successful swap pays a fee of at least 1 unit | Kani |
| A swap that moves nothing either way is refused | Kani |
| With a transfer-fee input the trader's spend is the fee base (no gross-up) | Kani |
| The settlement arithmetic never panics | Kani |
| Hook accounts are exactly the trailing accounts; too few is an error, never a panic | Kani |

**Fees (every input)**

| Property | How |
|---|---|
| `ceil_bps` is exactly `ceil(amount × bps / 10,000)`, and `None` exactly when that overflows | Z3 |
| Up to 100% the fee always exists and never exceeds the amount | Z3 |
| A fee is 0 exactly when the amount or the rate is 0 | Kani |
| Fees never fall as the amount or the rate rises | Z3 |
| Pipeline's fee is `ceil(amount / 400)`: 0.25%, at least 1 unit on any non-zero amount, never panics | Z3, Kani |
| Splitting a swap never pays less (any rate) | Z3 |
| `max_total_spend` is exactly `amount + ceil(25·amount/10,000) + ceil(bps·amount/10,000)` and monotone | Z3 |
| A swap's total debit is within `max_total_spend(amount_in)` | Z3 + Kani |
| `amount_for_budget` returns the largest amount within budget and steps down at most once | Z3 |
| `amount_for_budget` is monotone in budget and rate | Proptest |
| Integrator fees above 100% fail cleanly (`MathOverflow`) | Z3, LiteSVM |
| Every hop of a route pays its own fee | Proptest, LiteSVM |

**Credited input (`credited_input`, every input)**

| Property | How |
|---|---|
| Never more than was sent or than arrived; never panics | Kani |
| A pre-deposit can't raise the credit or the fee above what this call sent | Kani, Z3 |
| A transfer-fee shortfall is credited exactly; monotone | Kani |

**The swap handler (against the real router)**

| Property | How |
|---|---|
| Pipeline's fee goes only to this market's fee account for the input mint | LiteSVM |
| When paused, a swap fails before anything moves | LiteSVM |
| The event and return data equal what actually moved | LiteSVM |
| Every account aliasing the checks allow either fails or keeps the guarantees above | LiteSVM |
| Exact-out: refused if unsupported; a refund lowers the fee base exactly | LiteSVM, Proptest |
| Balances read after the strategy call come from the same valid token accounts | LiteSVM |

**Admin, registration, markets, sweeps, parsing (against the real router)**

| Property | How |
|---|---|
| The listing fee is 0 or between the rent-exempt minimum and the cap, on every path | LiteSVM, Proptest |
| Registration charges exactly the configured fee, never above the registrant's maximum | LiteSVM |
| Pausing works only while the router is upgradeable; `lift_pause` only once it's frozen | LiteSVM, Proptest |
| Creating a market costs the creator at most `max_creator_lamports` plus rent; the setup payer ends empty | LiteSVM |
| Only fresh, non-creator accounts keep their signature in market creation; swaps forward none | LiteSVM, Proptest |
| Market info: 2–4 distinct mints with one vault each, from the strategy itself; decoding never panics | LiteSVM, Proptest |
| A market is recorded once, exactly as reported; mint lookup never panics | LiteSVM, Proptest |
| A sweep empties the fee account into the treasury only (less any Token-2022 cut) | LiteSVM |
| Token and loader account parsing accepts exactly what Solana's own parsers accept; a 355-byte multisig is never read as a token account | LiteSVM, Proptest |

Some Proptest suites check copies of private router functions; a test
(`bok_copies_match_router.rs`) fails if the router's code changes underneath them.
