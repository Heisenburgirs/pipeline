# Launch curve: a bonding curve that graduates at a target market cap

`programs/launch-curve` is a Pipeline strategy that launches a token on a constant-product
bonding curve with virtual reserves (the pump.fun shape) and, once the curve sells out,
graduates it into a constant-product pool inside the same market. Trading never stops
and never moves: the curve phase and the pool phase are the same Pipeline market, the
same swap instruction and the same event.

It is a worked example of a strategy whose pricing changes regime mid-life, with the
transition decided inside the pricing function so that quotes and swaps always agree.

## The market

A market is one new token against one quote token (USDC, wrapped SOL, any SPL or
Token-2022 mint). At creation the strategy:

- creates the token mint (a fresh keypair that signs `create_market`; the router forwards
  the signature because the account is empty), with 6 decimals;
- mints the whole `supply` into its own token vault and gives up the mint authority, so
  the supply is fixed for good;
- creates the quote vault. Both vaults are owned by the launch PDA `["launch", token_mint]`.

Mint index 0 is the quote token, 1 is the launched token. Only exact-in trades are
supported. Pipeline's 0.25% applies to every trade in both phases, on top of the fees
below.

### Fees

Every trade, in either phase and either direction, pays two fees from its input:
`creator_fee_bps` to the market's **creator** (a wallet named at creation, at most 5%)
and `PROTOCOL_FEE_BPS` = 0.2% to whoever holds the **program's upgrade authority**. The
second one is compiled into the program, so a market creator cannot switch it off, and it
follows control of the program rather than a hard-coded key: upgrade the program to a new
authority and the fee moves with it; make the program immutable and nobody can claim it.

Fees are taken in the input mint (quote on a buy, token on a sell) and accrue inside the
market's own vaults next to the reserves, recorded in four counters on the launch account.
Each party claims theirs with the `claim_fees` custom action, to any token account of
the right mint. Nothing about a trade depends on the recipients' accounts existing, so a
closed wallet can never block trading. The quoter and the swap both see the net input, so
quotes already reflect the fees.

### Phase 1: the curve

`curve_supply` tokens are for sale. With `x = virtual_quote + quote_reserve` and
`y = virtual_token + token_reserve`, a buy of `q` gives `floor(y × q / (x + q))` tokens and
a sell of `t` gives `floor(x × t / (y + t))` quote, capped at what the vault holds, with `q`
and `t` the input after the fees above.

The remaining `supply − curve_supply` tokens sit in the token vault but are not for sale.
They are the pool's token side.

### Graduation

The buy that would take the last curve token graduates the market. That buy pays exactly
`ceil(x × left / (y − left))` for the `left` tokens still on the curve, the market flips to
the pool phase, and whatever is left of the buyer's input is spent on the new pool in the
same trade. Pricing is continuous across the boundary and nobody overpays for the last
curve token. The strategy emits a `Graduated` event with the raise and the pool's
opening reserves, and records the slot.

Graduation is one-way. Sells after it trade against the pool, never the curve.

### Phase 2: the pool

The quote raised on the curve and the held-back tokens trade as a constant-product pool
on real reserves, with a `pool_fee_bps` fee taken from the input and kept in the pool.
Nobody owns the liquidity and nothing can withdraw it: the curve PDA holds both vaults
and the program has no instruction that moves them except a swap. Graduation
permanently locks the raise as liquidity.

## Choosing parameters

`pipeline_init_market` takes a Borsh-encoded `InitParams`:

| Field | Meaning |
|---|---|
| `supply: u64` | Tokens minted, in base units (6 decimals) |
| `curve_supply: u64` | Tokens sold on the curve; the rest is held back for the pool |
| `virtual_quote: u64` | Virtual quote reserve |
| `virtual_token: u64` | Virtual token reserve, added to the tokens still for sale; the curve ends with exactly this many |
| `pool_fee_bps: u16` | Fee on pool trades after graduation, kept in the pool (0 in the presets) |
| `creator_fee_bps: u16` | Fee on every trade for the creator, at most 500 |
| `creator: Pubkey` | Who claims the creator fee |

Setup accounts after the router's fixed four: quote mint, token mint (writable, signer),
quote vault `["quote_vault", launch]`, token vault `["token_vault", launch]`, the quote
mint's token program, SPL Token.

The curve ends with `x_final = virtual_quote × (virtual_token + curve_supply) / virtual_token`
quote against `virtual_token` tokens, so:

- **raise** = `x_final − virtual_quote`
- **final price** = `x_final / virtual_token`
- **graduation market cap** = final price × `supply`
- **pool opening price** = raise / (`supply − curve_supply`)

Pick the parameters so the pool opening price equals the final price, or the price jumps
at graduation. On this curve one number fixes everything else: the multiple `M` by which
the price rises between the first and the last curve token. With `r = 1 / √M` and a final
price `p` (quote base units per token base unit):

- `curve_supply = supply / (1 + r)`, which holds back `r × curve_supply` tokens;
- `virtual_token = r × curve_supply / (1 − r)`;
- `virtual_quote = r × virtual_token × p`.

The curve then raises `r / (1 + r)` of the graduation market cap, and the held-back tokens
are worth exactly that raise at the final price, so the pool opens at parity.

`M` is a trade-off between how low the token starts and how much liquidity backs it at
graduation; you cannot pick both. pump.fun's own curve rises 14.7x and raises 20.7% of its
graduation market cap, but that depth only exists at its $69k graduation. Its tokens that
went on to reach $1B had on the order of 1 to 2% of their market cap in their main pool
by then, so a curve that bonds at $1B should lock about that much. Presets for a
one-billion-token supply, a USDC quote and a $1.00 final price (a $1B graduation):

| Start market cap | `M` | `curve_supply` (tokens) | `virtual_token` (tokens) | `virtual_quote` | Raised and locked at graduation |
|---|---|---|---|---|---|
| **$100k** (default) | 10,000x | 990,099,009.900990 | 10,001,000.100010 | $100,010.001 | $9.90M, 0.99% |
| $417k | 2,400x | 979,995,909.267370 | 20,420,931.416388 | $416,840.684 | $20.00M, 2.0% |
| $68M (pump.fun's shape) | 14.7x | 793,100,000 | 279,900,000 | $73,013,988.82 | $206.9M, 20.7% |

In base units with 6 decimals on both sides, the default is `curve_supply =
990_099_009_900_990`, `virtual_token = 10_001_000_100_010` and `virtual_quote =
100_010_001_000`. The start market cap is also the token's floor: the curve's reserves
cannot go below zero, so it never prices below its opening price. The tests in
`crates/pipeline-tests/tests/launch.rs` run the default through the router and check the
raise, the final price and the pool's opening price.

## Custom actions

| Action | Signer | Accounts |
|---|---|---|
| `claim_fees` | the market's creator, or the program's upgrade authority | claimant (signer), launch (writable), quote vault (writable), token vault (writable), claimant's quote token account (writable), claimant's token account (writable), quote mint, token mint, quote token program, SPL Token, this program's ProgramData; then any transfer-hook accounts of the quote mint |

The discriminator is Anchor's `sha256("global:claim_fees")[..8]`. A claimant who is both
creator and upgrade authority receives both shares in one call. A `FeesClaimed` event
reports what was paid.

## Account resolution

Account resolution returns the output vault, writable, in either phase: the token vault
for a buy, the quote vault for a sell. Transfer-hook accounts for the quote mint follow
as usual. Nothing about a trade's accounts depends on whether the market has graduated.

## Limits

- Exact-out is not supported.
- The quote token may have Token-2022 transfer fees or hooks; the launched token is a
  plain SPL Token mint.
- There is no deadline. A curve that never sells out trades on the curve indefinitely; a
  deadline that refunds or graduates early is a natural extension.
- The pool fee, if set, accrues to the pool itself; the creator and protocol fees are
  the ones that leave.

## On devnet

| | Address |
|---|---|
| Program | [`4scUVyL9JbLjvkppBk8PYNGRPMCQfp7ppdBFkthDYPLA`](https://orbmarkets.io/address/4scUVyL9JbLjvkppBk8PYNGRPMCQfp7ppdBFkthDYPLA/history?cluster=devnet) |
| Demo market (the default preset above, against a 6-decimal test quote) | [`CpA1g1zdf2JXC9mxqqdc48nJg63Z8FYdsqZQvLpiaLdS`](https://orbmarkets.io/address/CpA1g1zdf2JXC9mxqqdc48nJg63Z8FYdsqZQvLpiaLdS/history?cluster=devnet) |
| Demo token | `G1WDvVUudx95ccfRTjT9rGL4BDTZa85oG3Rq7hCoimed` |

The program is registered with the router. `scripts/devnet/strategies.ts` is what registered
it and created the demo market; run it with `ONLY=launch` against any cluster where
Pipeline is set up to create your own.
