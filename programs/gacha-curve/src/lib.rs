//! Gacha bonding-curve strategy for Pipeline.
//!
//! A market is a new token sold along a constant-product curve with virtual reserves
//! (the pump.fun shape) against a quote token such as wrapped SOL. The strategy mints
//! the whole supply into its own vault at market creation and then gives up the mint
//! authority, so the supply is fixed. Buys move quote into the curve and tokens out;
//! sells do the reverse. Every trade reprices the curve; there is no separate pool fee.
//!
//! The gacha is opt-in per buy. A buyer who sets the first byte of the trade's `extra`
//! to 1 pays `gacha_fee_bps` of their input on top of the curve price. The fee is spent
//! on the curve like any other buy, and the tokens it buys go into a **pot** vault owned
//! by the curve. The buyer's ticket is armed with the slot of the buy. After
//! `reveal_delay_slots`, anyone may resolve the ticket: the hash of the first block at
//! or after the reveal slot decides a 1-in-`win_one_in` roll, and a win pays
//! `bonus_bps` of the buyer's base output from the pot, capped at `max_win_pot_bps` of
//! the pot.
//!
//! Why the two-step reveal: a roll decided inside the buy transaction can be filtered
//! for free. A trader sets `min_amount_out` to the jackpot and lets losing rolls
//! revert, paying the fee only when they win. Deciding the roll with a block hash that
//! does not exist yet at buy time closes that, and resolving within a bounded window
//! after the reveal slot stops a loser from waiting for the slot-hash ring to roll
//! over into a different outcome.
//!
//! Quotes return the deterministic base amount. The bonus never passes through the
//! router: it is paid by `resolve_ticket`, a custom action anyone can call.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::{invoke_signed, set_return_data};
use anchor_spl::token::Token;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use pipeline_strategy_interface::{self as psi, fees, seeds as pipeline_seeds, TradeMode};

declare_id!("2TmcmquxnCB8AHoLmLJFYxZnVQdm9pREMEsmHqByQq7G");

const BPS: u128 = 10_000;
/// Decimals of every token this strategy launches.
pub const TOKEN_DECIMALS: u8 = 6;
/// `extra[0]` value that opts a buy into the gacha.
pub const GACHA_FLAG: u8 = 1;
/// A ticket must be resolved within this many slots of its reveal slot. The slot-hash
/// sysvar keeps 512 entries, so within this window the first block at or after the
/// reveal slot is always still present and the outcome cannot change.
pub const RESOLVE_WINDOW_SLOTS: u64 = 400;
/// Mint index of the quote token (what buyers pay with) and of the launched token.
pub const QUOTE_INDEX: u8 = 0;
pub const TOKEN_INDEX: u8 = 1;
/// The SlotHashes sysvar, read by `resolve_ticket`.
pub const SLOT_HASHES_SYSVAR: Pubkey = pubkey!("SysvarS1otHashes111111111111111111111111111");

#[program]
pub mod gacha_curve {
    use super::*;

    /// Called by the Pipeline router from `create_market`. `data` is a Borsh-encoded
    /// [`InitParams`]. The token mint is a fresh keypair that signs `create_market`;
    /// the router forwards its signature because the account is empty.
    pub fn pipeline_init_market(ctx: Context<InitMarket>, data: Vec<u8>) -> Result<()> {
        let params = InitParams::try_from_slice(&data).map_err(|_| GachaError::BadInitData)?;
        params.validate()?;

        let curve_key = ctx.accounts.curve.key();
        let expected_signer = pipeline_market_signer(&curve_key);
        require_keys_eq!(
            ctx.accounts.market_signer.key(),
            expected_signer,
            GachaError::NotPipeline
        );

        let bump = [ctx.bumps.curve];
        let token_mint_key = ctx.accounts.token_mint.key();
        let curve_seeds: &[&[u8]] = &[b"curve", token_mint_key.as_ref(), &bump];
        let curve_info = ctx.accounts.curve.to_account_info();
        let token_program = ctx.accounts.token_program.to_account_info();
        let mint_info = ctx.accounts.token_mint.to_account_info();

        // Mint the whole supply into the curve's vault, then freeze the supply for good.
        mint_to(
            &token_program,
            &mint_info,
            &ctx.accounts.token_vault.to_account_info(),
            &curve_info,
            params.supply,
            &[curve_seeds],
        )?;
        revoke_mint_authority(&token_program, &mint_info, &curve_info, &[curve_seeds])?;

        let curve = &mut ctx.accounts.curve;
        curve.quote_mint = ctx.accounts.quote_mint.key();
        curve.token_mint = token_mint_key;
        curve.quote_vault = ctx.accounts.quote_vault.key();
        curve.token_vault = ctx.accounts.token_vault.key();
        curve.pot_vault = ctx.accounts.pot_vault.key();
        curve.quote_reserve = 0;
        curve.token_reserve = params.supply;
        curve.virtual_quote = params.virtual_quote;
        curve.virtual_token = params.virtual_token;
        curve.gacha_fee_bps = params.gacha_fee_bps;
        curve.win_one_in = params.win_one_in;
        curve.bonus_bps = params.bonus_bps;
        curve.max_win_pot_bps = params.max_win_pot_bps;
        curve.reveal_delay_slots = params.reveal_delay_slots;
        curve.market_signer = expected_signer;
        curve.bump = ctx.bumps.curve;
        Ok(())
    }

    pub fn pipeline_market_info(ctx: Context<MarketInfoAccounts>) -> Result<()> {
        let curve = &ctx.accounts.curve;
        let info = psi::MarketInfo {
            mints: vec![curve.quote_mint.to_bytes(), curve.token_mint.to_bytes()],
            vaults: vec![curve.quote_vault.to_bytes(), curve.token_vault.to_bytes()],
            supports_exact_out: false,
        };
        set_return_data(&borsh_encode(&info)?);
        Ok(())
    }

    /// Strategy accounts for a trade: the output vault, and for a gacha buy also the
    /// pot vault and the trader's ticket.
    pub fn pipeline_resolve_accounts(
        ctx: Context<ReadTrade>,
        request: psi::ResolveRequest,
    ) -> Result<()> {
        let curve = &ctx.accounts.curve;
        let kind = TradeKind::of(&request.trade)?;
        let mut accounts = vec![psi::AccountRef {
            address: kind.output_vault(curve).to_bytes(),
            writable: true,
        }];
        if kind == TradeKind::GachaBuy {
            accounts.push(psi::AccountRef {
                address: curve.pot_vault.to_bytes(),
                writable: true,
            });
            if let Some(trader) = request.trade.trader {
                let trader = Pubkey::new_from_array(trader);
                accounts.push(psi::AccountRef {
                    address: ticket_address(&curve.key(), &trader).to_bytes(),
                    writable: true,
                });
            }
        }
        let result = psi::ResolveResult {
            accounts,
            complete: true,
        };
        set_return_data(&borsh_encode(&result)?);
        Ok(())
    }

    pub fn pipeline_quote(ctx: Context<ReadTrade>, request: psi::TradeRequest) -> Result<()> {
        let quote = ctx.accounts.curve.quote(&request)?.quote;
        set_return_data(&borsh_encode(&quote)?);
        Ok(())
    }

    /// Remaining accounts: the output vault, then for a gacha buy the pot vault and the
    /// trader's open ticket, then any transfer-hook accounts of the quote mint.
    pub fn pipeline_swap<'info>(
        ctx: Context<'info, SwapAccounts<'info>>,
        request: psi::SwapRequest,
    ) -> Result<()> {
        let curve = &ctx.accounts.curve;
        require_keys_eq!(
            ctx.accounts.market_signer.key(),
            curve.market_signer,
            GachaError::NotPipeline
        );
        let trade = &request.trade;
        let kind = TradeKind::of(trade)?;
        require_keys_eq!(
            ctx.accounts.input_vault.key(),
            kind.input_vault(curve),
            GachaError::WrongAccount
        );
        let own_count = kind.own_account_count();
        require!(
            ctx.remaining_accounts.len() >= own_count,
            GachaError::WrongAccount
        );
        let output_vault_info = &ctx.remaining_accounts[0];
        require_keys_eq!(
            output_vault_info.key(),
            kind.output_vault(curve),
            GachaError::WrongAccount
        );
        let hook_accounts = &ctx.remaining_accounts[own_count..];

        let reserve_in = kind.reserve_in(curve);
        let vault_in_balance = token_balance(&ctx.accounts.input_vault.to_account_info())?;
        let received = psi::credited_input(vault_in_balance, reserve_in, request.amount_sent);
        let priced = curve.quote(&psi::TradeRequest {
            amount: received,
            ..trade.clone()
        })?;

        let token_mint_key = curve.token_mint;
        let bump = [curve.bump];
        let curve_seeds: &[&[u8]] = &[b"curve", token_mint_key.as_ref(), &bump];
        let curve_info = ctx.accounts.curve.to_account_info();

        if priced.quote.amount_out > 0 {
            transfer_checked(
                &ctx.accounts.output_token_program.to_account_info(),
                output_vault_info,
                &ctx.accounts.output_mint.to_account_info(),
                &ctx.accounts.trader_output.to_account_info(),
                &curve_info,
                hook_accounts,
                priced.quote.amount_out,
                ctx.accounts.output_mint.decimals,
                &[curve_seeds],
            )?;
        }

        if kind == TradeKind::GachaBuy {
            let pot_vault_info = &ctx.remaining_accounts[1];
            require_keys_eq!(
                pot_vault_info.key(),
                curve.pot_vault,
                GachaError::WrongAccount
            );
            if priced.pot_out > 0 {
                transfer_checked(
                    &ctx.accounts.output_token_program.to_account_info(),
                    output_vault_info,
                    &ctx.accounts.output_mint.to_account_info(),
                    pot_vault_info,
                    &curve_info,
                    &[],
                    priced.pot_out,
                    TOKEN_DECIMALS,
                    &[curve_seeds],
                )?;
            }
            arm_ticket(
                &ctx.remaining_accounts[2],
                &ctx.accounts.curve.key(),
                &ctx.accounts.trader.key(),
                priced.quote.amount_out,
            )?;
        }

        // Move the recorded reserves by the trade itself, not from vault balances, so
        // tokens sent straight to a vault stay out of the price.
        let curve = &mut ctx.accounts.curve;
        match kind {
            TradeKind::Buy | TradeKind::GachaBuy => {
                curve.quote_reserve = curve
                    .quote_reserve
                    .checked_add(priced.quote.amount_in)
                    .ok_or(GachaError::Overflow)?;
                curve.token_reserve -= priced.quote.amount_out + priced.pot_out;
            }
            TradeKind::Sell => {
                curve.token_reserve = curve
                    .token_reserve
                    .checked_add(priced.quote.amount_in)
                    .ok_or(GachaError::Overflow)?;
                curve.quote_reserve -= priced.quote.amount_out;
            }
        }
        Ok(())
    }

    /// Custom action: opens the trader's ticket for this curve. A gacha buy needs an
    /// open, unarmed ticket; the buy arms it. Call this in the same transaction as the
    /// swap, before it.
    pub fn open_ticket(ctx: Context<OpenTicket>) -> Result<()> {
        let ticket = &mut ctx.accounts.ticket;
        ticket.curve = ctx.accounts.curve.key();
        ticket.trader = ctx.accounts.trader.key();
        ticket.armed = false;
        ticket.slot = 0;
        ticket.stake = 0;
        ticket.bump = ctx.bumps.ticket;
        Ok(())
    }

    /// Custom action: closes an unarmed ticket and returns its rent to the trader.
    pub fn close_ticket(ctx: Context<CloseTicket>) -> Result<()> {
        require!(!ctx.accounts.ticket.armed, GachaError::TicketArmed);
        Ok(())
    }

    /// Custom action, callable by anyone: decides an armed ticket and pays a win from
    /// the pot. The roll uses the hash of the first block at or after the ticket's
    /// reveal slot. Tickets not resolved within [`RESOLVE_WINDOW_SLOTS`] of the reveal
    /// slot expire and pay nothing. Either way the ticket closes and its rent goes back
    /// to the trader.
    pub fn resolve_ticket(ctx: Context<ResolveTicket>) -> Result<()> {
        let ticket = &ctx.accounts.ticket;
        let curve = &ctx.accounts.curve;
        require!(ticket.armed, GachaError::TicketNotArmed);

        let reveal_slot = ticket
            .slot
            .checked_add(curve.reveal_delay_slots)
            .ok_or(GachaError::Overflow)?;
        let now = Clock::get()?.slot;
        let expired = now > reveal_slot.saturating_add(RESOLVE_WINDOW_SLOTS);

        let mut outcome = Outcome {
            won: false,
            expired,
            payout: 0,
            slot_used: 0,
        };
        if !expired {
            let (slot_used, hash) = first_slot_hash_at_or_after(
                &ctx.accounts.slot_hashes.to_account_info(),
                reveal_slot,
            )?
            .ok_or(GachaError::TooEarly)?;
            outcome.slot_used = slot_used;
            outcome.won = roll(&hash, &ticket.key(), ticket.slot, curve.win_one_in);
            if outcome.won {
                let pot_balance = ctx.accounts.pot_vault.amount;
                outcome.payout = payout(ticket.stake, pot_balance, curve);
            }
        }

        if outcome.payout > 0 {
            let token_mint_key = curve.token_mint;
            let bump = [curve.bump];
            let curve_seeds: &[&[u8]] = &[b"curve", token_mint_key.as_ref(), &bump];
            transfer_checked(
                &ctx.accounts.token_program.to_account_info(),
                &ctx.accounts.pot_vault.to_account_info(),
                &ctx.accounts.token_mint.to_account_info(),
                &ctx.accounts.trader_token.to_account_info(),
                &ctx.accounts.curve.to_account_info(),
                &[],
                outcome.payout,
                TOKEN_DECIMALS,
                &[curve_seeds],
            )?;
        }

        emit!(TicketResolved {
            ticket: ticket.key(),
            curve: curve.key(),
            trader: ticket.trader,
            stake: ticket.stake,
            won: outcome.won,
            expired: outcome.expired,
            payout: outcome.payout,
            slot_used: outcome.slot_used,
        });
        Ok(())
    }
}

/// Setup data of `pipeline_init_market`, Borsh-encoded.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct InitParams {
    /// Tokens minted to the curve, in base units (6 decimals).
    pub supply: u64,
    /// Virtual quote reserve: sets the opening price together with the supply.
    pub virtual_quote: u64,
    /// Virtual token reserve added to the real one when pricing, so the price stays
    /// finite as the supply sells out.
    pub virtual_token: u64,
    /// Fee on gacha buys, in basis points of the input. Spent on the curve into the pot.
    pub gacha_fee_bps: u16,
    /// A gacha buy wins with probability 1 in this many.
    pub win_one_in: u32,
    /// Win size in basis points of the buyer's base output.
    pub bonus_bps: u16,
    /// Cap on any single win, in basis points of the pot's balance.
    pub max_win_pot_bps: u16,
    /// Slots between a gacha buy and the slot whose block hash decides it.
    pub reveal_delay_slots: u64,
}

impl InitParams {
    fn validate(&self) -> Result<()> {
        require!(self.supply > 0, GachaError::BadInitData);
        require!(self.virtual_quote > 0, GachaError::BadInitData);
        require!((self.gacha_fee_bps as u128) < BPS, GachaError::BadInitData);
        require!(self.win_one_in >= 1, GachaError::BadInitData);
        require!(self.bonus_bps > 0, GachaError::BadInitData);
        require!(
            self.max_win_pot_bps > 0 && (self.max_win_pot_bps as u128) <= BPS,
            GachaError::BadInitData
        );
        require!(self.reveal_delay_slots >= 1, GachaError::BadInitData);
        Ok(())
    }
}

#[account]
#[derive(InitSpace)]
pub struct Curve {
    pub quote_mint: Pubkey,
    pub token_mint: Pubkey,
    pub quote_vault: Pubkey,
    pub token_vault: Pubkey,
    pub pot_vault: Pubkey,
    /// Quote tokens the curve holds from trades.
    pub quote_reserve: u64,
    /// Tokens left for sale in the token vault.
    pub token_reserve: u64,
    pub virtual_quote: u64,
    pub virtual_token: u64,
    pub gacha_fee_bps: u16,
    pub win_one_in: u32,
    pub bonus_bps: u16,
    pub max_win_pot_bps: u16,
    pub reveal_delay_slots: u64,
    /// The Pipeline market PDA allowed to call `pipeline_swap`.
    pub market_signer: Pubkey,
    pub bump: u8,
}

/// A trader's gacha ticket for one curve: one per trader, reused across buys.
#[account]
#[derive(InitSpace)]
pub struct Ticket {
    pub curve: Pubkey,
    pub trader: Pubkey,
    /// Set by a gacha buy; cleared by closing the ticket on resolution.
    pub armed: bool,
    /// Slot of the gacha buy.
    pub slot: u64,
    /// The buyer's base output, which the bonus is a share of.
    pub stake: u64,
    pub bump: u8,
}

#[event]
pub struct TicketResolved {
    pub ticket: Pubkey,
    pub curve: Pubkey,
    pub trader: Pubkey,
    pub stake: u64,
    pub won: bool,
    pub expired: bool,
    pub payout: u64,
    /// The slot whose block hash decided the roll (0 if expired).
    pub slot_used: u64,
}

struct Outcome {
    won: bool,
    expired: bool,
    payout: u64,
    slot_used: u64,
}

/// What a trade is, from its mint indices and `extra`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TradeKind {
    Buy,
    GachaBuy,
    Sell,
}

impl TradeKind {
    fn of(trade: &psi::TradeRequest) -> Result<Self> {
        let gacha = match trade.extra.first().copied() {
            None | Some(0) => false,
            Some(GACHA_FLAG) => true,
            Some(_) => return err!(GachaError::BadExtra),
        };
        match (trade.input_index, trade.output_index) {
            (QUOTE_INDEX, TOKEN_INDEX) if gacha => Ok(TradeKind::GachaBuy),
            (QUOTE_INDEX, TOKEN_INDEX) => Ok(TradeKind::Buy),
            (TOKEN_INDEX, QUOTE_INDEX) if gacha => err!(GachaError::BadExtra),
            (TOKEN_INDEX, QUOTE_INDEX) => Ok(TradeKind::Sell),
            _ => err!(GachaError::WrongAccount),
        }
    }

    fn input_vault(self, curve: &Curve) -> Pubkey {
        match self {
            TradeKind::Buy | TradeKind::GachaBuy => curve.quote_vault,
            TradeKind::Sell => curve.token_vault,
        }
    }

    fn output_vault(self, curve: &Curve) -> Pubkey {
        match self {
            TradeKind::Buy | TradeKind::GachaBuy => curve.token_vault,
            TradeKind::Sell => curve.quote_vault,
        }
    }

    fn reserve_in(self, curve: &Curve) -> u64 {
        match self {
            TradeKind::Buy | TradeKind::GachaBuy => curve.quote_reserve,
            TradeKind::Sell => curve.token_reserve,
        }
    }

    /// Strategy accounts that precede the transfer-hook accounts in `pipeline_swap`.
    fn own_account_count(self) -> usize {
        match self {
            TradeKind::GachaBuy => 3,
            TradeKind::Buy | TradeKind::Sell => 1,
        }
    }
}

/// A priced trade: what the router sees, plus what the gacha fee bought for the pot.
struct Priced {
    quote: psi::QuoteResult,
    pot_out: u64,
}

impl Curve {
    /// The single pricing function used by both `pipeline_quote` and `pipeline_swap`.
    fn quote(&self, trade: &psi::TradeRequest) -> Result<Priced> {
        require!(
            trade.mode == TradeMode::ExactIn,
            GachaError::ExactOutUnsupported
        );
        let kind = TradeKind::of(trade)?;
        let x = self.virtual_quote as u128 + self.quote_reserve as u128;
        let y = self.virtual_token as u128 + self.token_reserve as u128;
        match kind {
            TradeKind::Buy => {
                require!(self.token_reserve > 0, GachaError::InsufficientLiquidity);
                let out = out_given_in(x, y, trade.amount).min(self.token_reserve as u128) as u64;
                Ok(Priced {
                    quote: psi::QuoteResult {
                        amount_in: trade.amount,
                        amount_out: out,
                    },
                    pot_out: 0,
                })
            }
            TradeKind::GachaBuy => {
                require!(self.token_reserve > 0, GachaError::InsufficientLiquidity);
                let fee = fees::ceil_bps(trade.amount, self.gacha_fee_bps as u64)
                    .ok_or(GachaError::Overflow)?;
                let base = trade.amount - fee;
                let out = out_given_in(x, y, base).min(self.token_reserve as u128) as u64;
                // The fee buys from the curve as it stands after the base buy.
                let x_after = x + base as u128;
                let y_after = y - out as u128;
                let left = (self.token_reserve - out) as u128;
                let pot_out = out_given_in(x_after, y_after, fee).min(left) as u64;
                Ok(Priced {
                    quote: psi::QuoteResult {
                        amount_in: trade.amount,
                        amount_out: out,
                    },
                    pot_out,
                })
            }
            TradeKind::Sell => {
                require!(self.quote_reserve > 0, GachaError::InsufficientLiquidity);
                let out = out_given_in(y, x, trade.amount).min(self.quote_reserve as u128) as u64;
                Ok(Priced {
                    quote: psi::QuoteResult {
                        amount_in: trade.amount,
                        amount_out: out,
                    },
                    pot_out: 0,
                })
            }
        }
    }
}

/// `floor(reserve_out × in / (reserve_in + in))` on virtual reserves.
pub fn out_given_in(reserve_in: u128, reserve_out: u128, amount_in: u64) -> u128 {
    let denominator = reserve_in + amount_in as u128;
    if denominator == 0 {
        return 0;
    }
    mul_div(reserve_out, amount_in as u128, denominator).0
}

/// The bonus a winning ticket pays: `bonus_bps` of the stake, capped at
/// `max_win_pot_bps` of the pot, and never more than the pot holds.
pub fn payout(stake: u64, pot_balance: u64, curve: &Curve) -> u64 {
    let bonus = (stake as u128 * curve.bonus_bps as u128 / BPS) as u64;
    let cap = (pot_balance as u128 * curve.max_win_pot_bps as u128 / BPS) as u64;
    bonus.min(cap).min(pot_balance)
}

/// The roll: a 64-bit value from the block hash, the ticket and the buy slot, won when
/// it is a multiple of `win_one_in`.
pub fn roll(hash: &[u8; 32], ticket: &Pubkey, buy_slot: u64, win_one_in: u32) -> bool {
    let digest =
        solana_sha256_hasher::hashv(&[hash, ticket.as_ref(), &buy_slot.to_le_bytes()]).to_bytes();
    let value = u64::from_le_bytes(digest[..8].try_into().unwrap());
    value % win_one_in.max(1) as u64 == 0
}

/// Reads the SlotHashes sysvar for the entry with the smallest slot that is at least
/// `slot`. Entries are stored newest first: `u64` count, then (slot, hash) pairs.
fn first_slot_hash_at_or_after(sysvar: &AccountInfo, slot: u64) -> Result<Option<(u64, [u8; 32])>> {
    let data = sysvar.try_borrow_data()?;
    require!(data.len() >= 8, GachaError::WrongAccount);
    let count = u64::from_le_bytes(data[..8].try_into().unwrap()) as usize;
    require!(data.len() >= 8 + count * 40, GachaError::WrongAccount);
    let mut best: Option<(u64, [u8; 32])> = None;
    for i in (0..count).rev() {
        let start = 8 + i * 40;
        let entry_slot = u64::from_le_bytes(data[start..start + 8].try_into().unwrap());
        if entry_slot >= slot {
            let hash: [u8; 32] = data[start + 8..start + 40].try_into().unwrap();
            best = Some((entry_slot, hash));
            break;
        }
    }
    Ok(best)
}

/// Arms the trader's open ticket with this buy. The ticket must belong to this curve
/// and trader and not be armed already.
fn arm_ticket(info: &AccountInfo, curve: &Pubkey, trader: &Pubkey, stake: u64) -> Result<()> {
    require_keys_eq!(*info.owner, crate::ID, GachaError::WrongAccount);
    require!(info.is_writable, GachaError::WrongAccount);
    let mut ticket = {
        let data = info.try_borrow_data()?;
        Ticket::try_deserialize(&mut &data[..])?
    };
    require_keys_eq!(ticket.curve, *curve, GachaError::WrongAccount);
    require_keys_eq!(ticket.trader, *trader, GachaError::WrongAccount);
    require!(!ticket.armed, GachaError::TicketPending);
    ticket.armed = true;
    ticket.slot = Clock::get()?.slot;
    ticket.stake = stake;
    let mut data = info.try_borrow_mut_data()?;
    let mut cursor: &mut [u8] = &mut data;
    ticket.try_serialize(&mut cursor)?;
    Ok(())
}

pub fn ticket_address(curve: &Pubkey, trader: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"ticket", curve.as_ref(), trader.as_ref()], &crate::ID).0
}

/// `a × b / d` as (quotient, remainder), with a 256-bit product so large reserves can't
/// overflow. The quotient saturates at `u128::MAX`; `d` must not be 0.
fn mul_div(a: u128, b: u128, d: u128) -> (u128, u128) {
    let mask = u64::MAX as u128;
    let (a1, a0) = (a >> 64, a & mask);
    let (b1, b0) = (b >> 64, b & mask);
    let (mid, mid_carry) = (a1 * b0).overflowing_add(a0 * b1);
    let (lo, lo_carry) = (a0 * b0).overflowing_add(mid << 64);
    let hi = a1 * b1 + (mid >> 64) + ((mid_carry as u128) << 64) + lo_carry as u128;
    if hi == 0 {
        return (lo / d, lo % d);
    }
    if hi >= d {
        return (u128::MAX, 0);
    }
    let mut remainder = hi;
    let mut quotient = 0u128;
    for bit in (0..128).rev() {
        let carry = remainder >> 127;
        remainder = (remainder << 1) | ((lo >> bit) & 1);
        if carry == 1 || remainder >= d {
            remainder = remainder.wrapping_sub(d);
            quotient |= 1 << bit;
        }
    }
    (quotient, remainder)
}

/// The Pipeline market PDA for a curve of this program.
fn pipeline_market_signer(curve: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[pipeline_seeds::MARKET, crate::ID.as_ref(), curve.as_ref()],
        &Pubkey::new_from_array(psi::PIPELINE_PROGRAM_ID),
    )
    .0
}

fn borsh_encode<T: AnchorSerialize>(value: &T) -> Result<Vec<u8>> {
    let mut encoded = Vec::new();
    value.serialize(&mut encoded)?;
    Ok(encoded)
}

fn token_balance(account: &AccountInfo) -> Result<u64> {
    let data = account.try_borrow_data()?;
    require!(data.len() >= 72, GachaError::WrongAccount);
    Ok(u64::from_le_bytes(data[64..72].try_into().unwrap()))
}

/// SPL Token `MintTo` (instruction 7).
fn mint_to<'info>(
    token_program: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    amount: u64,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    let mut data = vec![7u8];
    data.extend_from_slice(&amount.to_le_bytes());
    invoke_signed(
        &Instruction {
            program_id: token_program.key(),
            accounts: vec![
                AccountMeta::new(mint.key(), false),
                AccountMeta::new(to.key(), false),
                AccountMeta::new_readonly(authority.key(), true),
            ],
            data,
        },
        &[
            mint.clone(),
            to.clone(),
            authority.clone(),
            token_program.clone(),
        ],
        signer_seeds,
    )?;
    Ok(())
}

/// SPL Token `SetAuthority` (instruction 6) with `MintTokens` (0) set to none.
fn revoke_mint_authority<'info>(
    token_program: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    invoke_signed(
        &Instruction {
            program_id: token_program.key(),
            accounts: vec![
                AccountMeta::new(mint.key(), false),
                AccountMeta::new_readonly(authority.key(), true),
            ],
            data: vec![6u8, 0u8, 0u8],
        },
        &[mint.clone(), authority.clone(), token_program.clone()],
        signer_seeds,
    )?;
    Ok(())
}

/// `transfer_checked` that forwards transfer-hook accounts to the token program.
#[allow(clippy::too_many_arguments)]
fn transfer_checked<'info>(
    token_program: &AccountInfo<'info>,
    from: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    hook_accounts: &[AccountInfo<'info>],
    amount: u64,
    decimals: u8,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    let mut data = vec![12u8];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    let mut metas = vec![
        AccountMeta::new(from.key(), false),
        AccountMeta::new_readonly(mint.key(), false),
        AccountMeta::new(to.key(), false),
        AccountMeta::new_readonly(authority.key(), true),
    ];
    let mut infos = vec![from.clone(), mint.clone(), to.clone(), authority.clone()];
    for account in hook_accounts {
        metas.push(if account.is_writable {
            AccountMeta::new(account.key(), false)
        } else {
            AccountMeta::new_readonly(account.key(), false)
        });
        infos.push(account.clone());
    }
    infos.push(token_program.clone());
    invoke_signed(
        &Instruction {
            program_id: token_program.key(),
            accounts: metas,
            data,
        },
        &infos,
        signer_seeds,
    )?;
    Ok(())
}

/// Accounts passed by the router to `pipeline_init_market`: the four fixed accounts,
/// then this strategy's own. The token mint is a fresh keypair signing `create_market`.
#[derive(Accounts)]
pub struct InitMarket<'info> {
    pub market_signer: Signer<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + Curve::INIT_SPACE,
        seeds = [b"curve", token_mint.key().as_ref()],
        bump,
    )]
    pub curve: Account<'info, Curve>,
    /// The router's setup payer, funded by the market creator.
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub quote_mint: InterfaceAccount<'info, Mint>,
    #[account(
        init,
        payer = payer,
        mint::decimals = TOKEN_DECIMALS,
        mint::authority = curve,
        mint::token_program = token_program,
    )]
    pub token_mint: InterfaceAccount<'info, Mint>,
    #[account(
        init,
        payer = payer,
        seeds = [b"quote_vault", curve.key().as_ref()],
        bump,
        token::mint = quote_mint,
        token::authority = curve,
        token::token_program = quote_token_program,
    )]
    pub quote_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init,
        payer = payer,
        seeds = [b"token_vault", curve.key().as_ref()],
        bump,
        token::mint = token_mint,
        token::authority = curve,
        token::token_program = token_program,
    )]
    pub token_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init,
        payer = payer,
        seeds = [b"pot", curve.key().as_ref()],
        bump,
        token::mint = token_mint,
        token::authority = curve,
        token::token_program = token_program,
    )]
    pub pot_vault: InterfaceAccount<'info, TokenAccount>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct MarketInfoAccounts<'info> {
    pub curve: Account<'info, Curve>,
}

/// The fixed trade accounts, read-only, for quotes and account resolution. Only the
/// curve is read; the others may be placeholders.
#[derive(Accounts)]
pub struct ReadTrade<'info> {
    /// CHECK: unused in reads.
    pub market_signer: UncheckedAccount<'info>,
    pub curve: Account<'info, Curve>,
    /// CHECK: unused in reads.
    pub trader: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub trader_input: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub trader_output: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub input_vault: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub input_mint: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub output_mint: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub input_token_program: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub output_token_program: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct SwapAccounts<'info> {
    pub market_signer: Signer<'info>,
    #[account(mut)]
    pub curve: Account<'info, Curve>,
    /// CHECK: identity only; never a signer here.
    pub trader: UncheckedAccount<'info>,
    /// CHECK: unused; this strategy never refunds.
    #[account(mut)]
    pub trader_input: UncheckedAccount<'info>,
    /// CHECK: receives output; the token program checks it.
    #[account(mut)]
    pub trader_output: UncheckedAccount<'info>,
    /// CHECK: compared with the curve's vault for the input mint.
    #[account(mut)]
    pub input_vault: UncheckedAccount<'info>,
    pub input_mint: InterfaceAccount<'info, Mint>,
    pub output_mint: InterfaceAccount<'info, Mint>,
    pub input_token_program: Interface<'info, TokenInterface>,
    pub output_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct OpenTicket<'info> {
    #[account(mut)]
    pub trader: Signer<'info>,
    pub curve: Account<'info, Curve>,
    #[account(
        init,
        payer = trader,
        space = 8 + Ticket::INIT_SPACE,
        seeds = [b"ticket", curve.key().as_ref(), trader.key().as_ref()],
        bump,
    )]
    pub ticket: Account<'info, Ticket>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CloseTicket<'info> {
    #[account(mut)]
    pub trader: Signer<'info>,
    #[account(mut, has_one = trader, close = trader)]
    pub ticket: Account<'info, Ticket>,
}

#[derive(Accounts)]
pub struct ResolveTicket<'info> {
    #[account(mut, has_one = curve, has_one = trader, close = trader)]
    pub ticket: Account<'info, Ticket>,
    #[account(has_one = pot_vault, has_one = token_mint)]
    pub curve: Account<'info, Curve>,
    /// CHECK: matched against the ticket; receives the ticket's rent.
    #[account(mut)]
    pub trader: UncheckedAccount<'info>,
    #[account(mut)]
    pub pot_vault: InterfaceAccount<'info, TokenAccount>,
    /// Where a win is paid: any token account of the launched mint owned by the trader.
    #[account(mut, token::mint = token_mint, token::authority = trader)]
    pub trader_token: InterfaceAccount<'info, TokenAccount>,
    pub token_mint: InterfaceAccount<'info, Mint>,
    pub token_program: Program<'info, Token>,
    /// CHECK: the SlotHashes sysvar, checked by address.
    #[account(address = SLOT_HASHES_SYSVAR)]
    pub slot_hashes: UncheckedAccount<'info>,
}

#[error_code]
pub enum GachaError {
    #[msg("Init data is not valid InitParams")]
    BadInitData,
    #[msg("Caller is not this curve's Pipeline market")]
    NotPipeline,
    #[msg("Unexpected account")]
    WrongAccount,
    #[msg("Curve cannot provide that much output")]
    InsufficientLiquidity,
    #[msg("This strategy only supports exact-in trades")]
    ExactOutUnsupported,
    #[msg("extra must be empty, [0], or [1] on a buy")]
    BadExtra,
    #[msg("The trader's ticket is still armed; resolve it first")]
    TicketPending,
    #[msg("The ticket is not armed")]
    TicketNotArmed,
    #[msg("An armed ticket can only be closed by resolving it")]
    TicketArmed,
    #[msg("The reveal slot has no block hash yet")]
    TooEarly,
    #[msg("Arithmetic overflow")]
    Overflow,
}
