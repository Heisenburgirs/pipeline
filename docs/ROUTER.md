# How the Pipeline router works

The router is one Anchor program, `pipeline`, at `PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34`.
It keeps a registry of strategy programs and their markets, moves a trader's input into a
strategy, calls the strategy, measures what happened and charges a 0.25% fee. Strategies
hold their own liquidity; the router never custodies it.

## Accounts

| Account | Seeds | Contents |
|---|---|---|
| `Config` | `["config"]` | `admin`, `paused`, `listing_fee_lamports` (at most `MAX_LISTING_FEE_LAMPORTS`), `listing_fee_recipient`, `treasury_owner` (owner of the treasury token accounts that receive swept fees) |
| `Strategy` | `["strategy", program_id]` | `program_id`, `metadata_uri` (up to 200 bytes), `interface_version`, `registered_at`, `market_count` |
| `Market` | `["market", program_id, market_state]` | `strategy_program`, `market_state` (an account the strategy owns), `mints` (2–4), `vaults` (where input in each mint goes), `supports_exact_out`, `created_by`, `created_at`. The market PDA also **signs the router's calls that set up or trade the market** (`pipeline_init_market`, `pipeline_swap`; the read-only `pipeline_market_info` isn't signed), so a strategy can check a call came from Pipeline for that exact market |
| `fee_authority` | `["fees", market]` | No data. Owns the market's fee accounts. **Never signs during a swap or market setup** (a trader may list it, unsigned, among the strategy's accounts; it signs only sweeps to the treasury), so nothing a strategy receives can move fees |
| `setup_payer` | `["setup", market]` | No data. Holds the creator's `max_creator_lamports` while a strategy sets up a market, and is emptied again in the same instruction |
| Fee accounts | Associated token account of the market's `fee_authority`, one per mint | The 0.25% fees collected in that mint. Created by `init_fee_account` (anyone can pay) |

The 0.25% fee (`PIPELINE_FEE_BPS = 25`) and the listing fee cap are compiled into the
program.

## The strategy interface

A strategy is any program that implements these instructions. Their discriminators are
`sha256("global:<name>")[..8]`, so an Anchor function with the same name matches, and
native programs can implement the bytes directly. Types, discriminators and helpers are in
the `pipeline-strategy-interface` crate.

| Instruction | Called by | Purpose |
|---|---|---|
| `pipeline_init_market(data)` | The router, in `create_market` | Set up a market's state. `data` is strategy-specific |
| `pipeline_market_info()` | The router, in `create_market` and `register_existing_market`; read-only | Returns `MarketInfo { mints, vaults, supports_exact_out }` |
| `pipeline_resolve_accounts(TradeRequest, round)` | Quoters and SDKs, off-chain; read-only | Returns the accounts a trade needs beyond the fixed ones, and whether the list is complete |
| `pipeline_quote(TradeRequest)` | Quoters and SDKs, off-chain; read-only | Returns `QuoteResult { amount_in, amount_out }`: what `pipeline_swap` would do now |
| `pipeline_swap(SwapRequest)` | The router, in `swap` | Deliver output, and optionally refund unused input |

`TradeRequest` holds the mode (`ExactIn` or `ExactOut`), the input and output mint indices,
the amount, the trader (`None` for anonymous quotes) and strategy-specific `extra` bytes.
`SwapRequest` adds `amount_sent` (what the router moved into the vault) and
`min_amount_out`.

Accounts for `pipeline_swap` start with a fixed prefix, followed by the strategy's own:

0. Market signer: the router's market PDA (signer)
1. Market state (writable)
2. Trader's wallet (**not** a signer)
3. Trader's input token account (writable)
4. Trader's output token account (writable)
5. Strategy input vault (writable)
6. Input mint
7. Output mint
8. Input token program
9. Output token program

`pipeline_quote` and `pipeline_resolve_accounts` take the same layout, read-only.

**The trader is never a signer in calls to the strategy.** A strategy can pay into the
trader's accounts but can't take from them.

Strategies should compute `pipeline_swap` with the same code as `pipeline_quote`, depend
only on the accounts passed, the instruction data and the Clock and Rent sysvars, and
measure the input they received from their own vault rather than trusting `amount_sent`.
The example strategy does all three; [`STRATEGY-GUIDE.md`](STRATEGY-GUIDE.md) explains why.

Anyone may call a strategy directly, bypassing Pipeline and its fee. That is allowed.
Custom actions (adding liquidity, claims and so on) are ordinary instructions on the
strategy, described in its metadata.

An optional sixth instruction, `pipeline_seed_liquidity`, lets another program create and
fund a market on a strategy in one call (for example, a launchpad graduating a token).
See [`STRATEGY-GUIDE.md`](STRATEGY-GUIDE.md) section 9.

## Router instructions

| Instruction | Who | Effect |
|---|---|---|
| `initialize_config` | The router's upgrade authority, once | Creates `Config` |
| `update_config` | `admin` | Admin, pause, listing fee (0, or between the rent-exempt minimum and the cap), fee recipient, treasury owner. Pausing needs the router to still be upgradeable |
| `lift_pause` | Anyone, once the router is frozen | Clears a pause left in place |
| `register_strategy(metadata_uri, interface_version, max_listing_fee_lamports)` | The strategy program's upgrade authority, or the program itself signing with its PDA `["pipeline-register"]` | Pays the listing fee (failing if above the caller's maximum) and creates `Strategy` |
| `update_metadata(uri)` | The same | Updates `metadata_uri` |
| `create_market(init_data, max_creator_lamports)` | Anyone | Funds the `setup_payer` with up to `max_creator_lamports`, calls `pipeline_init_market` with it as payer (the creator's own signature is never passed on), checks the payer is still a plain system account, refunds what's left, then calls `pipeline_market_info` and records the `Market` |
| `register_existing_market` | Anyone | Records a market the strategy created by itself |
| `init_fee_account` | Anyone | Creates a market's fee account for one mint, idempotently |
| `swap(...)` | The trader | Below |
| `sweep_fees` | Anyone | Moves a market's collected fees in one mint to the treasury's token account |

When paused, `swap`, `create_market`, `register_existing_market` and `register_strategy`
stop. Admin actions and `sweep_fees` keep working, and direct calls to strategies are
unaffected. The pause power ends when the router is frozen: a frozen router can't be
paused, and anyone can clear a pause left in place.

## A swap, step by step

1. **Check.** Not paused; the market and strategy match; both mints belong to the market;
   exact-out only if the market supports it; neither fee account is the trader's input.
2. **Snapshot** the trader's input and output balances and the market's input vault.
3. **Send input** with `transfer_checked`, signed by the trader: `amount_in` (exact-in) or
   the maximum input (exact-out).
4. **Call the strategy's `pipeline_swap`,** signed by the market PDA. It sends output to the
   trader and may refund unused input.
5. **Measure.**
   - `trader_spent` = the trader's input balance before minus after.
   - `vault_kept` = the input vault's balance after minus before.
   - `net_sent = min(amount_in, max(trader_spent, vault_kept))`. Other code can run during
     the strategy call (a transfer hook, for example) and could top up the trader's input
     account to make the spend look smaller; the vault belongs to the market. With a
     transfer-fee token the vault keeps less than the trader spent, and the trader's spend
     counts.
   - `received` = the trader's output balance after minus before.
   - Require `received ≥ min_amount_out`, `trader_spent ≤ amount_in`, and `net_sent > 0`
     (no output for no input, and no empty swaps).
   - This step is `pipeline_strategy_interface::settlement::settle`, a pure function, so
     quoters can share it and its guarantees can be proven for every input.
6. **Charge fees** from the trader: `ceil(net_sent × 0.25%)` into the market's fee account,
   plus an optional integrator fee at a rate the trader signs.
7. **Publish** a `SwapEvent` (via `emit_cpi!`, so log truncation can't lose it) and return
   `SwapResult { net_sent, received, pipeline_fee, integrator_fee }`.

The most a swap can cost is known before signing:
`amount_in + ceil(amount_in × 25 / 10,000) + ceil(amount_in × integrator bps / 10,000)`
(`fees::max_total_spend`; each fee rounds up on its own). Multi-hop routes are
several `swap` instructions in one transaction; each hop pays 0.25%. Give every hop its own
minimum (its quote less slippage), size each later hop from the previous hop's minimum
rather than its quote, and put the trader's end-to-end minimum on the last. A later hop
spends its `amount_in` from the trader's account whatever the earlier hop delivered, so
this way it can never dip into intermediate tokens the trader already held.

## Events

| Event | Fields |
|---|---|
| `SwapEvent` | `market`, `strategy`, `trader`, `input_mint`, `output_mint`, `mode`, `net_sent`, `received`, `pipeline_fee`, `integrator`, `integrator_fee`, `slot`, `unix_timestamp` |
| `StrategyRegistered`, `MetadataUpdated` | `strategy`, `fee_paid`, `metadata_uri` |
| `MarketCreated` | `market`, `strategy`, `market_state`, `mints`, `created_by` |
| `FeesSwept` | `market`, `mint`, `amount` |
| `ConfigChanged` | The changed field and value |

Decode them with `pipeline-events` (Rust) or the TypeScript SDK.

## Quoting

- **On-chain truth:** each strategy's `pipeline_quote`, sharing code with its `pipeline_swap`.
- **`pipeline-quoter`:** runs the strategy's deployed program in an embedded, sandboxed
  Solana VM on account data you stream, and applies the router's and Token-2022's fees on
  top. About 10–20 µs per quote for simple strategies.
- **RPC simulation:** simulate `pipeline_quote` through any RPC node (the TypeScript SDK's
  `quoteStrategy`). No infrastructure; 50–300 ms.

`pipeline_resolve_accounts` returns up to about 30 accounts per round (return data is
limited to 1 KB). Strategies with a fixed account set answer in one round; state-dependent
ones return what they know and are asked again, up to four rounds.

## Solana limits

- **Call depth.** Trader → router (1) → strategy (2) → token program (3) → transfer hook (4)
  leaves one level spare. A strategy that calls another program while trading a hooked
  token uses the last one.
- **Compute.** A swap through the example strategy uses about 40–45k compute units, of
  which the router uses 30–35k (`crates/pipeline-tests/tests/compute.rs` enforces this).
- **Transaction size.** A single swap fits in a normal transaction. Set compute and
  loaded-data limits explicitly.
- **Rent.** Read `Rent::get()`; don't hard-code rent.

## Known limitations

- **Bypass is allowed.** Anyone can call a strategy directly without paying the fee.
- **Quote and swap can diverge** for strategies that don't share code between them. The
  trader's minimum output still protects them.
- **Restrictive transfer hooks.** A token whose hook refuses transfers into the market's
  fee account can be bought through Pipeline but not sold.
- **`create_market` passes on fresh signers.** An account that signs and is empty, unfunded
  and system-owned keeps its signature, so a strategy can create it (the creator's own key
  never does). A wallet with no SOL looks the same, including one emptied earlier in the
  same transaction, and so does a program's PDA signing through a CPI. Sign
  `create_market` only with new keypairs, and don't pass a program's own PDAs as signers.
- **Integrator fees have no cap.** The trader signs the rate; front-ends should show it.
- **Loader-v4 programs** can't register yet; use the upgradeable loader (v3).
- **Audit.** Formally verified ([`VERIFICATION.md`](VERIFICATION.md)) and audited with
  Stronghold of Security ([`SECURITY-AUDIT.md`](SECURITY-AUDIT.md)), after three internal
  adversarial rounds. No external audit.
