# Gacha curve: a bonding-curve strategy with a prize pot

`programs/gacha-curve` is a Pipeline strategy that launches a token on a constant-product
bonding curve with virtual reserves (the pump.fun shape) and adds a gacha. Every trade
pays a small fee, split between a **pot** of the token, the curve itself, the token's
creator and the launchpad, and a buyer who opts in gets a free chance at a bonus from
that pot. It is a launchpad: anyone creates a market on it, and the program's upgrade
authority earns from every one. It is a worked example of a strategy with per-trader state and a
custom action that pays out after the trade.

## The market

A market is one new token against one quote token (wrapped SOL, USDC, any SPL or
Token-2022 mint). At creation the strategy:

- creates the token mint (a fresh keypair that signs `create_market`; the router forwards
  the signature because the account is empty), with 6 decimals;
- mints the whole `supply` into its own token vault and gives up the mint authority, so
  the supply is fixed for good;
- creates the quote vault and the **pot** vault, all owned by the curve PDA.

Mint index 0 is the quote token, 1 is the launched token. Only exact-in trades are
supported. Pipeline's 0.25% applies to every trade, on top of the curve's own fee below.

Pricing uses virtual reserves. With `x = virtual_quote + quote_reserve` and
`y = virtual_token + token_reserve`, a buy of `q` gives `floor(y × q / (x + q))` tokens and
a sell of `t` gives `floor(x × t / (y + t))` quote, each capped at what the vault holds.
`virtual_quote` sets the opening price together with the supply; `virtual_token` keeps the
price finite as the supply sells out.

### Fees

Every trade pays four fees from its input and is priced on the rest. In the presets
they add up to 0.5%, so a buyer who wants $100 of tokens sends about $100.50, plus
Pipeline's 0.25%.

| Fee | Preset | Where it goes |
|---|---|---|
| `pot_fee_bps` | 0.30% | The **pot**. On a buy it is spent on the curve, as it stands after the buyer's own purchase, and the tokens it buys go to the pot vault. On a sell it is taken in tokens and moved to the pot. The pot only ever holds the launched token, and all trading in either direction funds the prizes. |
| `PROTOCOL_FEE_BPS` | 0.10% | The **launchpad**: whoever holds the program's upgrade authority. Compiled in, so a market creator cannot switch it off; it follows control of the program, and becomes unclaimable if the program is made immutable. |
| `creator_fee_bps` | 0.05% | The market's **creator**, a wallet named at creation. At most 5%. |
| `curve_fee_bps` | 0.05% | The **curve's reserves**: quote on a buy, tokens on a sell, with nothing paid out for it. It lifts the price a little for every holder, the way an LP fee deepens a pool. |

The creator's and the launchpad's fees are taken in the input mint and accrue inside the
market's own vaults next to the reserves, recorded in four counters on the curve account.
Each party claims theirs with the `claim_fees` custom action, to any token account of the
right mint. No trade depends on a recipient's account existing, so a closed wallet can
never block trading. The quoter and the swap both see the net input, so quotes already
reflect every fee.

### Setup data

`pipeline_init_market` takes a Borsh-encoded `InitParams`:

| Field | Meaning |
|---|---|
| `supply: u64` | Tokens minted to the curve, in base units (6 decimals) |
| `virtual_quote: u64` | Virtual quote reserve |
| `virtual_token: u64` | Virtual token reserve |
| `win_one_in: u32` | A gacha buy wins with probability 1 in this many |
| `bonus_bps: u16` | Win size, in basis points of the buyer's base output |
| `max_win_pot_bps: u16` | Cap on one win, in basis points of the pot's balance |
| `reveal_delay_slots: u64` | Slots between a gacha buy and the slot whose block hash decides it |
| `pot_fee_bps: u16` | Fee on every trade, in basis points of the input, paid into the pot |
| `curve_fee_bps: u16` | Fee on every trade, left in the curve's reserves |
| `creator_fee_bps: u16` | Fee on every trade for the creator, at most 500 |
| `creator: Pubkey` | Who claims the creator fee |

Setup accounts after the router's fixed four: quote mint, token mint (writable, signer),
quote vault `["quote_vault", curve]`, token vault `["token_vault", curve]`, pot vault
`["pot", curve]`, the quote mint's token program, SPL Token. The market state is the curve
PDA `["curve", token_mint]`.

Opting in costs nothing, so assume every buy does and keep the expected payout at or
below the pot fee: `bonus_bps / win_one_in ≤ pot_fee_bps`. For example a 0.3% pot fee
supports a 1-in-100 chance of a 20% bonus (an expected 0.2%), and sells only add to the
pot. `max_win_pot_bps` stops a hot streak from emptying it.

## A gacha buy

A buy opts in by setting the first byte of the trade's `extra` to `1`. It pays the same
fees and gets the same output as a plain buy. The strategy:

1. takes the fees off the input and prices the rest on the curve: that is the buyer's
   **base** output, and what `pipeline_quote` returns;
2. spends the pot fee on the curve as it stands after the base buy and sends what it buys
   to the pot vault;
3. arms the buyer's **ticket** with the current slot and the base amount.

A ticket is a PDA `["ticket", curve, trader]`, one per trader per curve. The trader opens
it with the `open_ticket` custom action (paying its rent) in the same transaction as the
swap, before it. A trader with an armed ticket can still buy and sell normally, but cannot
make another gacha buy until the ticket is resolved.

Account resolution returns the output vault and the pot vault for every trade, and for a
gacha buy also the ticket (only when the quote names a trader). Transfer-hook accounts for
the quote mint follow as usual.

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
rolls revert and cost only the transaction fee, and no fee is ever collected on them. Deciding with a block hash that does not exist yet when the buy lands closes that,
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
| `claim_fees` | the market's creator, or the program's upgrade authority | claimant (signer), curve (writable), quote vault (writable), token vault (writable), claimant's quote token account (writable), claimant's token account (writable), quote mint, token mint, quote token program, SPL Token, this program's ProgramData; then any transfer-hook accounts of the quote mint |

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

## On devnet

| | Address |
|---|---|
| Program | [`2TmcmquxnCB8AHoLmLJFYxZnVQdm9pREMEsmHqByQq7G`](https://orbmarkets.io/address/2TmcmquxnCB8AHoLmLJFYxZnVQdm9pREMEsmHqByQq7G/history?cluster=devnet) |
| Demo market (0.3% pot, 0.1% launchpad, 0.05% creator, 0.05% curve; 1-in-100 for a 20% bonus, 10-slot reveal, 9-decimal test quote) | [`Feqj8KswVnRW6shaxSQhczHNWscqoWSvRRspAYLvaBfD`](https://orbmarkets.io/address/Feqj8KswVnRW6shaxSQhczHNWscqoWSvRRspAYLvaBfD/history?cluster=devnet) |
| Demo token | `F4Tkeav4eLFkxBLqH6JUcXzYLJLjr8FSQ59s6xRMh8Nb` |

The program is registered with the router. The `strategies.ts` devnet script on the
`launch-curve` branch registered it, created the demo market, made a gacha buy and
resolved the ticket; run it with `ONLY=gacha` to create your own.
