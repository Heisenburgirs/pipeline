//! Gacha bonding-curve strategy for Pipeline.
//!
//! A market is a new token sold along a constant-product curve with virtual reserves
//! (the pump.fun shape) against a quote token such as wrapped SOL. The strategy mints
//! the whole supply into its own vault at market creation and then gives up the mint
//! authority, so the supply is fixed. Buys move quote into the curve and tokens out;
//! sells do the reverse. Every trade reprices the curve.
//!
//! Every trade pays a fee from its input, split four ways. `pot_fee_bps` goes to the
//! **pot**: on a buy it is spent on the curve and the tokens it buys go to the pot, on a
//! sell it is taken in tokens and moved to the pot directly. `curve_fee_bps` stays in
//! the curve's own reserves (quote on a buy, tokens on a sell), lifting the price for
//! every holder. `creator_fee_bps` accrues for the market's creator and
//! [`PROTOCOL_FEE_BPS`] for whoever holds this program's upgrade authority, the
//! launchpad; both sit in the vaults until claimed with `claim_fees`. The pot is what
//! gacha wins are paid from, so all trading funds the prizes.
//!
//! The gacha is opt-in per buy and costs nothing extra. A buyer who sets the first byte
//! of the trade's `extra` to 1 has their ticket armed with the slot of the buy and a
//! fresh randomness **seed**. The buyer, or anyone, then asks the ORAO VRF for that
//! seed, and once the oracles have fulfilled it anyone may resolve the ticket: the
//! verifiable randomness decides the roll. A 1-in-`win_one_in` win pays `bonus_bps` of
//! the buyer's base output from the pot, capped at `max_win_pot_bps` of the pot. A rarer
//! 1-in-`jackpot_one_in` **jackpot** pays `jackpot_bps` of the base output, a multiple
//! of it rather than a share, capped at `max_jackpot_pot_bps` of the pot.
//!
//! Why a separate reveal: a roll decided inside the buy transaction can be filtered for
//! free. A trader sets `min_amount_out` to the jackpot and lets losing rolls revert,
//! paying the fee only when they win. Deciding with randomness that does not exist yet
//! at buy time closes that. A ticket's seed is fixed by the ticket, a nonce it got when
//! it was opened and its round counter, and the buy refuses to arm when that seed has
//! already been requested, so nobody can learn an outcome before committing. Resolving within
//! `resolve_window_slots` of the buy stops a known winner from waiting for a bigger pot.
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
/// Mint index of the quote token (what buyers pay with) and of the launched token.
pub const QUOTE_INDEX: u8 = 0;
pub const TOKEN_INDEX: u8 = 1;
/// The ORAO VRF program, whose fulfilled randomness decides tickets.
pub const ORAO_VRF_PROGRAM: Pubkey = pubkey!("VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y");
/// Seed prefix of ORAO randomness request accounts: `[prefix, seed]` under the VRF
/// program.
pub const ORAO_RANDOMNESS_SEED: &[u8] = b"orao-vrf-randomness-request";
/// Anchor discriminator of ORAO's `RandomnessV2` account.
pub const ORAO_RANDOMNESS_V2_DISCRIMINATOR: [u8; 8] = [139, 239, 184, 215, 227, 86, 191, 226];
/// Fee on every trade, in basis points of the input, for the program's upgrade
/// authority (the launchpad). Compiled in, so a market creator cannot switch it off.
pub const PROTOCOL_FEE_BPS: u16 = 10;
/// Cap on the creator's fee.
pub const MAX_CREATOR_FEE_BPS: u16 = 500;
/// The upgradeable BPF loader, owner of every upgradeable program's ProgramData.
pub const BPF_LOADER_UPGRADEABLE: Pubkey = pubkey!("BPFLoaderUpgradeab1e11111111111111111111111");

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
        curve.win_one_in = params.win_one_in;
        curve.bonus_bps = params.bonus_bps;
        curve.max_win_pot_bps = params.max_win_pot_bps;
        curve.resolve_window_slots = params.resolve_window_slots;
        curve.pot_fee_bps = params.pot_fee_bps;
        curve.curve_fee_bps = params.curve_fee_bps;
        curve.creator_fee_bps = params.creator_fee_bps;
        curve.creator = params.creator;
        curve.creator_owed_quote = 0;
        curve.creator_owed_token = 0;
        curve.protocol_owed_quote = 0;
        curve.protocol_owed_token = 0;
        curve.jackpot_one_in = params.jackpot_one_in;
        curve.jackpot_bps = params.jackpot_bps;
        curve.max_jackpot_pot_bps = params.max_jackpot_pot_bps;
        curve.ticket_nonce = 0;
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

    /// Strategy accounts for a trade: the output vault and the pot vault, and for a
    /// gacha buy also the trader's ticket and, in a second round once the ticket's data
    /// is in hand, the ORAO randomness account for the ticket's next seed.
    pub fn pipeline_resolve_accounts(
        ctx: Context<ReadTrade>,
        request: psi::ResolveRequest,
    ) -> Result<()> {
        let curve = &ctx.accounts.curve;
        let kind = TradeKind::of(&request.trade)?;
        let mut accounts = vec![
            psi::AccountRef {
                address: kind.output_vault(curve).to_bytes(),
                writable: true,
            },
            psi::AccountRef {
                address: curve.pot_vault.to_bytes(),
                writable: true,
            },
        ];
        let mut complete = true;
        if kind == TradeKind::GachaBuy {
            if let Some(trader) = request.trade.trader {
                let trader = Pubkey::new_from_array(trader);
                let ticket = ticket_address(&curve.key(), &trader);
                accounts.push(psi::AccountRef {
                    address: ticket.to_bytes(),
                    writable: true,
                });
                if request.round == 0 {
                    complete = false;
                } else {
                    // The accounts of round 0 follow the fixed ones; the ticket is third.
                    // A ticket not opened yet (opened in the swap's transaction) will get
                    // the curve's next nonce and start at round 0.
                    let (nonce, rounds) = ctx
                        .remaining_accounts
                        .get(2)
                        .and_then(ticket_nonce_and_rounds)
                        .unwrap_or((curve.ticket_nonce.saturating_add(1), 0));
                    let seed = ticket_seed(&ticket, nonce, rounds.saturating_add(1));
                    accounts.push(psi::AccountRef {
                        address: randomness_address(&seed).to_bytes(),
                        writable: false,
                    });
                }
            }
        }
        let result = psi::ResolveResult { accounts, complete };
        set_return_data(&borsh_encode(&result)?);
        Ok(())
    }

    pub fn pipeline_quote(ctx: Context<ReadTrade>, request: psi::TradeRequest) -> Result<()> {
        let quote = ctx.accounts.curve.quote(&request)?.quote;
        set_return_data(&borsh_encode(&quote)?);
        Ok(())
    }

    /// Remaining accounts: the output vault, the pot vault, for a gacha buy the trader's
    /// open ticket and the ORAO randomness account for its next seed (which must not
    /// exist yet), then any transfer-hook accounts of the quote mint.
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

        // Unclaimed fees sit in the vault next to the reserve; neither is the trade's.
        let held_in = kind
            .reserve_in(curve)
            .checked_add(kind.owed_in(curve))
            .ok_or(GachaError::Overflow)?;
        let vault_in_balance = token_balance(&ctx.accounts.input_vault.to_account_info())?;
        let received = psi::credited_input(vault_in_balance, held_in, request.amount_sent);
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

        let pot_vault_info = &ctx.remaining_accounts[1];
        require_keys_eq!(
            pot_vault_info.key(),
            curve.pot_vault,
            GachaError::WrongAccount
        );
        // On a buy the fee bought tokens from the token vault; on a sell the fee is part
        // of the tokens the trader sent to the token vault. Either way: vault to pot.
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
        if priced.pot_in > 0 {
            transfer_checked(
                &ctx.accounts.input_token_program.to_account_info(),
                &ctx.accounts.input_vault.to_account_info(),
                &ctx.accounts.input_mint.to_account_info(),
                pot_vault_info,
                &curve_info,
                &[],
                priced.pot_in,
                TOKEN_DECIMALS,
                &[curve_seeds],
            )?;
        }

        if kind == TradeKind::GachaBuy {
            arm_ticket(
                &ctx.remaining_accounts[2],
                &ctx.remaining_accounts[3],
                &ctx.accounts.curve.key(),
                &ctx.accounts.trader.key(),
                priced.quote.amount_out,
            )?;
        }

        // Move the recorded reserves by the trade itself, not from vault balances, so
        // tokens sent straight to a vault stay out of the price.
        let curve = &mut ctx.accounts.curve;
        let claimable = priced.creator_fee + priced.protocol_fee;
        match kind {
            TradeKind::Buy | TradeKind::GachaBuy => {
                curve.quote_reserve = curve
                    .quote_reserve
                    .checked_add(priced.quote.amount_in - claimable)
                    .ok_or(GachaError::Overflow)?;
                curve.token_reserve -= priced.quote.amount_out + priced.pot_out;
                curve.creator_owed_quote = curve
                    .creator_owed_quote
                    .checked_add(priced.creator_fee)
                    .ok_or(GachaError::Overflow)?;
                curve.protocol_owed_quote = curve
                    .protocol_owed_quote
                    .checked_add(priced.protocol_fee)
                    .ok_or(GachaError::Overflow)?;
            }
            TradeKind::Sell => {
                curve.token_reserve = curve
                    .token_reserve
                    .checked_add(priced.quote.amount_in - priced.pot_in - claimable)
                    .ok_or(GachaError::Overflow)?;
                curve.quote_reserve -= priced.quote.amount_out;
                curve.creator_owed_token = curve
                    .creator_owed_token
                    .checked_add(priced.creator_fee)
                    .ok_or(GachaError::Overflow)?;
                curve.protocol_owed_token = curve
                    .protocol_owed_token
                    .checked_add(priced.protocol_fee)
                    .ok_or(GachaError::Overflow)?;
            }
        }
        Ok(())
    }

    /// Custom action: opens the trader's ticket for this curve. A gacha buy needs an
    /// open, unarmed ticket; the buy arms it. Call this in the same transaction as the
    /// swap, before it.
    pub fn open_ticket(ctx: Context<OpenTicket>) -> Result<()> {
        let curve = &mut ctx.accounts.curve;
        curve.ticket_nonce = curve
            .ticket_nonce
            .checked_add(1)
            .ok_or(GachaError::Overflow)?;
        let nonce = curve.ticket_nonce;
        let ticket = &mut ctx.accounts.ticket;
        ticket.nonce = nonce;
        ticket.curve = ctx.accounts.curve.key();
        ticket.trader = ctx.accounts.trader.key();
        ticket.armed = false;
        ticket.slot = 0;
        ticket.stake = 0;
        ticket.rounds = 0;
        ticket.seed = [0; 32];
        ticket.bump = ctx.bumps.ticket;
        Ok(())
    }

    /// Custom action: closes an unarmed ticket and returns its rent to the trader.
    pub fn close_ticket(ctx: Context<CloseTicket>) -> Result<()> {
        require!(!ctx.accounts.ticket.armed, GachaError::TicketArmed);
        Ok(())
    }

    /// Custom action, callable by anyone: decides an armed ticket from the ORAO
    /// randomness fulfilled for its seed and pays a win from the pot. Tickets not
    /// resolved within `resolve_window_slots` of their buy expire and pay nothing.
    /// Either way the ticket closes and its rent goes back to the trader.
    pub fn resolve_ticket(ctx: Context<ResolveTicket>) -> Result<()> {
        let ticket = &ctx.accounts.ticket;
        let curve = &ctx.accounts.curve;
        require!(ticket.armed, GachaError::TicketNotArmed);
        let randomness_info = ctx.accounts.randomness.to_account_info();
        require_keys_eq!(
            randomness_info.key(),
            randomness_address(&ticket.seed),
            GachaError::WrongAccount
        );

        let now = Clock::get()?.slot;
        let expired = now > ticket.slot.saturating_add(curve.resolve_window_slots);

        let mut outcome = Outcome {
            won: false,
            jackpot: false,
            expired,
            payout: 0,
            resolved_slot: now,
        };
        if !expired {
            let randomness = fulfilled_randomness(&randomness_info, &ticket.seed)?
                .ok_or(GachaError::NotFulfilled)?;
            let tier = roll(&randomness, curve);
            outcome.won = tier != Tier::Lose;
            outcome.jackpot = tier == Tier::Jackpot;
            if outcome.won {
                let pot_balance = ctx.accounts.pot_vault.amount;
                outcome.payout = payout(ticket.stake, pot_balance, curve, tier);
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
            jackpot: outcome.jackpot,
            expired: outcome.expired,
            payout: outcome.payout,
            resolved_slot: outcome.resolved_slot,
        });
        Ok(())
    }

    /// Custom action: pays the claimant's accrued fees in both mints to the token
    /// accounts given. The market's creator claims the creator fee; the program's
    /// upgrade authority claims the protocol fee. Remaining accounts: transfer-hook
    /// accounts of the quote mint, if any.
    pub fn claim_fees<'info>(ctx: Context<'info, ClaimFees<'info>>) -> Result<()> {
        let curve = &ctx.accounts.curve;
        let claimant = ctx.accounts.claimant.key();
        let is_creator = claimant == curve.creator;
        let is_protocol = {
            let data = ctx.accounts.program_data.try_borrow_data()?;
            upgrade_authority(&data) == Some(Some(claimant))
        };
        require!(is_creator || is_protocol, GachaError::NotFeeRecipient);

        let mut quote = 0u64;
        let mut token = 0u64;
        if is_creator {
            quote += curve.creator_owed_quote;
            token += curve.creator_owed_token;
        }
        if is_protocol {
            quote += curve.protocol_owed_quote;
            token += curve.protocol_owed_token;
        }

        let token_mint_key = curve.token_mint;
        let bump = [curve.bump];
        let curve_seeds: &[&[u8]] = &[b"curve", token_mint_key.as_ref(), &bump];
        let curve_info = ctx.accounts.curve.to_account_info();
        if quote > 0 {
            transfer_checked(
                &ctx.accounts.quote_token_program.to_account_info(),
                &ctx.accounts.quote_vault.to_account_info(),
                &ctx.accounts.quote_mint.to_account_info(),
                &ctx.accounts.claimant_quote.to_account_info(),
                &curve_info,
                ctx.remaining_accounts,
                quote,
                ctx.accounts.quote_mint.decimals,
                &[curve_seeds],
            )?;
        }
        if token > 0 {
            transfer_checked(
                &ctx.accounts.token_program.to_account_info(),
                &ctx.accounts.token_vault.to_account_info(),
                &ctx.accounts.token_mint.to_account_info(),
                &ctx.accounts.claimant_token.to_account_info(),
                &curve_info,
                &[],
                token,
                TOKEN_DECIMALS,
                &[curve_seeds],
            )?;
        }

        let curve = &mut ctx.accounts.curve;
        if is_creator {
            curve.creator_owed_quote = 0;
            curve.creator_owed_token = 0;
        }
        if is_protocol {
            curve.protocol_owed_quote = 0;
            curve.protocol_owed_token = 0;
        }
        emit!(FeesClaimed {
            curve: curve.key(),
            claimant,
            quote,
            token,
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
    /// A gacha buy wins with probability 1 in this many.
    pub win_one_in: u32,
    /// Win size in basis points of the buyer's base output.
    pub bonus_bps: u16,
    /// Cap on any single win, in basis points of the pot's balance.
    pub max_win_pot_bps: u16,
    /// A ticket must be resolved within this many slots of its buy, or it pays nothing.
    /// The VRF fulfils in seconds; the window stops a known winner from waiting for a
    /// bigger pot.
    pub resolve_window_slots: u64,
    /// Fee on every trade, in basis points of the input, paid into the pot.
    pub pot_fee_bps: u16,
    /// Fee on every trade, in basis points of the input, left in the curve's reserves.
    pub curve_fee_bps: u16,
    /// Fee on every trade, in basis points of the input, for `creator`. At most
    /// [`MAX_CREATOR_FEE_BPS`]. [`PROTOCOL_FEE_BPS`] is charged on top.
    pub creator_fee_bps: u16,
    /// Who claims the creator fee.
    pub creator: Pubkey,
    /// A gacha buy hits the jackpot with probability 1 in this many; 0 disables it.
    pub jackpot_one_in: u32,
    /// Jackpot size in basis points of the buyer's base output (1,000,000 = 100x).
    pub jackpot_bps: u32,
    /// Cap on a jackpot, in basis points of the pot's balance.
    pub max_jackpot_pot_bps: u16,
}

impl InitParams {
    fn validate(&self) -> Result<()> {
        require!(self.supply > 0, GachaError::BadInitData);
        require!(self.virtual_quote > 0, GachaError::BadInitData);
        require!(
            self.creator_fee_bps <= MAX_CREATOR_FEE_BPS,
            GachaError::BadInitData
        );
        require!(
            (self.pot_fee_bps as u128
                + self.curve_fee_bps as u128
                + self.creator_fee_bps as u128
                + PROTOCOL_FEE_BPS as u128)
                < BPS,
            GachaError::BadInitData
        );
        require!(self.win_one_in >= 1, GachaError::BadInitData);
        require!(self.bonus_bps > 0, GachaError::BadInitData);
        require!(
            self.max_win_pot_bps > 0 && (self.max_win_pot_bps as u128) <= BPS,
            GachaError::BadInitData
        );
        require!(self.resolve_window_slots >= 1, GachaError::BadInitData);
        require!(
            (self.max_jackpot_pot_bps as u128) <= BPS,
            GachaError::BadInitData
        );
        require!(
            self.jackpot_one_in == 0 || (self.jackpot_bps > 0 && self.max_jackpot_pot_bps > 0),
            GachaError::BadInitData
        );
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
    pub win_one_in: u32,
    pub bonus_bps: u16,
    pub max_win_pot_bps: u16,
    pub resolve_window_slots: u64,
    /// Fee on every trade, paid into the pot.
    pub pot_fee_bps: u16,
    /// Fee on every trade, left in the curve's reserves.
    pub curve_fee_bps: u16,
    /// Fee on every trade, accrued for `creator`.
    pub creator_fee_bps: u16,
    /// Claims the creator fee.
    pub creator: Pubkey,
    /// Fees accrued and not yet claimed. They sit in the vaults next to the reserves.
    pub creator_owed_quote: u64,
    pub creator_owed_token: u64,
    pub protocol_owed_quote: u64,
    pub protocol_owed_token: u64,
    /// Jackpot odds, size and cap; see [`InitParams`].
    pub jackpot_one_in: u32,
    pub jackpot_bps: u32,
    pub max_jackpot_pot_bps: u16,
    /// Tickets opened so far; each gets the next value as its nonce, so that seeds
    /// never repeat even when a ticket is closed and opened again.
    pub ticket_nonce: u64,
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
    /// How many times the ticket has been armed; names the seed of each round.
    pub rounds: u32,
    /// The ORAO randomness seed of the current round:
    /// `ticket_seed(ticket, nonce, rounds)`.
    pub seed: [u8; 32],
    /// Assigned from the curve's counter when the ticket was opened.
    pub nonce: u64,
    pub bump: u8,
}

#[event]
pub struct FeesClaimed {
    pub curve: Pubkey,
    pub claimant: Pubkey,
    pub quote: u64,
    pub token: u64,
}

#[event]
pub struct TicketResolved {
    pub ticket: Pubkey,
    pub curve: Pubkey,
    pub trader: Pubkey,
    pub stake: u64,
    pub won: bool,
    /// Set with `won` when the roll hit the jackpot tier.
    pub jackpot: bool,
    pub expired: bool,
    pub payout: u64,
    /// The slot the ticket was resolved in.
    pub resolved_slot: u64,
}

struct Outcome {
    won: bool,
    jackpot: bool,
    expired: bool,
    payout: u64,
    resolved_slot: u64,
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

    /// Unclaimed fees held in the input vault.
    fn owed_in(self, curve: &Curve) -> u64 {
        match self {
            TradeKind::Buy | TradeKind::GachaBuy => {
                curve.creator_owed_quote + curve.protocol_owed_quote
            }
            TradeKind::Sell => curve.creator_owed_token + curve.protocol_owed_token,
        }
    }

    /// Strategy accounts that precede the transfer-hook accounts in `pipeline_swap`.
    fn own_account_count(self) -> usize {
        match self {
            TradeKind::GachaBuy => 4,
            TradeKind::Buy | TradeKind::Sell => 2,
        }
    }
}

/// A priced trade: what the router sees, what the pot receives (tokens the pot fee
/// bought from the token vault on a buy, `pot_out`, or the pot fee in tokens on a sell,
/// `pot_in`), and the fees accrued for the creator and the protocol in the input mint.
struct Priced {
    quote: psi::QuoteResult,
    pot_out: u64,
    pot_in: u64,
    creator_fee: u64,
    protocol_fee: u64,
}

/// The four fees on one trade, each rounded up.
struct Fees {
    pot: u64,
    curve: u64,
    creator: u64,
    protocol: u64,
}

impl Fees {
    fn total(&self) -> u64 {
        self.pot + self.curve + self.creator + self.protocol
    }
}

impl Curve {
    /// The fees on an input of `amount`.
    fn fees_on(&self, amount: u64) -> Result<Fees> {
        let bps = |rate: u16| fees::ceil_bps(amount, rate as u64).ok_or(GachaError::Overflow);
        let fees = Fees {
            pot: bps(self.pot_fee_bps)?,
            curve: bps(self.curve_fee_bps)?,
            creator: bps(self.creator_fee_bps)?,
            protocol: bps(PROTOCOL_FEE_BPS)?,
        };
        require!(fees.total() <= amount, GachaError::InsufficientLiquidity);
        Ok(fees)
    }

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
            TradeKind::Buy | TradeKind::GachaBuy => {
                require!(self.token_reserve > 0, GachaError::InsufficientLiquidity);
                // All four fees come off the input; a gacha buy pays nothing extra. The
                // pot fee buys for the pot from the curve as it stands after the base
                // buy, the curve fee just stays in the quote reserve, and the creator's
                // and protocol's accrue in the quote vault until claimed.
                let fees = self.fees_on(trade.amount)?;
                let base = trade.amount - fees.total();
                let out = out_given_in(x, y, base).min(self.token_reserve as u128) as u64;
                let x_after = x + base as u128;
                let y_after = y - out as u128;
                let left = (self.token_reserve - out) as u128;
                let pot_out = out_given_in(x_after, y_after, fees.pot).min(left) as u64;
                Ok(Priced {
                    quote: psi::QuoteResult {
                        amount_in: trade.amount,
                        amount_out: out,
                    },
                    pot_out,
                    pot_in: 0,
                    creator_fee: fees.creator,
                    protocol_fee: fees.protocol,
                })
            }
            TradeKind::Sell => {
                require!(self.quote_reserve > 0, GachaError::InsufficientLiquidity);
                // All four fees are taken in tokens: the pot fee goes to the pot, the
                // curve fee stays in the token reserve unpaid, the creator's and
                // protocol's accrue in the token vault, and the rest sells.
                let fees = self.fees_on(trade.amount)?;
                let net = trade.amount - fees.total();
                let out = out_given_in(y, x, net).min(self.quote_reserve as u128) as u64;
                Ok(Priced {
                    quote: psi::QuoteResult {
                        amount_in: trade.amount,
                        amount_out: out,
                    },
                    pot_out: 0,
                    pot_in: fees.pot,
                    creator_fee: fees.creator,
                    protocol_fee: fees.protocol,
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

/// What a ticket's roll came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Lose,
    Win,
    Jackpot,
}

/// What a ticket pays: the win or jackpot share of the stake, capped at the tier's
/// share of the pot, and never more than the pot holds.
pub fn payout(stake: u64, pot_balance: u64, curve: &Curve, tier: Tier) -> u64 {
    let (prize_bps, cap_bps) = match tier {
        Tier::Lose => return 0,
        Tier::Win => (curve.bonus_bps as u128, curve.max_win_pot_bps as u128),
        Tier::Jackpot => (curve.jackpot_bps as u128, curve.max_jackpot_pot_bps as u128),
    };
    let prize = (stake as u128 * prize_bps / BPS).min(u64::MAX as u128) as u64;
    let cap = (pot_balance as u128 * cap_bps / BPS) as u64;
    prize.min(cap).min(pot_balance)
}

/// The roll: the first 8 bytes of the fulfilled randomness as a little-endian `u64`. A
/// multiple of `jackpot_one_in` is a jackpot, else a multiple of `win_one_in` is a win.
pub fn roll(randomness: &[u8; 64], curve: &Curve) -> Tier {
    let value = u64::from_le_bytes(randomness[..8].try_into().unwrap());
    if curve.jackpot_one_in > 0 && value % curve.jackpot_one_in as u64 == 0 {
        Tier::Jackpot
    } else if value % curve.win_one_in.max(1) as u64 == 0 {
        Tier::Win
    } else {
        Tier::Lose
    }
}

/// The randomness seed of the `round`-th gacha buy of a ticket opened with `nonce`.
pub fn ticket_seed(ticket: &Pubkey, nonce: u64, round: u32) -> [u8; 32] {
    solana_sha256_hasher::hashv(&[
        b"gacha-vrf",
        ticket.as_ref(),
        &nonce.to_le_bytes(),
        &round.to_le_bytes(),
    ])
    .to_bytes()
}

/// The ORAO randomness request account for a seed.
pub fn randomness_address(seed: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[ORAO_RANDOMNESS_SEED, seed], &ORAO_VRF_PROGRAM).0
}

/// A ticket's nonce and how many rounds it has been armed, or `None` if the account
/// is not a ticket of this program (for example not opened yet).
fn ticket_nonce_and_rounds(info: &AccountInfo) -> Option<(u64, u32)> {
    if *info.owner != crate::ID {
        return None;
    }
    let data = info.try_borrow_data().ok()?;
    Ticket::try_deserialize(&mut &data[..])
        .ok()
        .map(|ticket| (ticket.nonce, ticket.rounds))
}

/// Reads ORAO's `RandomnessV2` account for `seed`: `Ok(None)` while it does not exist
/// or is still pending, the 64 bytes of randomness once fulfilled.
fn fulfilled_randomness(info: &AccountInfo, seed: &[u8; 32]) -> Result<Option<[u8; 64]>> {
    if info.data_is_empty() {
        return Ok(None);
    }
    require_keys_eq!(*info.owner, ORAO_VRF_PROGRAM, GachaError::WrongAccount);
    let data = info.try_borrow_data()?;
    require!(
        data.len() >= 9 && data[..8] == ORAO_RANDOMNESS_V2_DISCRIMINATOR,
        GachaError::WrongAccount
    );
    // RequestAccount enum: 0 = Pending, 1 = Fulfilled { client, seed, randomness }.
    if data[8] != 1 {
        return Ok(None);
    }
    require!(data.len() >= 9 + 32 + 32 + 64, GachaError::WrongAccount);
    require!(&data[41..73] == seed, GachaError::WrongAccount);
    Ok(Some(data[73..137].try_into().unwrap()))
}

/// Arms the trader's open ticket with this buy. The ticket must belong to this curve
/// and trader and not be armed already, and the ORAO randomness account for its next
/// seed must not exist yet, so the outcome cannot have been learned before the buy.
fn arm_ticket(
    info: &AccountInfo,
    randomness: &AccountInfo,
    curve: &Pubkey,
    trader: &Pubkey,
    stake: u64,
) -> Result<()> {
    require_keys_eq!(*info.owner, crate::ID, GachaError::WrongAccount);
    require!(info.is_writable, GachaError::WrongAccount);
    let mut ticket = {
        let data = info.try_borrow_data()?;
        Ticket::try_deserialize(&mut &data[..])?
    };
    require_keys_eq!(ticket.curve, *curve, GachaError::WrongAccount);
    require_keys_eq!(ticket.trader, *trader, GachaError::WrongAccount);
    require!(!ticket.armed, GachaError::TicketPending);
    let round = ticket.rounds.checked_add(1).ok_or(GachaError::Overflow)?;
    let seed = ticket_seed(&info.key(), ticket.nonce, round);
    require_keys_eq!(
        randomness.key(),
        randomness_address(&seed),
        GachaError::WrongAccount
    );
    require!(randomness.data_is_empty(), GachaError::SeedAlreadyRequested);
    ticket.armed = true;
    ticket.slot = Clock::get()?.slot;
    ticket.stake = stake;
    ticket.rounds = round;
    ticket.seed = seed;
    let mut data = info.try_borrow_mut_data()?;
    let mut cursor: &mut [u8] = &mut data;
    ticket.try_serialize(&mut cursor)?;
    Ok(())
}

/// Reads the upgrade authority from `UpgradeableLoaderState::ProgramData`
/// (bincode: u32 tag 3, u64 slot, Option<Pubkey>). `Some(None)` means immutable.
fn upgrade_authority(data: &[u8]) -> Option<Option<Pubkey>> {
    if data.len() < 13 || data[..4] != 3u32.to_le_bytes() {
        return None;
    }
    match data[12] {
        0 => Some(None),
        1 if data.len() >= 45 => Some(Some(Pubkey::new_from_array(data[13..45].try_into().ok()?))),
        _ => None,
    }
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
    #[account(mut)]
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
    /// CHECK: the ORAO randomness account for the ticket's seed, checked by address and
    /// parsed in the handler.
    pub randomness: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct ClaimFees<'info> {
    pub claimant: Signer<'info>,
    #[account(mut, has_one = quote_mint, has_one = token_mint, has_one = quote_vault, has_one = token_vault)]
    pub curve: Account<'info, Curve>,
    #[account(mut)]
    pub quote_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub token_vault: InterfaceAccount<'info, TokenAccount>,
    /// Where the quote-side fees go: any token account of the quote mint.
    #[account(mut, token::mint = quote_mint, token::token_program = quote_token_program)]
    pub claimant_quote: InterfaceAccount<'info, TokenAccount>,
    /// Where the token-side fees go: any token account of the launched mint.
    #[account(mut, token::mint = token_mint, token::token_program = token_program)]
    pub claimant_token: InterfaceAccount<'info, TokenAccount>,
    pub quote_mint: InterfaceAccount<'info, Mint>,
    pub token_mint: InterfaceAccount<'info, Mint>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub token_program: Program<'info, Token>,
    /// CHECK: this program's ProgramData, checked by address and owner; names the
    /// upgrade authority that may claim the protocol fee.
    #[account(
        owner = BPF_LOADER_UPGRADEABLE,
        seeds = [crate::ID.as_ref()],
        bump,
        seeds::program = BPF_LOADER_UPGRADEABLE,
    )]
    pub program_data: UncheckedAccount<'info>,
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
    #[msg("The ticket's randomness has not been requested from the VRF, or not fulfilled yet")]
    NotFulfilled,
    #[msg("Randomness for this ticket's next seed was requested before the buy")]
    SeedAlreadyRequested,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("Only the market's creator or the program's upgrade authority may claim fees")]
    NotFeeRecipient,
}
