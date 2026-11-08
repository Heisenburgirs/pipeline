# Integrating Pipeline (desks, terminals, bots, indexers)

Integrate once and every Pipeline market, including ones created after you integrate, is discoverable, quotable, tradable and chartable with no per-market or per-strategy code.

Libraries:

| Library | Language | Purpose |
|---|---|---|
| `pipeline-client` | Rust | Addresses, instruction builders, account decoders |
| `pipeline-events` | Rust | Event decoding |
| `pipeline-quoter` | Rust | Embedded-VM quoting |
| `sdk/ts` | TypeScript | The same for TypeScript |

All are Apache-2.0; package names are placeholders until first publish. Router program: `PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34`.

## 1. Discover strategies and markets

- **Accounts:** `getProgramAccounts` on the router, filtered by the 8-byte Anchor discriminator:
  - `Strategy`: `sha256("account:Strategy")[..8]`. Holds the program ID and metadata URL.
  - `Market`: `sha256("account:Market")[..8]`. Holds the strategy, market state, mints, vaults and exact-out support.

  Decoders: `pipeline_client::decode_strategy` / `decode_market`, or the TypeScript SDK.
- **Events:** subscribe to the router and decode `MarketCreated` and `StrategyRegistered` (section 4) to learn about new ones as they appear.
- **Metadata:** fetch the strategy's metadata URL for its name, image, links and custom-action forms (see `docs/STRATEGY-GUIDE.md` section 7). Treat metadata as untrusted user content.

## 2. Quote

Two options, from fastest to simplest:

1. **`pipeline-quoter`, embedded.**
   - Load each strategy's program (from its ProgramData account) and keep the market's accounts updated from your stream.
   - `Quoter::quote` runs the strategy's real `pipeline_quote` in a sandboxed VM and applies Pipeline's fee, your integrator fee and Token-2022 transfer fees: about 10–20 µs for simple strategies.
   - `Quoter::simulate_swap` simulates the whole router swap for exact numbers with any token behaviour.
   - Reload a program when its ProgramData changes, since strategies can be upgraded.
2. **RPC simulation** of `pipeline_quote` (TypeScript SDK `quoteStrategy`): no infrastructure, 50–300 ms.

**Before quoting a new market, resolve its accounts** with `pipeline_resolve_accounts` (the quoter's `resolve_accounts`, or the SDK). This tells you which strategy-specific accounts to cache and to include in swaps.

**Keep the quoter's clock current** with `set_clock(slot, epoch, timestamp)`; the epoch decides which Token-2022 transfer fee applies.

**Pass the trader** if you know them. Some strategies price by trader, and anonymous quotes may differ from what the trader actually gets.

## 3. Build a swap

Use `pipeline_client::swap` (or the SDK) with:

- the trader, market state, the trader's input and output token accounts, the market's input vault (`Market::vaults`), both mints and their token programs;
- **strategy accounts**: the result of account resolution, then any transfer-hook accounts for the **output** mint;
- **input hook accounts**: the transfer-hook accounts for the **input** mint, if it has a hook;
- optionally, your integrator fee account (a token account in the input mint) and fee rate.

Arguments:

| Mode | `amount_in` | `amount_out` |
|---|---|---|
| Exact-in | The amount sent to the strategy | Ignored |
| Exact-out (if the market supports it) | The most to send | The strategy-level output from the quote |

**`min_amount_out` is always what must arrive in the trader's output account.** Set it from the quote minus your slippage tolerance. The router enforces it independently of the strategy.

**What the trader pays:** the router charges `ceil(net_sent × 0.25%)` plus your fee on top of the input actually kept by the strategy. The most a swap can cost is `pipeline_strategy_interface::fees::max_total_spend(amount_in, integrator_fee_bps)`. To spend at most a budget, use `fees::amount_for_budget`.

**Multi-hop:** put several `swap` instructions in one transaction. Each hop pays 0.25%. Size each later hop from the previous hop's output with `amount_for_budget`, and put the trader's end-to-end minimum on the last hop.

**Before the first swap that sells a mint in a market,** that market's fee account for the mint must exist. Add `init_fee_account` (idempotent) to the transaction if needed.

**Native SOL:** wrap into a WSOL token account before the swap and close it after, as usual.

## 4. Index trades and build charts

Every swap emits one `SwapEvent` as a router self-CPI, so it is not lost to log truncation.

- **Find it:** the event appears in the transaction's inner instructions, as an instruction to the router whose data starts with Anchor's event tag. Decode it with `pipeline_events::decode`.
- **Fields:** market, strategy, trader, input and output mints, mode, `net_sent` (input the swap was charged for, excluding fees), `received` (output that arrived), `pipeline_fee`, the integrator account and fee, slot and timestamp.
- **Price** is `received / net_sent` in the mints' units. Adjust for decimals; Token-2022 transfer fees are already reflected in both numbers.
- **Volume** is `net_sent` in the input mint.

The same decoder handles `MarketCreated`, `StrategyRegistered`, `MetadataUpdated`, `FeesSwept` and `ConfigChanged`. Trades made by calling a strategy directly, bypassing Pipeline, do not produce Pipeline events.

## 5. What to show users

Show neutral facts and no judgements:

- whether the strategy program is upgradeable or frozen;
- mint and freeze authorities;
- liquidity (vault balances);
- market age;
- volume.

Pipeline does not vet strategies.

## 6. Limits worth knowing

- **Restrictive transfer hooks.** A mint whose hook rejects transfers into the market's fee account cannot be sold through Pipeline, though it can be bought.
- **Frozen fee accounts.** Mints whose accounts start frozen (Token-2022 DefaultAccountState), or whose freeze authority freezes a market's fee account, cannot be sold through that market.
- **CPI Guard.** Traders with Token-2022 CPI Guard enabled on their input account cannot swap through the router, because it moves their tokens in a cross-program call.
- **Permanent delegates.** A mint's permanent delegate can move Pipeline's collected fees in that mint, so they should be swept often. Traders are protected: the router never lets a swap take more than `amount_in`.
- **Creating markets.** A strategy's setup never receives the creator's signature. It spends from a router payer funded with `max_creator_lamports` (unused SOL is returned), so set that to the rent you expect.
- **Exact-out with transfer-fee input tokens.** Unused input refunded by the strategy pays the token's transfer fee again, and Pipeline's fee is measured on the trader's real loss. Keep `amount_in` close to the quote for such tokens.
- **Call depth.** Calls nest at most 4 levels. A trader using a smart wallet, combined with a hooked token and a strategy that calls another protocol, can exceed it.
- **Compute.** A typical swap costs 40–55k compute units in total. Strategies vary.
