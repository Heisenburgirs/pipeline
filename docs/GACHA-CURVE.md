# Gacha curve: a bonding-curve strategy with a prize pot

`programs/gacha-curve` is a Pipeline strategy that launches a token on a constant-product
bonding curve with virtual reserves (the pump.fun shape) and adds an opt-in gacha: a buyer
can pay a small extra fee into a pot of the token and get a chance at a bonus from that
pot. It is a worked example of a strategy with per-trader state and a custom action that
pays out after the trade.

## The market

A market is one new token against one quote token (wrapped SOL, USDC, any SPL or
Token-2022 mint). At creation the strategy:

- creates the token mint (a fresh keypair that signs `create_market`; the router forwards
  the signature because the account is empty), with 6 decimals;
- mints the whole `supply` into its own token vault and gives up the mint authority, so
  the supply is fixed for good;
- creates the quote vault and the **pot** vault, all owned by the curve PDA.

Mint index 0 is the quote token, 1 is the launched token. Only exact-in trades are
supported. The curve has no fee of its own; Pipeline's 0.25% applies to every trade.

Pricing uses virtual reserves. With `x = virtual_quote + quote_reserve` and
`y = virtual_token + token_reserve`, a buy of `q` gives `floor(y × q / (x + q))` tokens and
a sell of `t` gives `floor(x × t / (y + t))` quote, each capped at what the vault holds.
`virtual_quote` sets the opening price together with the supply; `virtual_token` keeps the
price finite as the supply sells out.

### Setup data

`pipeline_init_market` takes a Borsh-encoded `InitParams`:

| Field | Meaning |
|---|---|
| `supply: u64` | Tokens minted to the curve, in base units (6 decimals) |
| `virtual_quote: u64` | Virtual quote reserve |
| `virtual_token: u64` | Virtual token reserve |
| `gacha_fee_bps: u16` | Fee on gacha buys, in basis points of the input |
| `win_one_in: u32` | A gacha buy wins with probability 1 in this many |
| `bonus_bps: u16` | Win size, in basis points of the buyer's base output |
| `max_win_pot_bps: u16` | Cap on one win, in basis points of the pot's balance |
| `reveal_delay_slots: u64` | Slots between a gacha buy and the slot whose block hash decides it |

Setup accounts after the router's fixed four: quote mint, token mint (writable, signer),
quote vault `["quote_vault", curve]`, token vault `["token_vault", curve]`, pot vault
`["pot", curve]`, the quote mint's token program, SPL Token. The market state is the curve
PDA `["curve", token_mint]`.

Keep expected payout at or below the fee so the pot stays solvent:
`bonus_bps / win_one_in ≤ gacha_fee_bps`. For example a 2% fee supports a 1-in-20 chance
of a 40% bonus. `max_win_pot_bps` stops a hot streak from emptying the pot.

## A gacha buy

A buy opts in by setting the first byte of the trade's `extra` to `1`. The strategy then:

1. takes `gacha_fee_bps` of the input as the fee and prices the rest on the curve: that is
   the buyer's **base** output, and what `pipeline_quote` returns;
2. spends the fee on the curve as it stands after the base buy, and sends what it buys to
   the pot vault;
3. arms the buyer's **ticket** with the current slot and the base amount.

A ticket is a PDA `["ticket", curve, trader]`, one per trader per curve. The trader opens
it with the `open_ticket` custom action (paying its rent) in the same transaction as the
swap, before it. A trader with an armed ticket can still buy and sell normally, but cannot
make another gacha buy until the ticket is resolved.

Account resolution for a gacha buy returns the token vault, the pot vault and the ticket
(the ticket only when the quote names a trader). Plain buys return the token vault; sells
return the quote vault. Transfer-hook accounts for the quote mint follow as usual.

## Resolving a ticket

Anyone can call `resolve_ticket` once the chain has a block at or after the ticket's
reveal slot (`buy slot + reveal_delay_slots`). The roll is
`sha256(block hash ‖ ticket ‖ buy slot)` read as a little-endian `u64`; the ticket wins when
it is a multiple of `win_one_in`. The block hash is that of the first block at or after the
reveal slot, read from the SlotHashes sysvar, so a skipped slot moves to the next block
rather than blocking resolution.

A win pays `min(bonus_bps × stake, max_win_pot_bps × pot)` from the pot to any token
account of the launched mint owned by the ticket's trader. Win or lose, the ticket closes
and its rent returns to the trader. The outcome is published as a `TicketResolved` event.

**Tickets expire.** A ticket must be resolved within 400 slots of its reveal slot
(`RESOLVE_WINDOW_SLOTS`). Later than that it pays nothing, win or not, and closes. The
SlotHashes sysvar keeps 512 entries, so inside the window the deciding block is still
present and the outcome cannot change. Without the window, a losing trader could wait for
the ring to roll past the reveal slot and get a fresh roll from a later block. Front-ends
should resolve for their users promptly; a keeper can resolve everyone's.

### Why the roll is not decided in the buy

A roll decided inside the swap can be filtered for free. The router guarantees the trader
at least `min_amount_out`, so a trader sets that to the jackpot amount and submits; losing
rolls revert and cost only the transaction fee, and the gacha fee is never collected on
them. Deciding with a block hash that does not exist yet when the buy lands closes that,
and it keeps `pipeline_quote` and `pipeline_swap` in agreement: both return the base
amount, and the bonus never passes through the router.

Block leaders can influence their block's hash, so this is fine for prizes smaller than
what a leader could earn by trying, which `max_win_pot_bps` bounds. For large prizes,
swap the slot hash for a VRF.

## Custom actions

| Action | Signer | Accounts |
|---|---|---|
| `open_ticket` | trader | trader (writable), curve, ticket (writable), system program |
| `close_ticket` (unarmed only) | trader | trader (writable), ticket (writable) |
| `resolve_ticket` | anyone | ticket (writable), curve, trader (writable), pot vault (writable), trader's token account (writable), token mint, SPL Token, SlotHashes sysvar |

Discriminators are Anchor's `sha256("global:<name>")[..8]`. The tests in
`crates/pipeline-tests/tests/gacha.rs` build every instruction by hand and are the
reference for account order.

## Limits

- Exact-out is not supported.
- The quote token may have Token-2022 transfer fees or hooks; the launched token is a
  plain SPL Token mint.
- One ticket per trader per curve. A trader who wants several open rolls uses several
  wallets.
- A win is paid in the launched token from the pot, never from the curve's reserves, so
  the curve's price is never affected by payouts.
