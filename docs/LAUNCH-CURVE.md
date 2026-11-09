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
supported. Pipeline's 0.25% applies to every trade in both phases.

### Phase 1: the curve

`curve_supply` tokens are for sale. With `x = virtual_quote + quote_reserve` and
`y = virtual_token + token_reserve`, a buy of `q` gives `floor(y × q / (x + q))` tokens and
a sell of `t` gives `floor(x × t / (y + t))` quote, capped at what the vault holds. There is
no fee of the strategy's own on the curve.

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
| `pool_fee_bps: u16` | Fee on pool trades after graduation, kept in the pool |

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
at graduation. The pump.fun ratios do this almost exactly. Scaled to a one-billion-token
supply, a USDC quote and a $1B graduation:

| Parameter | Value | Why |
|---|---|---|
| `supply` | 1,000,000,000 tokens | |
| `curve_supply` | 793,100,000 tokens | pump.fun sells 793.1M of 1B on the curve |
| `virtual_token` | 279,900,000 tokens | pump.fun prices against 1,073,000,000 virtual tokens that include the real ones; 279.9M is what is left when the 793.1M real ones are gone |
| `virtual_quote` | $73,013,988.82 | `virtual_token² / (virtual_token + curve_supply)`, which puts the final price at $1.00 |

That curve opens at $0.068 per token (a $68M market cap), ends at $1.00 (a $1B market
cap) and raises **$206.9M**, 20.7% of the graduation market cap. The 206.9M held-back
tokens are worth $206.9M at $1.00, so the pool opens at $0.99993, within 0.01% of where
the curve ended. These are the same proportions as pump.fun itself: 30 SOL virtual, 85
SOL raised, 411 SOL market cap at graduation.

The tests in `crates/pipeline-tests/tests/launch.rs` run exactly these numbers through the
router and check the raise, the final price and the pool's opening price.

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
- The pool fee accrues to the pool itself. Routing it to a creator or a treasury is a
  small change to `apply` plus an account.
