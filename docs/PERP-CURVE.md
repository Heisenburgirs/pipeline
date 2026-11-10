# Perp-backed curve: a launch curve whose fee funds a position every holder owns

`programs/perp-curve` is the launch curve of [`LAUNCH-CURVE.md`](LAUNCH-CURVE.md), with
one addition: part of every trade's fee goes into a **treasury** that holds a perpetual
position, and every holder owns a share of that position they can cash out by burning
tokens. A token launched on it has two exits: the curve, and the position.

The curve, the graduation into a locked pool, the creator and protocol fees and
`claim_fees` are exactly as in the launch curve; read that first. This page covers what
is new.

## The treasury

Every trade in either phase pays `perp_fee_bps` of its input to the treasury, on top of
the creator's and the protocol's fees. In the presets that is 0.3%, so the full stack is
0.3% creator, 0.2% protocol, 0.3% treasury, plus Pipeline's 0.25%.

- On a **buy** the fee is quote. It accrues in the quote vault next to the reserve,
  counted in `perp_owed_quote`, until `deploy` pushes it into the position.
- On a **sell** the fee is tokens. It accrues in the token vault, counted in
  `perp_owed_token`, until `deploy` **burns** it: fewer tokens, the same position, so
  every remaining holder's share grows.

`deploy` is a custom action anyone can call. It burns whatever tokens are waiting, and
pushes the quote into the position once it has reached `min_deploy`, opening the
position on the first deploy (the caller pays its rent). It is refused only when there
is nothing to do. A keeper or the front-end calls it from time to time.

## The position

The position lives on `programs/test-perp`, a minimal perpetuals venue in this repo
built to look like a real one: one PDA per **market** (`SOL-PERP`, `BTC-PERP`, ...)
quoting a base asset in a collateral mint at a price its authority sets (an oracle on a
real venue), and one PDA per **position** per owner per market, long or short at a fixed
leverage. The strategy's position is `["position", market, launch]` under the venue,
owned by the launch PDA, which signs every venue call.

Which market, which way and how much leverage are market parameters, chosen by whoever
launches the token: a 1x long SOL treasury, a 5x short, anything the venue lists. The
venue's collateral mint must be the token's quote mint.

Each deploy adds `amount × leverage / price` of base at the venue's price and averages
the entry price. The position's **equity** is its collateral plus profit or loss at the
current price, floored at zero; a position whose equity reaches zero is wiped on its
next decrease. The venue has no funding and no fee, and pays winners out of a vault that
a demo seeds with extra collateral.

The strategy never reads the venue's state and holds no copy of it. It only calls three
things, open, increase and decrease a share, so a real venue plugs in behind the same
calls and account layouts.

## Burning into the position

`burn_to_close(amount)` is a custom action signed by a holder. It burns `amount` of
their tokens and closes `amount / circulating` of the position, paying that share of its
equity in quote to the holder. `circulating` is what holders have in hand: the supply
less everything burned and everything the market itself holds, which is the reserve,
the held-back pool share before graduation, and unclaimed fees.

The curve is untouched: burned tokens were never its reserves, so its price does not
move. A holder therefore picks whichever exit pays more for the same tokens, the curve
or the position, and the two move independently: the curve with the token's own
demand, the position with the perp's price. When the position is underwater the curve
wins; when the perp has run, the position does.

Burning refuses more than the circulating supply, and anything before the first deploy
has opened the position.

## Setup data

`pipeline_init_market` takes the launch curve's `InitParams` with these fields appended:

| Field | Meaning |
|---|---|
| `perp_market: Pubkey` | The venue market the position is taken on |
| `perp_direction: u8` | 0 long, 1 short |
| `perp_leverage: u8` | 1 to 20 |
| `perp_fee_bps: u16` | Fee on every trade for the treasury, at most 500 |
| `min_deploy: u64` | `deploy` leaves the quote alone below this (it still burns tokens) |

Setup accounts are the launch curve's: quote mint, token mint (writable, signer), quote
vault, token vault, the quote mint's token program, SPL Token.

## Custom actions

| Action | Signer | Accounts |
|---|---|---|
| `deploy` | anyone (pays the position's rent on the first call) | payer (writable), launch (writable), quote vault (writable), token vault (writable), token mint (writable), venue market, position (writable), venue vault (writable), quote mint, quote token program, SPL Token, the venue program, system program |
| `burn_to_close(amount: u64)` | the holder | holder, launch (writable), holder's token account (writable), holder's quote token account (writable), token mint (writable), quote mint, venue market, position (writable), venue vault (writable), SPL Token, quote token program, the venue program |
| `claim_fees` | creator or upgrade authority | as in the launch curve |

Events: `Deployed { amount, burned }` and `BurnedForPosition { holder, burned,
circulating, payout }`, besides the launch curve's `Graduated` and `FeesClaimed`.

## Limits

- The venue's collateral mint is the quote mint, and venue transfers do not forward
  Token-2022 transfer-hook accounts, so a quote mint with hooks cannot back a position.
- `test-perp` is a playground: its price is set by an authority, there is no funding,
  no margin call before zero equity, and the vault is only as deep as what was put in.
- A holder's payout is whatever the venue pays for the closed share; the strategy does
  not promise a floor.

## On devnet

| | Address |
|---|---|
| Venue (`test-perp`) | `8FcD14vCkywa5zPAMmy3g9VpnESJYUZM36c6hzJBdsYi` |
| Strategy (`perp-curve`) | `4LHmX6x7xgB2eNeuRsuF3U3huhFoyfYeddRLzsBwzvPc` |
| Demo venue market (SOL-PERP-ceb9, priced at the SOL price of the day) | `BAx8pFJX5aLfTMJ2kW5J92Nzct2MVwgoHU9vRQ64Ruhf` |
| Demo market (the launch preset, 0.3% treasury, 2x long) | `4dQ9LNDqPqGG32wJqRKPGK2k7hxFcVDteYUNq36Ftwqj` |
| Demo token | `E5FXWVhcixn6n7xGqHJ49K1nskQue14Av1rVNnTHtAM8` |
| The market's position on the venue | `5WkguXj7yp78sQjqoZo7g4W4iWfGNNL7yYof2oShEiaf` |

Both programs are deployed and the strategy is registered with the router. The
`strategies.ts` devnet script (`ONLY=perp`) created a SOL-PERP market on the venue at the
live SOL price, seeded its vault, launched the token with a 2x long treasury, bought
$1,000 (so $3 went to the treasury), deployed it into the position, marked SOL up 20%
and burned a tenth of the tokens in hand into the position for $0.42: a tenth of the
position's $4.20 of equity.
