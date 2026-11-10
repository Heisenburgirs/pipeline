# Gacha curve: a launchpad bonding curve with packs of prizes

`programs/gacha-curve` is a Pipeline strategy that launches a token on a constant-product
bonding curve with virtual reserves (the pump.fun shape) and adds a gacha. Every trade
pays a small fee, split between a **pot** of the token, the curve itself, the token's
creator and the launchpad. Every buy drops a free **draw** into one of the buyer's
**packs**, and opening a draw is a chance at a prize from the pot sized to that buy. It
is a launchpad: anyone creates a market on it, and the program's upgrade authority earns
from every one. As a Pipeline strategy it is a worked example of per-trader state,
multi-round account resolution, and custom actions that pay out after the trade.

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
| `win_one_in: u32` | A draw wins with probability 1 in this many |
| `bonus_bps: u16` | Win size, in basis points of the draw's purchase |
| `max_win_pot_bps: u16` | Cap on one win, in basis points of the pot's balance |
| `fresh_window_slots: u64` | A draw opened within this many slots of its buy is capped by the pot as it is; later, also by the pot as it was at the buy |
| `pot_fee_bps: u16` | Fee on every trade, in basis points of the input, paid into the pot |
| `curve_fee_bps: u16` | Fee on every trade, left in the curve's reserves |
| `creator_fee_bps: u16` | Fee on every trade for the creator, at most 500 |
| `creator: Pubkey` | Who claims the creator fee |
| `jackpot_one_in: u32` | A draw hits the jackpot with probability 1 in this many; 0 disables it |
| `jackpot_bps: u32` | Jackpot size, in basis points of the draw's purchase (1,000,000 is 100x) |
| `max_jackpot_pot_bps: u16` | Cap on one jackpot, in basis points of the pot's balance |

Setup accounts after the router's fixed four: quote mint, token mint (writable, signer),
quote vault `["quote_vault", curve]`, token vault `["token_vault", curve]`, pot vault
`["pot", curve]`, the quote mint's token program, SPL Token. The market state is the curve
PDA `["curve", token_mint]`.

Draws are free, so assume every buy gets one and keep the expected payout of both tiers
at or below the pot fee: `bonus_bps / win_one_in + jackpot_bps / jackpot_one_in ≤
pot_fee_bps`. The presets spend the 0.3% exactly: a 1-in-100 chance of a 20% bonus
(0.2%) plus a 1-in-100,000 chance of a 100x jackpot (0.1%), and sells only add to the
pot. The caps, `max_win_pot_bps` and `max_jackpot_pot_bps`, stop a hot streak or an early
jackpot from emptying it: a $100 buy's jackpot is $10,000 of tokens only once the pot
holds $20,000, and half the pot before that.

## Packs and draws

A **pack** is an account `["pack", curve, trader, index]`, opened by the trader with the
`open_pack` custom action (paying its rent) and holding up to 16 **draws**. A buy names
one of the trader's packs by putting its index in the trade's `extra` (two bytes,
little-endian). Such a buy pays the same fees and gets the same output as a plain buy;
on top, the strategy drops a draw into the pack recording:

- the buyer's **base** output, which prizes are a share or a multiple of, so a $1 draw can
  only ever pay prizes sized to $1 and spamming small buys buys nothing;
- the pot's balance at that moment;
- the slot;
- a fresh randomness **seed**, `sha256("gacha-vrf" ‖ pack ‖ nonce ‖ draw index)`, where
  `nonce` was assigned to the pack from a counter on the curve when it was opened. The
  ORAO randomness account for that seed must not exist yet (see below).

A full pack means opening another, with the next index; packs and their draws
accumulate for as long as the trader likes. A pack whose every draw has been opened
(or that holds none) can be closed with `close_pack` for its rent; a pack closed and
opened again gets a new nonce, so seeds never repeat. A buy without a pack index is a
plain buy and drops nothing, which is also what anonymous quotes and aggregators get.

Account resolution for a buy into a pack takes two rounds: the output vault, the pot
vault and the pack first, then, once the pack's data is in hand, the ORAO randomness
account for its next draw's seed (a pack not opened yet is assumed to be opened in the
swap's transaction with the curve's next nonce). Plain buys and sells return the output
vault and the pot vault. Transfer-hook accounts for the quote mint follow as usual.

## Opening a draw

The randomness comes from the [ORAO VRF](https://github.com/orao-network/solana-vrf),
program `VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y` on devnet and mainnet. To open a
draw, the trader (or anyone) sends ORAO's `request_v2` with the draw's seed, paying
ORAO's request fee (0.0003 SOL on devnet); the oracles fulfil it within seconds into the
account `["orao-vrf-randomness-request", seed]` under the VRF program. The strategy
never calls ORAO: it only reads that account.

Anyone can then call `open_draw` with that account. The strategy checks it is the account
for the draw's seed, owned by the VRF program, fulfilled, and carrying the same seed, then
reads the 64 bytes of randomness. The roll is their first 8 bytes as a little-endian
`u64`; a multiple of `jackpot_one_in` is a jackpot, otherwise a multiple of `win_one_in`
is a win. Until the account is fulfilled, opening fails with `NotFulfilled`.

A win pays `min(bonus_bps × purchase, max_win_pot_bps × pot)` and a jackpot
`min(jackpot_bps × purchase, max_jackpot_pot_bps × pot)` from the pot to any token
account of the launched mint owned by the pack's trader. The `pot` in those caps is the
pot as it is now for a draw opened within `fresh_window_slots` of its buy, and the smaller
of the pot now and the pot at the buy for a draw opened later. Early draws, when the pot
is still small, are not punished for opening promptly, and sitting on a draw whose
randomness is already known can never make it worth more. Win or lose, the draw is
marked opened and a `DrawResolved` event reports the outcome. Front-ends should request
the randomness as soon as the trader wants to open a draw and open it as soon as it is
fulfilled; a keeper can open everyone's.

### Why the roll is not decided in the buy

A roll decided inside the swap can be filtered for free. The router guarantees the trader
at least `min_amount_out`, so a trader sets that to the jackpot amount and submits; losing
rolls revert and cost only the transaction fee, and no fee is ever collected on them.
Deciding with randomness that does not exist yet when the buy lands closes that, and it
keeps `pipeline_quote` and `pipeline_swap` in agreement: both return the base amount,
and the prize never passes through the router.

A draw's seed is fixed by the pack, its nonce and the draw's index, so it is known before
the buy. What stops a trader from requesting it early, seeing the outcome and only then
buying is that the buy refuses to add the draw (`SeedAlreadyRequested`) if the randomness
account for that seed already exists, pending or fulfilled. Since the only way to move a
pack to its next seed is a buy, no outcome can be learned before the buy that commits to
it. The randomness itself is ORAO's: oracle signatures over the seed, verifiable and not
influenceable by block leaders, so it is fit for large jackpots.

## Custom actions

| Action | Signer | Accounts |
|---|---|---|
| `open_pack(index: u16)` | trader | trader (writable), curve (writable), pack (writable), system program |
| `close_pack` (every draw opened) | trader | trader (writable), pack (writable) |
| `open_draw(draw: u8)` | anyone | pack (writable), curve, trader, pot vault (writable), trader's token account (writable), token mint, SPL Token, the ORAO randomness account for the draw's seed |
| `claim_fees` | the market's creator, or the program's upgrade authority | claimant (signer), curve (writable), quote vault (writable), token vault (writable), claimant's quote token account (writable), claimant's token account (writable), quote mint, token mint, quote token program, SPL Token, this program's ProgramData; then any transfer-hook accounts of the quote mint |

Discriminators are Anchor's `sha256("global:<name>")[..8]`, arguments follow them Borsh-
encoded. The tests in `crates/pipeline-tests/tests/gacha.rs` build every instruction by
hand and are the reference for account order.

## Limits

- Exact-out is not supported.
- The quote token may have Token-2022 transfer fees or hooks; the launched token is a
  plain SPL Token mint.
- A buy into a pack that is not open yet assumes the pack will get the curve's next
  nonce; if another pack is opened on the curve between the quote and the swap, the
  randomness account no longer matches and the swap fails with `WrongAccount`. Opening
  the pack in its own transaction first avoids the race.
- A prize is paid in the launched token from the pot, never from the curve's reserves,
  so the curve's price is never affected by payouts.

## On devnet

| | Address |
|---|---|
| Program | [`2TmcmquxnCB8AHoLmLJFYxZnVQdm9pREMEsmHqByQq7G`](https://orbmarkets.io/address/2TmcmquxnCB8AHoLmLJFYxZnVQdm9pREMEsmHqByQq7G/history?cluster=devnet) |
| Demo market (0.3% pot, 0.1% launchpad, 0.05% creator, 0.05% curve; 1-in-100 for a 20% bonus, 1-in-100,000 for a 100x jackpot, ORAO VRF, 1,500-slot fresh window, 9-decimal test quote) | [`5cNpJXz2Nf3gohTR6JWUBSQgZd6HTT9PWoxjQbK413Qg`](https://orbmarkets.io/address/5cNpJXz2Nf3gohTR6JWUBSQgZd6HTT9PWoxjQbK413Qg/history?cluster=devnet) |
| Demo token | `6AawmqpeCnx6E5uSYL4hBwLy9RWw5NxuytrjrBoKgLMb` |

The program is registered with the router. The `strategies.ts` devnet script on the
`launch-curve` branch registered it, created the demo market, opened a pack, bought into
it, requested the draw's seed from ORAO (fulfilled in about two seconds) and opened the
draw; run it with `ONLY=gacha` to create your own.
