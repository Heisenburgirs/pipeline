# Stronghold of Security — Final Audit Report

**Project:** Pipeline router
**Audit:** #2 (router only, pre-mainnet)
**Audit Date:** 2026-11-09
**Git Ref:** `73e5ca0`
**Auditor:** Stronghold of Security v1.0 (automated adversarial audit)
**Scope:** `programs/pipeline/src/lib.rs`, `programs/pipeline/src/token.rs`, `crates/pipeline-strategy-interface/src/lib.rs`

Line references are to these three files at `73e5ca0`. `lib.rs` means `programs/pipeline/src/lib.rs`, `token.rs` means `programs/pipeline/src/token.rs` and `interface` means `crates/pipeline-strategy-interface/src/lib.rs`.

---

## Executive Summary

This audit found no CRITICAL or HIGH issues in the Pipeline router. It found one MEDIUM and five LOW issues: four LOW are confirmed and one is potential. None of them is a code defect that lets someone take funds the router protects. The MEDIUM issue (H014) was in the documented multi-hop guidance, not in the router. Two LOW issues (H004, H030) are edge cases of a documented design choice in `create_market`. One LOW (H024) is a doc comment that did not match the code. One LOW (H027) is an admin power. The potential issue (H010) needs a strategy that moves its own input out of its vault during the swap.

The router's fee and settlement arithmetic was not re-audited here. The Book of Knowledge verification proved it for every input with Kani and Z3: 47 invariants, 0 violations ([`VERIFICATION.md`](VERIFICATION.md), including its addendum). This audit therefore looked at what feeds that arithmetic: account binding, signatures passed to other programs, Token-2022 behaviour, aliasing, ordering, admin powers and events. The three router findings from audit #1 were rechecked, and all three fixes hold.

**Resolution in one sentence:** H014 and H024 are fixed in the documentation; H004, H030, H027 and H010 are accepted as design and documented, with tests that pin the behaviour. The three coverage gaps (G001–G003) remain open as low-risk follow-ups.

### Key Statistics

| Metric | Count |
|--------|-------|
| Attack hypotheses investigated | 80 (14 Tier 1, 24 Tier 2, 42 Tier 3) |
| CONFIRMED | 5 |
| POTENTIAL | 1 |
| Investigated & cleared (NOT VULNERABLE) | 74 (9 of them carry an informational note) |
| Requires manual review | 0 |
| Coverage gaps carried as follow-ups | 3 (G001 MEDIUM, G002 LOW, G003 LOW) |
| LiteSVM proof-of-concept test files written | 52 (`crates/pipeline-tests/tests/sos2_*.rs`) |

### Severity Distribution

| Severity | Count | CVSS Range | Requires Immediate Action |
|----------|-------|------------|---------------------------|
| CRITICAL | 0 | 9.0 – 10.0 | — |
| HIGH | 0 | 7.0 – 8.9 | — |
| MEDIUM | 1 | 4.0 – 6.9 | Fixed (documentation) |
| LOW | 5 (4 confirmed, 1 potential) | 0.1 – 3.9 | Resolved: fixed or accepted |
| INFO | 9 (notes on cleared items) | N/A | Optional |

### Findings Summary

| ID | Finding | Severity | Status | CVSS 3.1 | Location | Resolution |
|----|---------|----------|--------|----------|----------|------------|
| H014 | Multi-hop guidance left earlier hops without a minimum; a pre-held intermediate balance silently backfills a sandwiched first hop | MEDIUM | CONFIRMED | 5.3 `AV:N/AC:H/PR:N/UI:R/S:U/C:N/I:H/A:N` | docs; `interface:372` | Fixed in docs |
| H004 | Freshness is judged per instruction: a signer emptied by the fee or an earlier instruction looks fresh | LOW | CONFIRMED | 3.1 `AV:N/AC:H/PR:N/UI:R/S:U/C:N/I:L/A:N` | `lib.rs:231-235`, `:825-827` | Accepted, documented |
| H030 | A program's zero-lamport authority PDA, signed into `create_market` via CPI, reaches the strategy | LOW | CONFIRMED | 3.1 `AV:N/AC:H/PR:N/UI:R/S:U/C:N/I:L/A:N` | `lib.rs:231-235`, `:825-837` | Accepted, documented |
| H024 | Exact-in `trade.amount` is the gross `amount_in`; the docs said it was the net amount that arrives | LOW | CONFIRMED | 3.1 `AV:N/AC:H/PR:N/UI:R/S:U/C:N/I:L/A:N` | `lib.rs:486-488`; `interface:154`, `:162-164` | Fixed in docs |
| H027 | After the freeze, the admin can block all registrations by pointing `listing_fee_recipient` at a write-protected key | LOW | CONFIRMED | 2.7 `AV:N/AC:L/PR:H/UI:N/S:U/C:N/I:N/A:L` | `lib.rs:73-75`, `:1043-1045` | Accepted (admin trust) |
| H010 | Fee base when input leaves the declared vault during the call and `trader_input` is refilled from outside | LOW | POTENTIAL | 3.1 `AV:N/AC:H/PR:L/UI:N/S:U/C:N/I:L/A:N` | `lib.rs:394-398`, `:449-531`; `interface:378` | Accepted |

**Average CVSS (confirmed):** 3.5. **Highest:** H014 at 5.3.

---

## Resolution

Each finding has been decided by the project; these resolutions are final for this audit.

- **H014 (MEDIUM, multi-hop guidance): fixed in the docs.** Every hop now carries its own `min_amount_out` (its quote less slippage), each later hop is sized from the previous hop's minimum rather than its quote (so it can never dip into tokens the trader already held), and the last hop also carries the trader's end-to-end minimum. This is now in `docs/INTEGRATOR-GUIDE.md` and `docs/ROUTER.md`. A regression test in `crates/pipeline-tests/tests/sos2_h014.rs` (`h014_first_hop_minimum_refuses_sandwiched_route`) shows that a first-hop minimum makes the sandwiched route revert and leaves the pre-held balance untouched.
- **H004 and H030 (LOW, fresh-signer forwarding): accepted and documented.** The router can't tell a new keypair from a wallet with zero SOL (including one emptied earlier in the same transaction), or from a program's PDA signing through a CPI. Callers must sign `create_market` only with new keypairs, and programs must not pass their own PDAs as signers. This is stated in the `create_market` doc comment, `docs/STRATEGY-GUIDE.md` and the public router docs (`public/docs/ROUTER.md`). Tests in `sos2_h004.rs` and `sos2_h030.rs` pin the behaviour.
- **H024 (LOW, docs): fixed.** The `TradeRequest.amount` doc comment in the interface crate and `docs/STRATEGY-GUIDE.md` now say that for exact-in, `amount` is what the router sent, before any transfer fee.
- **H027 (LOW): accepted as admin trust.** The admin can block registrations just as well by raising the listing fee to the cap. A test (`sos2_h027.rs`) pins the behaviour.
- **H010 (POTENTIAL, LOW): accepted.** It needs the strategy itself to move input out of its own vault during the call. That is equivalent to the accepted bypass of calling the strategy directly.
- **Coverage gaps G001–G003: open, low-risk follow-ups.**
  - G001: check the build output for sBPF stack-frame warnings.
  - G002: show that mints that are frozen by default, or NonTransferable, fail closed.
  - G003: measure the heap ceiling for a hook-heavy swap.

  G001 is rated a MEDIUM *coverage gap* because a failure there would be a deterministic revert (a liveness problem), not a loss of funds.

---

## Scope, Threat Model and Accepted Design

Pipeline is a permissionless router. Developers register arbitrary strategy programs. Every market trades through the router, which moves the trader's input into the strategy's vault, calls the strategy, measures balances and charges a fixed 0.25% fee. The router holds no liquidity and computes no price.

**What the router must protect**, against any strategy, token issuer, trader, front-runner or third party:
- **Traders.** The trader never signs for the strategy and never pays more than `amount_in` plus the stated fees. The trader receives at least `min_amount_out`, or the swap fails.
- **Pipeline's fee.** Each swap pays 0.25% of what it is charged for (`net_sent`). Fees go only to the market's fee account, and from there only to the treasury.
- **Market isolation and identity.** One market can't affect another market's accounts, fees or record.
- **Registry and admin bounds.** Registration needs the program's upgrade authority or its `["pipeline-register"]` PDA. Admin powers are bounded.
- **Liveness.** Nobody can permanently block swaps, market creation, fee sweeps or registration for others.

**Accepted design** (not reported as vulnerabilities):
- Calling a strategy directly, which bypasses the router and its fee, is allowed.
- A hostile strategy can steal its own pool or its own users' input.
- Integrator fees are uncapped; the trader signs the rate.
- `create_market` passes on the signature of *fresh* accounts, so a strategy can create them. A fresh account is one that signs and is empty: no lamports, no data, owned by the System program. A wallet with zero SOL looks the same.
- Admin trust: while the router is upgradeable, the admin can pause. The admin can change the listing fee within the cap and change the recipients.
- Token issuers keep their own powers over their own token's markets: freeze, permanent delegate, transfer fee and transfer hooks.

**Out of scope:** Pipeline's own strategies are out of scope for this audit. Test programs, SDKs, services and apps were used only as context or test fixtures.

---

## Critical Findings

None.

---

## High Priority Findings

None.

---

## Medium Priority Findings

| ID | Title | Location | Issue | Resolution |
|----|-------|----------|-------|------------|
| H014 | Multi-hop guidance left earlier hops without a minimum | `docs/INTEGRATOR-GUIDE.md`, `docs/ROUTER.md` | A sandwiched first hop was topped up from tokens the trader already held | Fixed in docs; regression test |

### MEDIUM-001: Multi-hop guidance put the minimum only on the last hop

**ID:** H014
**Severity:** MEDIUM
**CVSS:** 5.3 (`CVSS:3.1/AV:N/AC:H/PR:N/UI:R/S:U/C:N/I:H/A:N`)
**Status:** CONFIRMED (confidence 9/10; LiteSVM test with the real router and the repository's constant-product test strategy, no crafted state)
**Classification:** NEW
**Location:** the guidance in `docs/INTEGRATOR-GUIDE.md` (multi-hop, §3), `docs/ROUTER.md` (§2.4) and the routing guidance. The router's per-hop check is at `interface:372`, called from `lib.rs:525-531`.

#### Description

A multi-hop route is several `swap` instructions in one transaction. The router checks each hop on its own: the hop fails if less than that hop's `min_amount_out` reaches that hop's `trader_output`. The guidance told integrators to put the trader's end-to-end minimum only on the last hop, and to size each later hop's `amount_in` from the previous hop's quote with `amount_for_budget`. That fixes hop 2's `amount_in` when the trader signs.

Hop 2 spends its fixed `amount_in` from the trader's intermediate-token account, whatever hop 1 delivered. If that account already holds a balance (common for USDC or wSOL, and for desks and bots), a hop 1 that delivers almost nothing is topped up from that balance. The last hop's minimum still passes, and the route succeeds.

#### Attack Scenario

1. The victim's intermediate account (hop 1's output and hop 2's input) already holds at least what hop 2 will spend.
2. The victim signs a route built per the old guidance: hop 1 `min_amount_out = 0`, hop 2 with a fixed `amount_in` and the end-to-end minimum.
3. The attacker front-runs hop 1's market until hop 1 would deliver close to nothing.
4. Hop 1 delivers 60,934 B against a quote of 906,610 B. Hop 2 spends 906,610 B, mostly from the pre-held balance. The final minimum passes.
5. The attacker back-runs and keeps most of the victim's hop-1 input.

In the test, the route took 845,676 B from the trader's existing 5,000,000 B. The sandwicher made 798,370 A on a 1,002,500 A trade (about 80%), after router and pool fees on both legs.

#### Impact

The victim could lose up to the whole value of each non-final hop. The route reported success and the victim received exactly the signed minimum, so nothing looked wrong. Without an attacker, ordinary hop-1 slippage was also absorbed silently from the pre-held balance. No shipped SDK or client built such routes: neither has a multi-hop builder yet. The exposure came from integrators following the docs.

#### Evidence

```rust
// interface:370-374 (settlement::settle) — the check is per hop, on this hop's own output account
let received = measured.output_after.saturating_sub(measured.output_before);
...
if received < min_amount_out {
    return Err(SettleError::InsufficientOutput);
}
```

Nothing links hop 2's debit of `trader_input` to what hop 1 delivered. With a single-market `swap` instruction, nothing in the router could link them.

#### Recommended Fix (applied)

No router change. The guidance now says to give every hop its own `min_amount_out` (its quote less slippage), with the end-to-end minimum on the last hop. A hop that under-delivers beyond its slippage now reverts the whole route.

What remains: a hop that delivers between its minimum and its quote can still be topped up by at most the difference. That difference is the slippage the trader already accepted for that hop, so this is the same exposure as any single swap. To remove even that, an integrator can do either of the following (optional):
- size each later hop from the previous hop's *minimum* rather than its quote;
- route through a fresh, empty intermediate account.

#### Tests

`crates/pipeline-tests/tests/sos2_h014.rs`:
- `h014_first_hop_minimum_refuses_sandwiched_route`: with the guidance, the sandwiched route fails with `InsufficientOutput` and the pre-held balance is unchanged. The un-sandwiched route succeeds.
- `h014_zero_first_hop_minimum_backfills_sandwiched_hop_from_preheld_balance`: pins the backfill that the guidance prevents.
- `h014_control_fresh_intermediate_account_reverts_sandwiched_route`: with an empty intermediate account, the route reverts.

**Resolution:** Fixed in the docs. Every hop carries its own minimum (INTEGRATOR-GUIDE, ROUTER.md), and a regression test shows the sandwiched route reverts and leaves the pre-held balance untouched.

---

## Low Priority Findings

| ID | Title | Location | Issue | Resolution |
|----|-------|----------|-------|------------|
| H004 | Freshness judged per instruction | `lib.rs:231-235`, `:825-827` | A wallet emptied earlier in the same transaction looks fresh | Accepted, documented |
| H030 | Program authority PDAs look fresh | `lib.rs:231-235`, `:825-837` | A zero-lamport PDA that an integrator signs via CPI reaches the strategy | Accepted, documented |
| H024 | Exact-in `trade.amount` is gross | `lib.rs:486-488`; `interface:154`, `:162-164` | Docs said net; a strategy following them over-credits | Fixed in docs |
| H027 | Admin can block registrations after freeze | `lib.rs:73-75`, `:1043-1045` | An unvalidated recipient plus Anchor `mut` refuses every registration | Accepted (admin trust) |
| H010 | Fee base with forwarding strategies plus outside refills | `lib.rs:394-398`, `:449-531`; `interface:378` | Both terms of `max(trader_spent, vault_kept)` can be lowered in one swap | Accepted |

### LOW-001: Freshness is judged per instruction, not per transaction

**ID:** H004 · **Severity:** LOW · **CVSS:** 3.1 · **Status:** CONFIRMED (confidence 9/10) · **Classification:** NEW (related to audit #1 H017)
**Location:** `lib.rs:231-235` (`keeps_signature`), `lib.rs:825-827` (`is_fresh`), `lib.rs:830-837` (`meta_like`)

#### Description

`create_market` keeps an account's signature for the strategy only if the account is not this instruction's creator and is fresh. `is_fresh` reads the account's live balance when the instruction starts. The audit #1 fix excludes the creator by key, and that fix holds. But any *other* wallet that signed the transaction and was brought to exactly 0 lamports earlier in the same transaction also looks fresh. On Solana a signature applies to the whole transaction, so the wallet is a signer in every instruction that lists it, even if the client set `is_signer = false` in this one.

```rust
// lib.rs:231-232
let creator = ctx.accounts.creator.key();
let keeps_signature = |account: &AccountInfo| *account.key != creator && is_fresh(account);

// lib.rs:825-827
fn is_fresh(account: &AccountInfo) -> bool {
    account.lamports() == 0 && account.data_is_empty() && *account.owner == system_program::ID
}
```

Four cases were tested:

| Case | What emptied the wallet | Signature passed on? |
|---|---|---|
| (a) separate fee payer holding exactly the fee | the runtime's transaction fee | yes (accepted: a zero-SOL wallet) |
| (d) fee payer "send max" to a new creator keypair | the wallet's own transfer | yes (accepted: a zero-SOL wallet) |
| (b) creator of an earlier `create_market` in the same transaction | the router (Market rent plus `max_creator_lamports`) | yes |
| (c) payer of an earlier `register_strategy` | the router (listing fee plus Strategy rent) | yes |

#### Attack Scenario

A hostile strategy's own web page builds one transaction. Instruction 1 creates a market with creator C1 and empties C1. Instruction 2 creates a market on the hostile strategy and lists C1 and C1's token account among the strategy accounts. The strategy then calls SPL `Approve(u64::MAX)` on C1's token account. The test shows this succeeding.

#### Impact

The strategy gets full delegation over the token accounts the transaction lists, plus anything else C1's signature authorises. The preconditions are:
- the victim signs a transaction that an attacker composed;
- the victim's balance is exactly zero at the right moment.

An honest single-`create_market` flow never meets them. The approval happens inside a strategy CPI, so it is less visible in a wallet preview than a top-level `Approve`.

#### Recommended Fix (applied)

There is no on-chain fix without reading the Instructions sysvar, which would be an interface change. Even that would not catch case (a). The rule is now documented in the `create_market` doc comment, `docs/STRATEGY-GUIDE.md` and `public/docs/ROUTER.md`: sign `create_market` only with new keypairs. A wallet emptied earlier in the same transaction counts as a zero-SOL wallet.

#### Tests

`crates/pipeline-tests/tests/sos2_h004.rs` has six tests:
- a funded-payer control;
- cases (a) and (d), pinned as accepted;
- cases (b), (b') with a hostile setup, and (c), pinned as documented behaviour.

Each test checks that the victim is at exactly 0 lamports before the second instruction.

**Resolution:** Accepted and documented. Callers must sign `create_market` only with new keypairs. Tests pin the behaviour.

---

### LOW-002: A program's authority PDA, signed via CPI, looks fresh

**ID:** H030 · **Severity:** LOW · **CVSS:** 3.1 · **Status:** CONFIRMED (confidence 9/10) · **Classification:** NEW
**Location:** `lib.rs:231-235`, `lib.rs:825-837`

#### Description

H030 is the program-caller version of H004. Suppose an integrator program calls `create_market` through `invoke_signed` and marks one of its own PDAs as a signer. That PDA has zero lamports and no data, and is owned by the System program (for example, the PDA that owns its escrow token accounts). It passes `is_fresh`, and it is not the creator, so the strategy receives its signature. The router can't tell a PDA's CPI signature from a keypair's. A program's authority PDA usually holds no lamports, so it always looks fresh.

#### Attack Scenario

The test runs a real chain of calls: transaction → integrator → router `create_market` → hostile strategy → SPL `Approve`. The hostile setup became delegate (`u64::MAX`) over an escrow holding 1,000,000 tokens that the integrator's PDA owns. In the control, a PDA holding 1 lamport did not keep its signature, and nothing was approved.

#### Impact

Anything the PDA's authority allows: approvals, transfers, authority changes on its token accounts. The integrator has to build a signer meta for the PDA by hand. Anchor's `CpiContext::with_remaining_accounts` passes on only outer-transaction signers, so this does not happen by accident in the usual Anchor pattern. The realistic case is an integrator that signs a PDA on purpose without knowing that the strategy then holds that authority.

#### Recommended Fix (applied)

Documentation. Programs must not pass their own PDAs as signers to `create_market`. The `create_market` doc comment, `docs/STRATEGY-GUIDE.md` and `public/docs/ROUTER.md` now say so. Two router-side options were considered and not adopted:
- never pass on any signer other than the router's own (this breaks strategies that create keypair accounts);
- an explicit list, chosen by the creator, of the accounts allowed to keep their signature.

Both conflict with the project's priority on developer flexibility.

#### Tests

`crates/pipeline-tests/tests/sos2_h030.rs`:
- `h030_fresh_authority_pda_signature_is_forwarded_documented`: pins the behaviour.
- `h030_authority_pda_with_one_lamport_is_not_forwarded`: control; a PDA with any lamports does not keep its signature.

**Resolution:** Accepted and documented. Programs must not pass their own PDAs as signers to `create_market`. Tests pin the behaviour.

---

### LOW-003: Exact-in `trade.amount` is the gross amount, but the docs said net

**ID:** H024 · **Severity:** LOW · **CVSS:** 3.1 · **Status:** CONFIRMED (confidence 9/10) · **Classification:** NEW
**Location:** `lib.rs:486-488` (request built), `interface:154` and `:162-164` (doc comments at `73e5ca0`), `docs/STRATEGY-GUIDE.md` §1

#### Description

For exact-in, the router sets `TradeRequest.amount = args.amount_in`. That is the amount it sent, before any Token-2022 transfer fee. At `73e5ca0`, the interface doc comment and the strategy guide said exact-in `amount` was "the input the strategy receives", after the transfer fee. The off-chain quoter follows the docs and passes the net amount to `pipeline_quote`. So a strategy that prices `trade.amount` directly in `pipeline_swap` would see quote and swap diverge, and it would over-credit its own pool by the transfer fee.

```rust
// lib.rs:486-488
amount: match args.mode {
    TradeMode::ExactIn => args.amount_in,
    TradeMode::ExactOut => args.amount_out,
},
```

#### Impact

The loss falls only on a third-party strategy's own pool, and only with a transfer-fee input mint. That is within accepted design (strategies hold their own liquidity). The router's guarantees are unchanged: the fee base is measured, the trader pays the gross amount plus 0.25%, and `min_amount_out` is checked on the measured output. Pipeline's own strategies use `credited_input`, which credits what arrived in the vault, and they are unaffected.

#### Recommended Fix (applied)

Docs only. `TradeRequest.amount` and the strategy guide now say that for exact-in, `amount` is what the router sent, before any transfer fee (the same as `amount_sent`), and that strategies should measure what arrived.

#### Tests

`crates/pipeline-tests/tests/sos2_h024.rs` pins three things:
- the gross `trade.amount`;
- the no-fee control;
- the router's unchanged guarantees, and that the constant-product test strategy credits what arrived.

**Resolution:** Fixed in the docs. For exact-in, `amount` is what the router sent, before any transfer fee.

---

### LOW-004: After the freeze, the admin can block all registrations through `listing_fee_recipient`

**ID:** H027 · **Severity:** LOW · **CVSS:** 2.7 · **Status:** CONFIRMED (confidence 9/10) · **Classification:** NEW
**Location:** `lib.rs:73-75` (`update_config` stores the recipient without checks), `lib.rs:1043-1045` (`RegisterStrategy.listing_fee_recipient` is always `mut`)

#### Description

`update_config` accepts any `listing_fee_recipient`. `register_strategy` always requires the recipient to be writable, even when the fee is 0. The runtime makes some keys read-only no matter what the transaction asks for: builtin programs, sysvars, and the program being called (Solana calls this write-demotion). If the admin sets the recipient to one of these, every `register_strategy` fails with Anchor `ConstraintMut`, even at fee 0. The test showed this for:
- the System program / `Pubkey::default()`;
- the Rent and Clock sysvars;
- ComputeBudget;
- the router's own ID.

```rust
// lib.rs:1043-1045
/// CHECK: receives SOL only; must match the config.
#[account(mut, address = config.listing_fee_recipient @ PipelineError::WrongFeeRecipient)]
pub listing_fee_recipient: UncheckedAccount<'info>,
```

#### Impact

Only new registrations are affected. Swaps, markets, sweeps, the pause and metadata are not. The admin can undo it with another `update_config`. If the admin key were lost after such a setting, the block would be permanent. Non-admins cannot change the recipient.

#### Why accepted

The admin can already deter all registrations by setting the listing fee to the 1,000 SOL cap. The admin is trusted (a multisig on mainnet). This adds a free, hard block, but it gives no new power over anyone's funds.

#### Optional hardening (not adopted)

- Reject reserved or executable recipients in `update_config`.
- Or don't require the recipient to be writable when the fee is 0.

#### Tests

`crates/pipeline-tests/tests/sos2_h027.rs` covers:
- the matrix of recipients after the freeze;
- the pinned block (`h027_admin_can_block_zero_fee_registration_via_recipient_accepted`);
- the admin can reverse it;
- a non-admin cannot change the recipient.

**Resolution:** Accepted as admin trust. A test pins the behaviour.

---

### LOW-005: Fee base when input leaves the declared vault and `trader_input` is refilled from outside

**ID:** H010 · **Severity:** LOW · **CVSS:** 3.1 · **Status:** POTENTIAL (confidence 8/10) · **Classification:** NEW
**Location:** `lib.rs:394-398` (vault pinned to `market.vaults[input_index]`), `lib.rs:449-531` (snapshots, CPI, re-reads, `settle`), `interface:378`

#### Description

The fee base is:

```rust
// interface:378
let net_sent = trader_spent.max(vault_kept).min(amount_in);
```

Here `trader_spent` is measured on `trader_input`, and `vault_kept` on the one vault the market declared for the input mint. The `max` stops Pipeline from losing fee when only one of the two is lowered. Two LiteSVM tests show that it does:
- an output-mint hook refilling `trader_input` alone is still charged on the full amount;
- a vault emptied alone is still charged on the full amount.

If both happen in the same swap, both terms are low. The router then charges `amount_in − min(R, F)`, where R is the refill and F is the input moved out of the vault. Applying `settle` to the measured balances of the two halves gives a fee of 250 instead of 2,500.

#### Preconditions

All three are needed together:
1. A strategy that moves the input it keeps out of its declared vault *during* `pipeline_swap`. Examples are a forwarding wrapper, or a launch strategy that pays a creator fee to a wallet mid-swap. None of Pipeline's strategies does this.
2. Code inside the call that credits `trader_input`:
   - with a legacy SPL input, that is the *output* mint's Token-2022 hook (the output issuer), or code the strategy calls;
   - with a Token-2022 input it is impossible, because Token-2022 is already on the stack.
3. A trader who parks R in the refill source before the swap.

#### Impact

Pipeline loses at most `25 bps × min(R, F)` per affected swap. The `net_sent` in `SwapEvent` understates the input for those swaps. Traders and strategies lose nothing. The combined swap was not run in one transaction, because no fixture does both movements. Its outcome follows from the proven `settle` applied to the two measured halves.

#### Why accepted

It needs the strategy itself to move input out of its own vault during the call. A strategy that wants its traders to pay less can already route them around the router entirely, which costs less and needs no issuer.

#### Optional documentation

Add a rule to the strategy guide: keep the input you keep in the declared vault until `pipeline_swap` returns, and refund only from it.

#### Tests

`crates/pipeline-tests/tests/sos2_h010.rs`:
- `h010_refill_alone_is_charged_on_what_the_vault_kept`: passes.
- `h010_forwarding_alone_is_charged_on_the_trader_spend`: passes; uses crafted state, a vault delegate standing in for a forwarding strategy.

**Resolution:** Accepted. It requires the strategy to move input out of its own vault during the call, which is equivalent to the accepted bypass of calling the strategy directly.

---

## Informational Notes

These come from cleared hypotheses. None is a vulnerability. Each is a runbook or documentation point.

- **H046 — freezing to a burn key keeps the pause power.** The router counts as frozen only when its upgrade authority is `None`. If the authority is handed to a burn key, the admin can still pause. The freeze must be `SetAuthority(None)`, ideally in the same transaction as `lift_pause`.
- **H047 — `MAX_LISTING_FEE_LAMPORTS` is a placeholder (1,000 SOL, `lib.rs:28-30`).** It becomes permanent at the freeze. The rent floor on the listing fee is checked only when the fee is set; a later rent rise is recoverable by the admin.
- **H039 — pre-funding a pending `create_market`'s fresh signer strips its signature.** This is a grief: the attempt reverts atomically and can be retried with a new keypair. It can only remove privilege, never add it.
- **H051 — the `["pipeline-register"]` PDA path.** If a program exposes a path that lets any caller make it sign with that PDA, anyone can register it and set its metadata URI. The upgrade authority can overwrite the URI while the program is upgradeable. No privilege is stored.
- **H052 — any account the strategy owns can be registered as a market.** The router checks ownership only. A third-party strategy whose `market_info` doesn't check the account type can get a record written over a non-market account. This affects only listings for that strategy.
- **H067 — the strategy receives no creator identity at init.** Markets whose state address comes from public seeds go to whoever creates them first. Using a fresh keypair as `market_state` avoids this.
- **H074 — `SwapEvent.integrator` is recorded whenever an integrator account is passed, even when the fee is 0.** Indexers should credit integrators by fee amount, not by the field alone.
- **H076 — transfer-fee input mints.** The issuer can withhold Pipeline's fee on the fee transfer, and the sweep to the treasury is taxed again. This is an issuer power over its own mint, but the docs don't mention it.
- **H080 — doc wording is stronger than the code in three places:**
  - "fee PDA never passed to strategies": a trader can list it, unsigned, among the remaining accounts;
  - "setup PDA holds at most `max_creator_lamports`": not if a third party pre-funds it;
  - "market PDA signs every router call": not `pipeline_market_info`.

  None of these is a capability.

Two further observations:
- **Freshness wording.** `lib.rs:228` and `docs/ROUTER.md` say freshness is judged "before the creator pays anything". More precisely, it is judged at the start of this instruction, after anything earlier in the transaction (see H004).
- **BOK drift guard (H065).** The guard checks that copied source fragments exist, not that constant values match. It supports the model tests, not the router.

---

## Combination Attack Analysis

The six actionable findings were checked in pairs, and each was also paired with the relevant cleared items (H039, H055, H018/H028). **No pair combines into anything worse than its parts.** Each finding needs a different actor and a different precondition. Where two findings touch the same code, fixing one does not make the other worse.

### Combination Matrix

| | H014 | H004 | H030 | H024 | H027 | H010 |
|---|---|---|---|---|---|---|
| **H014** | — | — | — | — | — | — |
| **H004** | — | — | shared code path | — | — | — |
| **H030** | — | shared code path | — | — | — | — |
| **H024** | — | — | — | — | — | shared state (exact-in input measurement) |
| **H027** | — | — | — | — | — | — |
| **H010** | — | — | — | shared state | — | — |

Category pairs checked: Access Control × Token/Economic, Access Control × CPI, Arithmetic × Token/Economic, State Machine × Timing, State Machine × CPI, Upgrade/Admin × Access Control, Error Handling × State Machine, Account Validation × CPI, Timing × Economic. Only the cells above have a "yes" to any of the five questions (enables, is enabled by, amplifies, shares code, shares state).

### Chain 1 (examined, no amplification): H004 + H030 + H039 — signatures of empty accounts

| ID | Severity | Role |
|----|----------|------|
| H004 | LOW | A wallet emptied earlier in the transaction keeps its signature |
| H030 | LOW | A program's empty PDA keeps its signature when signed through a CPI |
| H039 | INFO (cleared) | A third party can pre-fund a fresh account to strip its signature |

All three rest on one predicate, `is_fresh` (`lib.rs:825-827`), read live at the start of the instruction. Together they don't reach further:
- H004 needs the victim to sign a transaction the attacker composed.
- H030 needs an integrator program to build a signer meta for its own PDA.
- Neither makes the other easier: one is about outer-transaction wallets, the other about CPI-signed PDAs.
- H039 runs the other way. Pre-funding can only *remove* a signature.

So the combined reach is still "the signatures of empty accounts that the transaction itself lists". The documented rule ("sign only with new keypairs; don't pass your own PDAs") covers both findings at once. **Combined severity: LOW.**

### Chain 2 (examined, mutually exclusive): H010 + H024 — exact-in input accounting

Both concern how exact-in input is measured. H024 matters only with a **Token-2022 transfer-fee** input mint. H010's issuer-refill form needs a **legacy SPL** input, because with a Token-2022 input, Token-2022 is already on the stack and no hook can move `trader_input`. So the two can't apply to the same swap's input in the form that H010 describes. Neither affects the other's outcome:
- H024's over-credit is inside the strategy's own books;
- H010's shortfall is in the router's measured fee base, which never reads `trade.amount`.

**Combined severity: LOW.**

### Chain 3 (examined, no amplification): H010 + fee-base items (H018, H028)

The cleared items show the fee base resists refills alone, outflows alone, pre-deposits, donations, dust splitting, exact-out and multi-hop. H010 is the only case found where two outside movements in one swap lower it. A strategy that cooperates with its traders has a cheaper path: the traders call it directly and skip the router. H010 adds no reach beyond that accepted path.

### Chain 4 (examined, fixed): H014 + H055 (no deadline)

`swap` has no deadline. A route held under a durable nonce can land much later (H055, cleared). With the old guidance (no minimum on hop 1), that would have given a sandwicher unlimited time to set up the price. It would have raised the likelihood of H014 but not its impact, which was already capped at hop 1's value. With every hop carrying its own minimum, a late route is bounded hop by hop like any single swap.

### Chain 5 (examined): H027 + admin cluster (H046, H047)

H027 (block registrations through the recipient), H046 (burn-key freeze keeps the pause) and H047 (fee cap placeholder) all need the admin key. They are admin trust and don't widen anyone else's power. Their combined lesson belongs in the freeze runbook (see Recommendations).

### Findings That Enable Others

| Finding | Enables | Combined Impact |
|---------|---------|-----------------|
| — | — | No finding enables another finding to reach higher impact |

---

## Attack Trees

### Goal A: Use a victim's signature inside a hostile strategy

```
GOAL: A hostile strategy signs with a victim's authority (e.g. Approve on its token accounts)
├── PATH A1: Wallet emptied earlier in the same transaction (H004 b/c) [CONFIRMED, accepted]
│   ├── STEP 1: Victim signs an attacker-composed transaction
│   ├── STEP 2: An earlier router instruction drains the victim to exactly 0
│   └── STEP 3: Later create_market lists the victim; is_fresh → signature kept
├── PATH A2: Fee payer at 0 by fee or "send max" (H004 a/d) [CONFIRMED, accepted]
│   └── LEAF: Zero-SOL wallet at the moment the router looks
├── PATH A3: Integrator's authority PDA signed via CPI (H030) [CONFIRMED, accepted]
│   ├── STEP 1: Integrator hand-builds a signer meta for its own PDA
│   └── STEP 2: PDA has 0 lamports → signature kept
├── PATH A4: Creator's own signature (audit #1 H017) [RESOLVED — excluded by key]
└── PATH A5: Pre-funding to manipulate freshness (H039) [cleared — strips, never adds]

CRITICAL NODE: the rule that callers sign create_market only with new keypairs and never
pass their own PDAs as signers (documented). It breaks A1, A2 and A3. An on-chain node
(Instructions-sysvar check or an explicit signer list) was considered and not adopted.
```

All live paths are atomic: one transaction, signed by the victim or built by the integrator.

### Goal B: Take a trader's funds in a multi-hop route

```
GOAL: Extract value from a trader's multi-hop route
├── PATH B1: Sandwich hop 1, backfill from pre-held balance (H014) [CONFIRMED → FIXED]
│   ├── STEP 1: Trader holds the intermediate token in the same account
│   ├── STEP 2: Hop 1 has no minimum (old guidance)        ← now refused: per-hop minimum
│   └── STEP 3: Front-run / back-run around the route
├── PATH B2: Hold the route and wait for a bad price (H055) [cleared — bounded by each minimum]
└── PATH B3: Residual: top-up within the hop's own slippage tolerance [by design; optional
             mitigation: size the next hop from the previous hop's minimum]

CRITICAL NODE: per-hop min_amount_out (fixed in the docs).
```

This is a multi-transaction sequence (front-run, victim, back-run), usually in one block or bundle.

### Goal C: Swap through the router while paying less than 0.25%

```
GOAL: Under-pay Pipeline's fee on a router swap
├── PATH C1: Refill trader_input alone (hook / SyncNative) [cleared — max() charges vault_kept]
├── PATH C2: Move input out of the vault alone [cleared — max() charges trader_spent]
├── PATH C3: Both in one swap (H010) [POTENTIAL, accepted]
│   ├── STEP 1: Strategy forwards kept input out of its declared vault mid-call
│   ├── STEP 2: Output-mint hook refills a legacy-SPL trader_input mid-call
│   └── STEP 3: Router charges amount_in − min(R, F)
└── PATH C4: Skip the router and call the strategy directly [accepted design]

CRITICAL NODE: the strategy keeping its input in its declared vault until the call returns.
C4 is always cheaper for a cooperating strategy, so closing C3 in the router would not raise
the fee floor.
```

### Goal D: Block registration for others

```
GOAL: No new strategy can be registered
├── PATH D1: Recipient set to a write-protected key (H027) [CONFIRMED, accepted — admin only]
├── PATH D2: Listing fee set to the 1,000 SOL cap [accepted admin power]
├── PATH D3: Pause [ends at freeze; lift_pause — audit #1 H007 RESOLVED]
└── PATH D4: Third party, no authority (H036) [cleared — no permanent lever]

CRITICAL NODE: the admin key. Every live path needs it.
```

### Critical Fix Nodes (Summary)

| Node | Attack paths broken | Status |
|------|---------------------|--------|
| Per-hop `min_amount_out` in the guidance | B1 (and B2's amplification) | Done |
| "Sign `create_market` only with new keypairs; don't pass your own PDAs" | A1, A2, A3 | Documented |
| Keep kept input in the declared vault during the call | C3 | Optional strategy-guide rule |
| Admin key custody (multisig) and freeze runbook | D1, D2 | Operational |

---

## Severity Re-Calibration

**No re-calibration required — the severities are internally consistent.** Each finding keeps the investigator's rating. Reasons:

| Finding | Severity | Why it stays |
|---------|----------|--------------|
| H014 | MEDIUM | The router has no code defect. But integrators were told to build routes this way, and the loss to a victim is real and silent. The knowledge base rates missing slippage protection on a DEX with MEV exposure as MEDIUM or higher; this is the multi-hop form. It is not chained with any other finding. |
| H004 | LOW | Same class and preconditions as audit #1 H017 (LOW): the victim must sign an attacker-composed transaction, and the balance must be exactly zero. |
| H030 | LOW | Same predicate and impact class as H004. It needs a deliberate, hand-built signer meta by the integrator. Rated the same as H004 for consistency (CVSS 3.1 each). |
| H024 | LOW | A doc mismatch with a concrete loss, but only to a third-party strategy's own pool (accepted design). It does not affect the router's guarantees. LOW rather than INFO because a strategy following the docs literally would lose money. |
| H027 | LOW | Admin only and reversible. It could be argued down to INFO under accepted admin trust. It stays LOW because it contradicts the stated liveness property ("nobody can permanently block registration") if the admin key is lost. |
| H010 | LOW (POTENTIAL) | Fee under-collection only. It needs a forwarding strategy plus an output-issuer refill on a legacy-SPL input. There is a cheaper accepted alternative (direct calls). |

No finding takes part in an attack tree that reaches higher impact, so no chain-based increase applies. The knowledge-base false-positive patterns were checked: Anchor `Account<T>` owner and discriminator checks, runtime reentrancy rules, `remaining_accounts` with signer stripping, and `invoke_signed` with router PDAs. None of the six findings rests on one of those patterns.

Ratings changed earlier in the audit, not in this report: during investigation, 13 Tier 3 ratings from the lightweight model were re-verified, and all 13 were reclassified NOT VULNERABLE, keeping their informational notes. Among them were H074 (POTENTIAL → NOT VULNERABLE, INFO) and H051 (a LOW estimate → INFO).

---

## Requires Manual Expert Review

None. No finding had confidence below 7/10 or was left undetermined.

---

## Recommendations

No fix is outstanding for any confirmed or potential finding (see [Resolution](#resolution)). What remains are follow-ups. Items marked *(optional)* are improvements, not requirements.

**Status at publication (2026-11-09):** every follow-up below is done except the freeze runbook, which is an operational step for when the router is frozen. The three router-hardening ideas at the end were considered and not adopted.

### Before mainnet (recommended)

1. [x] **G001 — sBPF stack-frame check.** *Done: the router build reports no stack-offset warnings.* Look in the `cargo build-sbf` output for `Stack offset ... exceeded max offset of 4096` warnings on `pipeline::` symbols. If any frame is flagged, add a LiteSVM test that drives that path with the largest arguments (every optional account, a long hook tail, non-empty `extra`) and asserts that it succeeds. A failure here would be a deterministic revert, not a loss of funds.
2. [ ] **Freeze runbook (H046, H047).** *Recorded as the freeze runbook; it applies when the router is frozen.* Before freezing, replace the `MAX_LISTING_FEE_LAMPORTS` placeholder (`lib.rs:28-30`) with the chosen cap. Freeze with `SetAuthority(None)`, not a burn key, and run `lift_pause` in the same transaction.
3. [x] **G002 — default-frozen and NonTransferable mints.** *Done (`sos2_g002.rs`): only the issuer's own token's markets are affected, every failure reverts cleanly, and other markets trade normally.* Add `crates/pipeline-tests/tests/sos2_g002.rs`. Show that `init_fee_account`, `create_market` and `register_existing_market` still work. Show that swaps on such a market revert atomically, that `sweep_fees` has no effect, and that other markets are unaffected.
4. [x] **G003 — heap ceiling.** *Done (`sos2_g003.rs`): about 28 hook accounts fit the default 32 KiB heap; beyond that the transaction aborts and reverts with nothing moved. The integrator guide now says to add `RequestHeapFrame` for longer lists.* Add `crates/pipeline-tests/tests/sos2_g003.rs`. Record the largest number of remaining accounts plus hook accounts that a swap handles within the 32 KiB heap, and check that going beyond it fails cleanly. Publish the figure in the integrator guide.

### Documentation *(optional)*

5. [x] Mirror the new-keypair rule in `docs/INTEGRATOR-GUIDE.md` §6 ("Creating markets"): sign only with new keypairs, and don't pass your program's own PDAs as signers. That section currently says only that the creator's signature never reaches the strategy. (H004, H030)
6. [x] Add a strategy-guide rule: keep the input you keep in your declared vault until `pipeline_swap` returns, and refund only from it. (H010)
7. [x] Multi-hop: note that sizing each later hop from the previous hop's *minimum* rather than its quote also removes the slippage-sized top-up. When the SDK gets a multi-hop builder, it should set per-hop minimums by default and have a test for the sandwiched-with-pre-held-balance case. (H014)
8. [x] Note in the integrator guide that transfer-fee mints tax Pipeline's fee and sweep legs. (H076)
9. [x] Note that `SwapEvent.integrator` is set by the trader and does not prove a fee was paid. (H074)
10. [x] Correct the three wordings listed under H080, and change "judged before the creator pays anything" to "judged at the start of this instruction". (H004, H080)
11. [x] Short notes in the strategy guide on:
    - pre-funding grief (H039);
    - the permissionless `["pipeline-register"]` PDA path (H051);
    - public-seed markets going to the first creator (H067);
    - checking the account type in `market_info` (H052).

### Test hardening *(optional)*

12. [x] Add a short LiteSVM test for output-side aliasing: `trader_output` equal to the strategy's output vault, with `min_amount_out = 0`. Today this is settled only by code reading. (H075)
13. [x] Strengthen the BOK drift guard so it compares constant values, not just the presence of source fragments, or extract the router's pure helpers so the property tests run on the real code. (H065; BOK recommendation 3)

### Router hardening, considered and not adopted *(optional)*

- Reject reserved or executable `listing_fee_recipient` values, or don't require the recipient to be writable at fee 0. (H027)
- Check the Instructions sysvar in `create_market`, or take an explicit list of accounts allowed to keep their signature. (H004, H030)
- Have the strategy report its refund, or let a market declare extra measured input sinks. (H010)

### Ongoing practice

- Re-run this audit (stacked) on any change to the three in-scope files, and keep the Kani and Z3 proofs in CI.
- Audit Pipeline's own strategies separately before they go to mainnet.
- Rebuild the router binary used by the tests whenever the router changes, so the tests exercise the code that ships.

---

## Investigated & Cleared

74 hypotheses were investigated and found NOT VULNERABLE. Most are backed by a LiteSVM test. The rest are settled by code reading, as each finding file states.

<details>
<summary>Click to expand cleared items (74 total)</summary>

| ID | Hypothesis | Why safe |
|----|------------|----------|
| H001 | Freeze-aware pause: `router_data` binding, every pause gate, ordering (recheck of audit #1 H007) | `router_data` is bound to the router's real ProgramData; pausing needs an upgrade authority; `lift_pause` clears a pause once frozen. Fix complete |
| H002 | Pause outlives upgradeability via loader-v4 migration | The premise holds in code, but the v3→v4 migration was removed from the runtime (Agave 4.3+); the state can't be reached |
| H003 | `create_market` never forwards the creator's signature: aliases and duplicates (recheck of audit #1 H017) | Freshness decided before any payment; creator excluded by key, including duplicates. Fix complete |
| H005 | Listing-fee payer maximum checked against the value actually transferred (recheck of audit #1 H023(a)) | `max_listing_fee_lamports` compared with the fee actually charged; `ListingFeeBelowRent` added. Fix complete |
| H006 | `sweep_fees` fee-authority signature vs a caller-chosen hook tail | Hook-tail metas are unsigned; Token-2022 drops signer privilege for hook accounts; fee ATAs are pinned per market and mint |
| H007 | Hook-tail signer stripping for co-signers in all four router transfers | `token.rs` builds every tail meta with `is_signer = false`; the authority is the only signer meta |
| H008 | In `swap` the strategy receives exactly one signature | Fixed metas: market PDA signs; trader read-only and unsigned; remaining accounts via `meta_like(_, false)` |
| H009 | `settle` is fed the right balances | Snapshot → transfer → CPI → re-read with `checked_balance`; input-side aliases are refused or harmless |
| H011 | Cross-market isolation | Market PDA seeds include strategy and state; vaults, mints and fee ATAs re-bound per swap |
| H012 | `swap` account-binding substitution matrix | Every substituted account fails an Anchor or router check |
| H013 | `create_market` accepts an existing strategy-owned `market_state` | No second record; re-initialisation would need a strategy bug; attribution is first-come anyway |
| H015 | Reentrancy closed on every path | The runtime forbids A→B→A; the router keeps no state across its CPIs |
| H016 | Type confusion in `checked_balance` / `balance` / `decimals` | Owner, length (≥165, ≠355), type byte and mint checked; `decimals` fails safe in `transfer_checked` |
| H017 | Strategy relays its market-PDA signature into another strategy | That PDA is never the one another strategy expects (different seeds) |
| H018 | Unmeasured window after `settle` during the fee transfers | While Token-2022 is on the stack, a hook can't move Token-2022 balances; remaining moves are bounded and accepted |
| H019 | `register_existing_market` first write wins, no correction path | Accepted design; record keyed by (strategy, state); swaps re-check vaults |
| H020 | Issuer's hook as an in-transaction actor while the strategy is off the stack | Accepted issuer power over its own mint; no reach into other markets |
| H021 | Duplicating router accounts in remaining accounts widens privileges | The market PDA can become a writable signer, but it owns nothing the strategy could misuse |
| H022 | Setup-payer funnel vs assign-and-return, partial spends, pre-funding | Post-check: system-owned and empty, then a full refund; creator's spend capped at `max_creator_lamports` |
| H023 | `fetch_market_info` return data only from the strategy | Return-data program ID must equal the strategy; plain `invoke`, unsigned read-only metas |
| H025 | Fee custody in `sweep_fees` | Source and treasury ATAs re-derived; only the market's fee PDA signs |
| H026 | Registrar binding: loader-v3 parsing in `check_registrar` | Owner checks on Program and ProgramData, address cross-check, exact field parsing (property-tested) |
| H028 | Fee completeness on the generic path after the `settle` refactor | Pre-deposit, donation, dust splitting, exact-out and multi-hop all pay at least the proven floor |
| H029 | Event authenticity | Only the router's event-authority PDA can sign `emit_cpi!`; strategies and hooks can't forge events |
| H031 | Native SOL: `SyncNative` and unsynced lamports | Router reads `amount` only; a refill needs an in-window actor (same as H010) |
| H032 | Declared vault can be a router-owned account | Fee-authority PDA never signs for the strategy; aliasing harmless (BOK SWAP-13) |
| H033 | Token-2022 / SPL Token binaries tested vs mainnet | Tests run p-token and Token-2022 v11; router strips tail signers itself, so it doesn't depend on the token program dropping them |
| H034 | Loader-v4 strategies can't register once the router is frozen | Refused cleanly (`UnsupportedLoader`); loader-v4 is abandoned on current clusters |
| H035 | Admin power inventory after the freeze | Three `Config` writers; no admin power over funds, strategies or markets |
| H036 | Liveness: third-party permanent blocks | Pre-funding and deposits block nothing; Anchor `init` and `CreateIdempotent` handle pre-funded addresses |
| H037 | Strategy closes and re-creates its vault inside the call | Post-call `checked_balance` re-validates owner, length, type and mint |
| H038 | Closed keypair vault re-created; immutable `Market` points at it | Only that strategy's own market is affected; the trader is still protected by measured min-out |
| H039 | Pre-funding a pending fresh signer strips its signature | Grief only; retryable; reduces privilege — INFO |
| H040 | One hook tail serves three transfers | Unsigned tail; each destination bound separately |
| H041 | Heap and compute exhaustion from long lists, hook tails, `extra` | Limits apply per transaction; only the oversized transaction fails (ceiling: G003) |
| H042 | CPI depth budget | Measured against the runtime's limit of 5; documented depth holds |
| H043 | Fee ATA has no repair path under issuer actions | Fails closed; issuer powers over its own mint |
| H044 | `Market` record immutable and goes stale | Vaults pinned per swap; stale cases reach only that market |
| H045 | `initialize_config`: binding, once, before freeze | Requires the router's upgrade authority; `init` PDA |
| H046 | "Freeze" to a burn key keeps the pause power | Matches the definition of frozen — INFO (runbook) |
| H047 | Listing-fee cap placeholder permanent at freeze; rent floor checked only when set | Bounded and recoverable — INFO |
| H048 | Pause-gate coverage matches the docs | Exactly the four documented instructions are gated |
| H049 | PDA seed spaces can't collide; canonical bumps | Distinct prefixes and fixed-length parts; stored canonical bumps |
| H050 | Registering the router or a non-strategy program | Needs that program's authority or its PDA; inert if done |
| H051 | `["pipeline-register"]` PDA path and permissionless signing | Only the program's own code can sign; no stored privilege — INFO |
| H052 | Any strategy-owned account registered as a market | Ownership plus the strategy's own type check — INFO |
| H053 | `metadata_uri` bounds and update semantics | ≤200 bytes on both writers; space allocated for the maximum |
| H054 | `interface_version` checked only at registration | No later instruction depends on it |
| H055 | No deadline; durable-nonce swaps valid indefinitely | Still bounded by the signed `min_amount_out` and `max_total_spend` |
| H056 | Exact-out behaviour | Measured settlement; refunds lower the base; trader protected by min-out |
| H057 | Router-level edges of `settle` | Every edge refused with an error; whole transaction reverts |
| H058 | `MarketInfo` structural validation gaps | Gaps reach only the declaring market; every swap re-checks |
| H059 | `SwapEvent` / `SwapResult` semantics vs docs and SDK | Fields match the measured values |
| H060 | Other events: no-op `ConfigChanged`, front-runnable `created_by`, silent init | No protected property depends on them |
| H061 | Return data when the router is a CPI callee | `set_return_data` is the last action in `swap` |
| H062 | Hook-program identity edge cases | Token-2022 on the stack blocks re-entry; tail unsigned |
| H063 | Account-level Token-2022 extensions | Only the account owner can enable them; the router enables none |
| H064 | Known-vulnerable dependencies | One advisory, in a crate path not reachable in the on-chain router |
| H065 | BOK drift guard coverage | Weak guard, but it supports test copies, not the router (note above) |
| H066 | Legacy SPL vs p-token measurement edges | Multisig authority refused (355 bytes); self-transfer measured as zero |
| H067 | No creator identity at init; public-seed markets first-come | Router never forwards the creator; per-strategy seed design — INFO |
| H068 | Authorities and lamports given to the setup PDA are locked | Reaches only the strategy's own accounts and the capped allowance |
| H069 | `init_fee_account` relies on ATA derivation | The ATA program derives and validates the address; idempotent |
| H070 | Anchor duplicate-mutable gaps | Each aliasing pair reviewed; none breaks a guarantee |
| H071 | Composing router instructions leaks no state | No static or transient state; `swap` writes no router account |
| H072 | Wire format and discriminators | Match the docs; no collisions |
| H073 | No account-passed sysvars, no instruction introspection | Confirmed by grep |
| H074 | Integrator fee account semantics and bounds | Money paths sound; attribution field cosmetic — INFO |
| H075 | Output-side aliasing | Min-out on measured delta; strategy only harms its own vault (code reading) |
| H076 | Transfer-fee mints: issuer captures Pipeline's fee | Issuer power over its own mint; isolated — INFO |
| H077 | Trader as delegate of `trader_input` | Trader signs; spend bounded by `amount_in` and the delegated amount |
| H078 | Unmeasured trader-owned accounts passed to the strategy | Passed unsigned; strategy can't debit them without the trader's signature |
| H079 | Rent-state transitions in `create_market` and `register_strategy` | Fixed-size `init` and capped transfers; no third-party rent trap |
| H080 | Market-PDA and fee-PDA wording in docs | Wording stronger than the code; no capability — INFO |

</details>

---

## Audit Coverage

### Files Analyzed

| File | LOC (production) | Focus areas | Findings |
|------|------------------|-------------|----------|
| `programs/pipeline/src/lib.rs` | 1,343 | all nine | H004, H010, H024, H027, H030 |
| `programs/pipeline/src/token.rs` | 168 | access control, CPI, token, account validation | — (cleared: H007, H016, H066) |
| `crates/pipeline-strategy-interface/src/lib.rs` | 409 (excluding the test and Kani modules) | arithmetic (BOK-proven), token, economic | H010 (`settle` inputs), H014 (per-hop check), H024 (doc comment) |

All three in-scope files changed since audit #1. Context only (not audited): the strategies, the test programs, `docs/`, `sdk/ts`, `crates/pipeline-client` and `crates/pipeline-quoter`.

### Analysis Depth by Area

| Focus area | Hypotheses | Confirmed / Potential |
|------------|-----------|----------------------|
| Access Control | 11 | H004, H030 |
| Account Validation | 7 | — |
| Arithmetic (inputs to proven maths) | 3 | H024 |
| State Machine | 11 | — |
| CPI & External | 11 | — |
| Token & Economic | 14 | H010 |
| Oracle & Data | 6 | — |
| Upgrade & Admin | 10 | H027 |
| Error Handling | 3 | — |
| Timing & Ordering | 4 | H014 |

### Coverage Verification 

| Dimension | Result |
|-----------|--------|
| Instructions | 11/11 entry points (10 router instructions and Anchor's event-CPI entry), each with a primary hypothesis |
| Exploit patterns | 54/70 applicable patterns cited; 13 more covered in substance; 2 unexamined (EP-086 stack overflow → G001; EP-057 NonTransferable → G002) |
| Playbooks | AMM red flags 10/10 applicable; Token-2022 checklist 13/15 full, 2 partial (DefaultAccountState, NonTransferable → G002). Combined 23/25 |
| Gaps | No CRITICAL or HIGH gap. G001 MEDIUM; G002 and G003 LOW |

**Depth notes.** Four security claims rest on code reading only, with no test: H063, H070, H075 and H078. The heap estimate in H041 is also code reading (G003). 23 of the 80 hypotheses were settled by code reading. Most of those are INFO or documentation items.

**Test binary.** The PoC tests ran against the prebuilt router binary. That binary predates commit `44d9a1b`, which rewrote `ceil_bps` in 64-bit arithmetic. The rewrite is proven equivalent for every input (Z3), so the tested behaviour is the shipped behaviour.

---

## Methodology

This audit used the Stronghold of Security methodology, stacked on audit #1.

1. **Scan and index.** The three in-scope files (1,920 LOC) were indexed. A static pre-scan flagged 541 hot spots (218 high, 323 medium). Semgrep ran 10 of 12 SOS rules (2 failed to parse; grep covered them).
2. **Parallel context building.** Nine context analyses: access control, arithmetic, state machine, CPI, token/economic, oracle/data, upgrade/admin, timing/ordering, and a DeFi economic-model analysis of fees, multi-hop, integrator and listing fees. All passed the quality gate.
3. **Synthesis.** The analyses were merged into `an architecture document`. The handover from audit #1 supplied three RECHECK items and the lineage of dropped dismissals. All in-scope files had changed, so there were no verification-only passes.
4. **Strategy generation.** 80 hypotheses (14 Tier 1, 24 Tier 2, 42 Tier 3; 36 novel). Each is written as a verification check: the property, the code, when it might fail, and the test that settles it. The pure arithmetic was treated as verified (BOK) and not re-proven.
5. **Investigation.** 14 batches in priority order. Each hypothesis was investigated against the code and, where possible, settled by a new LiteSVM test in `crates/pipeline-tests/tests/sos2_<id>.rs`. Existing source and tests were not edited. Each test asserts the *safe* behaviour. At investigation time, 52 test files ran with 221 tests passing and 7 failing as intended: those 7 demonstrated H004 (3), H014, H024, H027 and H030. After the resolutions, these tests were rewritten to pin the fixed or accepted behaviour. Thirteen Tier 3 ratings were re-verified by a second model. No supplemental strategies were needed, because batch 1 found nothing.
6. **Coverage verification.** Exploit-pattern, playbook and instruction coverage were checked (`the audit's coverage check`), producing gap hypotheses G001–G003.
7. **Final synthesis.** The pairwise combination matrix, attack trees, severity check and this report.

**Formal verification input.** The Book of Knowledge run ([`VERIFICATION.md`](VERIFICATION.md)) covers 47 invariants with 0 violations. The fee maths, `settle`, `hook_split` and `credited_input` are proven for every input: Kani over the full domain, or Z3 over the integers in `crates/pipeline-strategy-interface/proofs/fee_maths_z3.py`. Account-level properties pass LiteSVM tests (`crates/pipeline-tests/tests/bok_*.rs`). BOK says nothing about account binding, CPI privilege, Token-2022 behaviour or ordering. Those were this audit's focus.

---

## Disclaimer

This automated audit is a thorough starting point. It does not guarantee that the code has no vulnerabilities.

**It does not replace:**
- a manual review by security experts;
- comprehensive test coverage of future changes;
- a bug bounty programme;
- ongoing monitoring of the deployed program.

**Limitations:**
- The audit covers the three router files at `73e5ca0`. Strategies (including Pipeline's own strategies), SDKs, services and apps were not audited.
- All testing was local (LiteSVM). Mainnet token-program binaries were compared by reading, not on chain (H033).
- Some claims rest on code reading, as each finding states.
- The project's accepted-design choices shape what counts as a vulnerability. A reader with a different threat model should review the "Accepted design" list.
- New issues can appear with any code change.

No external audit of the router is planned. This report is published with the code so outside developers can review it.

---

## Audit Lineage

| # | Date | Git Ref | Confirmed | Potential | Files Scanned | Notes |
|---|------|---------|-----------|-----------|---------------|-------|
| 1 | 2026-11-08 | `f63d0ef` | 8 | 1 | 8 | Router plus Pipeline's own strategies |
| 2 | 2026-11-09 | `73e5ca0` | 5 | 1 | 3 | Router only; 3/3 in-scope files changed since #1 |

---

## Finding Evolution

### Evolution Summary

| Classification | Count | Description |
|----------------|-------|-------------|
| NEW | 6 | First reported in this audit: H004, H010, H014, H024, H027, H030 |
| RECURRENT | 0 | — |
| REGRESSION | 0 | — |
| RESOLVED | 3 | Audit #1 router findings H007, H017, H023(a); fixes verified by H001, H003, H005 |
| Out of scope | 6 | Audit #1 findings in Pipeline's own strategies; not rechecked here |

### New Findings

| ID | Title | Severity |
|----|-------|----------|
| H014 | Multi-hop guidance left earlier hops without a minimum | MEDIUM |
| H004 | Freshness judged per instruction | LOW |
| H030 | Program authority PDAs look fresh | LOW |
| H024 | Exact-in `trade.amount` is gross | LOW |
| H027 | Admin can block registrations through the recipient | LOW |
| H010 | Fee base with forwarding strategies plus outside refills | LOW (POTENTIAL) |

**H004 classification.** H004 is NEW, but related to audit #1 H017. It is not RECURRENT and not a REGRESSION:
- Audit #1 H017 was about *this instruction's creator* being emptied by the router's own payment and then looking fresh. The fix (judge freshness before payment; exclude the creator by key) holds. H003 tested it with aliases and duplicates.
- H004 concerns *other* signers emptied *earlier in the transaction*. Audit #1 did not report that case, and the H017 fix was never meant to cover it.
- The same `is_fresh` predicate underlies both, but no case that audit #1 fixed has broken. So there is no regression and no severity increase.

**H010 lineage.** Audit #1 cleared the refill attack on the fee base and noted, in passing, that forwarding strategies depend on keeping input in the declared vault. This audit is the first to test that dependency and report it as a finding.

### Recurrent Findings

None.

### Regressions

None.

### Resolved Findings

| ID (audit #1) | Title | Original Severity | Resolution | Verified by |
|---------------|-------|-------------------|------------|-------------|
| H007 | The pause power survives a router freeze | MEDIUM | Fixed in code: `update_config` refuses `paused = true` once the upgrade authority is `None`; anyone can call `lift_pause` once frozen | H001 (complete); H002 (loader-v4 path unreachable); H046 (burn-key note) |
| H017 | `create_market` drains the creator so it looks fresh and its signature reaches the strategy | LOW | Fixed in code: freshness judged before the creator pays; the creator's key never keeps its signature | H003 (complete, including aliases and duplicates) |
| H023(a) | `register_strategy` has no payer maximum on the listing fee | LOW | Fixed in code: `max_listing_fee_lamports` argument (`ListingFeeAboveMaximum`), plus `ListingFeeBelowRent` | H005 (complete; checked against the value actually transferred) |

The Pipeline's own strategies are out of scope for this audit, so their audit #1 findings were carried for lineage only and not rechecked.

---

## Report Metadata

| Field | Value |
|-------|-------|
| Report generated | 2026-11-09 |
| Stronghold of Security version | 1.0.0 |
| Audit number | #2 |
| Audit ID | `d5ab499f-86d4-4ef2-9e90-0d6e9aa24119` |
| Git ref | `73e5ca0` (`73e5ca092a3522689dcb3fe4064027d9c168a4df`) |
| Previous audits | 1 (#1, 2026-11-08, `f63d0ef`) |
| Tier | deep (user-selected: pre-mainnet, report published with the code) |
| Files in scope | 3 (1,920 LOC) |
| Hypotheses investigated | 80 (supplemental: 0) |
| Investigation batches | 14 |
| Context analyses | 9 |
| PoC test files | 52 |
| Coverage gaps | 3 (G001–G003) |
| Models | index: haiku · context analysis: opus · quality gate: haiku · strategy: opus · investigation: Tier 1 opus, Tier 2 sonnet, Tier 3 haiku (re-verified by sonnet) · coverage: sonnet · report: opus |
| Cross-skill input | Book of Knowledge, 47 invariants, 0 violations ([`VERIFICATION.md`](VERIFICATION.md)) |

---

**End of Report**
