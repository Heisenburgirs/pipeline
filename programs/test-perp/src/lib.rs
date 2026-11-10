//! A minimal perpetuals venue, shaped like a real one and meant for tests and devnet
//! demos. It is what `programs/perp-curve` holds its position on.
//!
//! - A **market** (`["market", symbol]`, for example `SOL-PERP`) quotes one base asset in
//!   one collateral mint. Its price is set by the market's authority with `set_price`;
//!   on a real venue it would come from an oracle.
//! - A **position** (`["position", market, owner]`) is long or short at a fixed
//!   leverage. `increase` deposits collateral and adds `collateral × leverage / price`
//!   of base at the current price, averaging the entry price; `decrease` closes a
//!   fraction of the base and pays that fraction of the position's **equity**,
//!   collateral plus profit or loss at the current price, from the market's vault. A
//!   position whose equity has gone to zero is wiped on its next decrease.
//!
//! There is no funding, no fee, no margin call before equity reaches zero, and no
//! counterparty: the vault pays winners out of what everyone deposited, so a demo seeds
//! it with extra collateral. None of that matters to a strategy built on it, which only
//! needs open, add, close a share, and the account layouts.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("8FcD14vCkywa5zPAMmy3g9VpnESJYUZM36c6hzJBdsYi");

/// Base amounts are in millionths of the base asset; a price is collateral base units
/// per whole base asset.
pub const BASE_UNIT: u64 = 1_000_000;
pub const LONG: u8 = 0;
pub const SHORT: u8 = 1;
pub const MAX_LEVERAGE: u8 = 20;

#[program]
pub mod test_perp {
    use super::*;

    /// Creates a market quoting `symbol` in `collateral_mint`, at `price`.
    pub fn create_market(ctx: Context<CreateMarket>, symbol: [u8; 16], price: u64) -> Result<()> {
        require!(price > 0, PerpError::BadPrice);
        let market = &mut ctx.accounts.market;
        market.authority = ctx.accounts.authority.key();
        market.symbol = symbol;
        market.collateral_mint = ctx.accounts.collateral_mint.key();
        market.vault = ctx.accounts.vault.key();
        market.price = price;
        market.price_slot = Clock::get()?.slot;
        market.bump = ctx.bumps.market;
        Ok(())
    }

    /// Sets the market's price. The authority stands in for an oracle.
    pub fn set_price(ctx: Context<SetPrice>, price: u64) -> Result<()> {
        require!(price > 0, PerpError::BadPrice);
        let market = &mut ctx.accounts.market;
        market.price = price;
        market.price_slot = Clock::get()?.slot;
        Ok(())
    }

    /// Opens an empty position for `owner` on the market: long or short, at `leverage`.
    /// `payer` funds the account, so a program-owned `owner` can have one.
    pub fn open_position(ctx: Context<OpenPosition>, direction: u8, leverage: u8) -> Result<()> {
        require!(
            direction == LONG || direction == SHORT,
            PerpError::BadDirection
        );
        require!(
            (1..=MAX_LEVERAGE).contains(&leverage),
            PerpError::BadLeverage
        );
        let position = &mut ctx.accounts.position;
        position.market = ctx.accounts.market.key();
        position.owner = ctx.accounts.owner.key();
        position.direction = direction;
        position.leverage = leverage;
        position.collateral = 0;
        position.base = 0;
        position.entry_price = 0;
        position.bump = ctx.bumps.position;
        Ok(())
    }

    /// Deposits `amount` of collateral and adds `amount × leverage / price` of base at
    /// the current price, averaging the entry price into the position.
    pub fn increase(ctx: Context<Increase>, amount: u64) -> Result<()> {
        require!(amount > 0, PerpError::Nothing);
        let price = ctx.accounts.market.price;
        let position = &mut ctx.accounts.position;
        let added = (amount as u128)
            .checked_mul(position.leverage as u128)
            .and_then(|n| n.checked_mul(BASE_UNIT as u128))
            .map(|n| n / price as u128)
            .ok_or(PerpError::Overflow)?;
        require!(added > 0, PerpError::Nothing);
        let base = position.base as u128;
        // Weighted average entry over the old and new base.
        let entry = (base * position.entry_price as u128 + added * price as u128) / (base + added);
        position.entry_price = u64::try_from(entry).map_err(|_| PerpError::Overflow)?;
        position.base = u64::try_from(base + added).map_err(|_| PerpError::Overflow)?;
        position.collateral = position
            .collateral
            .checked_add(amount)
            .ok_or(PerpError::Overflow)?;

        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.owner_collateral.to_account_info(),
                    mint: ctx.accounts.collateral_mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.collateral_mint.decimals,
        )?;
        emit!(Increased {
            position: position.key(),
            amount,
            base_added: added as u64,
            price,
        });
        Ok(())
    }

    /// Closes `num / den` of the position's base and pays that share of its equity to
    /// `to`. A position with no equity left is wiped and pays nothing.
    pub fn decrease(ctx: Context<Decrease>, num: u64, den: u64) -> Result<()> {
        require!(den > 0 && num > 0 && num <= den, PerpError::BadShare);
        let price = ctx.accounts.market.price;
        let position = &mut ctx.accounts.position;
        require!(position.base > 0, PerpError::Nothing);

        let equity = equity(position, price);
        let base_closed = (position.base as u128 * num as u128 / den as u128) as u64;
        require!(base_closed > 0, PerpError::Nothing);
        let payout = (equity as u128 * base_closed as u128 / position.base as u128) as u64;
        let collateral_closed =
            (position.collateral as u128 * base_closed as u128 / position.base as u128) as u64;

        if equity == 0 {
            // Liquidated: everything is gone.
            position.base = 0;
            position.collateral = 0;
        } else {
            position.base -= base_closed;
            position.collateral -= collateral_closed;
            if position.base == 0 {
                position.collateral = 0;
            }
        }

        if payout > 0 {
            let market = &ctx.accounts.market;
            let seeds: &[&[u8]] = &[b"market", &market.symbol, &[market.bump]];
            token_interface::transfer_checked(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.key(),
                    TransferChecked {
                        from: ctx.accounts.vault.to_account_info(),
                        mint: ctx.accounts.collateral_mint.to_account_info(),
                        to: ctx.accounts.to.to_account_info(),
                        authority: market.to_account_info(),
                    },
                    &[seeds],
                ),
                payout,
                ctx.accounts.collateral_mint.decimals,
            )?;
        }
        emit!(Decreased {
            position: position.key(),
            base_closed,
            payout,
            price,
            liquidated: equity == 0,
        });
        Ok(())
    }
}

/// A position's equity at `price`: collateral plus profit or loss, floored at zero.
pub fn equity(position: &Position, price: u64) -> u64 {
    let base = position.base as i128;
    let delta = price as i128 - position.entry_price as i128;
    let signed = if position.direction == LONG {
        delta
    } else {
        -delta
    };
    let pnl = base * signed / BASE_UNIT as i128;
    let equity = position.collateral as i128 + pnl;
    equity.clamp(0, u64::MAX as i128) as u64
}

#[account]
#[derive(InitSpace)]
pub struct Market {
    pub authority: Pubkey,
    pub symbol: [u8; 16],
    pub collateral_mint: Pubkey,
    pub vault: Pubkey,
    /// Collateral base units per whole base asset.
    pub price: u64,
    pub price_slot: u64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Position {
    pub market: Pubkey,
    pub owner: Pubkey,
    pub direction: u8,
    pub leverage: u8,
    /// Collateral deposited and not yet closed out, in collateral base units.
    pub collateral: u64,
    /// Base held, in millionths of the base asset.
    pub base: u64,
    /// Average entry price.
    pub entry_price: u64,
    pub bump: u8,
}

#[event]
pub struct Increased {
    pub position: Pubkey,
    pub amount: u64,
    pub base_added: u64,
    pub price: u64,
}

#[event]
pub struct Decreased {
    pub position: Pubkey,
    pub base_closed: u64,
    pub payout: u64,
    pub price: u64,
    pub liquidated: bool,
}

#[derive(Accounts)]
#[instruction(symbol: [u8; 16])]
pub struct CreateMarket<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        init,
        payer = authority,
        space = 8 + Market::INIT_SPACE,
        seeds = [b"market".as_ref(), symbol.as_ref()],
        bump,
    )]
    pub market: Account<'info, Market>,
    pub collateral_mint: InterfaceAccount<'info, Mint>,
    #[account(
        init,
        payer = authority,
        seeds = [b"vault", market.key().as_ref()],
        bump,
        token::mint = collateral_mint,
        token::authority = market,
        token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetPrice<'info> {
    pub authority: Signer<'info>,
    #[account(mut, has_one = authority)]
    pub market: Account<'info, Market>,
}

#[derive(Accounts)]
pub struct OpenPosition<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub owner: Signer<'info>,
    pub market: Account<'info, Market>,
    #[account(
        init,
        payer = payer,
        space = 8 + Position::INIT_SPACE,
        seeds = [b"position", market.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub position: Account<'info, Position>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Increase<'info> {
    pub owner: Signer<'info>,
    #[account(has_one = vault, has_one = collateral_mint)]
    pub market: Account<'info, Market>,
    #[account(mut, has_one = market, has_one = owner)]
    pub position: Account<'info, Position>,
    #[account(mut, token::mint = collateral_mint)]
    pub owner_collateral: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub collateral_mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct Decrease<'info> {
    pub owner: Signer<'info>,
    #[account(has_one = vault, has_one = collateral_mint)]
    pub market: Account<'info, Market>,
    #[account(mut, has_one = market, has_one = owner)]
    pub position: Account<'info, Position>,
    #[account(mut)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    /// Where the closed share's equity is paid.
    #[account(mut, token::mint = collateral_mint)]
    pub to: InterfaceAccount<'info, TokenAccount>,
    pub collateral_mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[error_code]
pub enum PerpError {
    #[msg("price must be positive")]
    BadPrice,
    #[msg("direction is 0 (long) or 1 (short)")]
    BadDirection,
    #[msg("leverage is between 1 and 20")]
    BadLeverage,
    #[msg("num / den must be a share between 0 and 1")]
    BadShare,
    #[msg("nothing to add or close")]
    Nothing,
    #[msg("arithmetic overflow")]
    Overflow,
}
