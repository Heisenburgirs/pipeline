//! Launch-curve strategy for Pipeline: a bonding curve that graduates into a pool.
//!
//! A market launches a new token against a quote token (USDC, wrapped SOL, any SPL or
//! Token-2022 mint). The strategy mints the whole `supply` into its own vault at market
//! creation and gives up the mint authority, so the supply is fixed. The token then
//! trades in two phases, both through the same Pipeline market:
//!
//! 1. **Curve.** `curve_supply` tokens sell along a constant-product curve with virtual
//!    reserves (the pump.fun shape). Buys move quote into the curve and tokens out,
//!    sells do the reverse, and there is no fee beyond Pipeline's.
//! 2. **Pool.** The buy that takes the last curve token graduates the market. The quote
//!    raised on the curve and the `supply − curve_supply` tokens held back are paired as
//!    a plain constant-product pool with real reserves and a `pool_fee_bps` fee that
//!    accrues to the pool. The liquidity is owned by the market and can never be
//!    withdrawn.
//!
//! Choose the parameters so that the held-back tokens are worth the raise at the
//! curve's final price, and the pool opens at the price the curve ended at. The
//! pump.fun ratios do this: the curve ends with `virtual_quote + raise` against exactly
//! `virtual_token`, so the final price is their ratio, and the raise is about 20.7% of
//! the graduation market cap. With USDC as quote and the supply at 1 billion tokens, a
//! $1B graduation raises about $207M.
//!
//! A buy that crosses the graduation point pays exactly for the remaining curve tokens
//! and spends the rest of its input on the new pool in the same trade, so pricing is
//! continuous and a buyer never overpays for the last curve token.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::{invoke_signed, set_return_data};
use anchor_spl::token::Token;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use pipeline_strategy_interface::{self as psi, fees, seeds as pipeline_seeds, TradeMode};

declare_id!("4scUVyL9JbLjvkppBk8PYNGRPMCQfp7ppdBFkthDYPLA");

const BPS: u128 = 10_000;
/// Decimals of every token this strategy launches.
pub const TOKEN_DECIMALS: u8 = 6;
/// Mint index of the quote token (what buyers pay with) and of the launched token.
pub const QUOTE_INDEX: u8 = 0;
pub const TOKEN_INDEX: u8 = 1;

#[program]
pub mod launch_curve {
    use super::*;

    /// Called by the Pipeline router from `create_market`. `data` is a Borsh-encoded
    /// [`InitParams`]. The token mint is a fresh keypair that signs `create_market`;
    /// the router forwards its signature because the account is empty.
    pub fn pipeline_init_market(ctx: Context<InitMarket>, data: Vec<u8>) -> Result<()> {
        let params = InitParams::try_from_slice(&data).map_err(|_| LaunchError::BadInitData)?;
        params.validate()?;

        let launch_key = ctx.accounts.launch.key();
        let expected_signer = pipeline_market_signer(&launch_key);
        require_keys_eq!(
            ctx.accounts.market_signer.key(),
            expected_signer,
            LaunchError::NotPipeline
        );

        let bump = [ctx.bumps.launch];
        let token_mint_key = ctx.accounts.token_mint.key();
        let launch_seeds: &[&[u8]] = &[b"launch", token_mint_key.as_ref(), &bump];
        let launch_info = ctx.accounts.launch.to_account_info();
        let token_program = ctx.accounts.token_program.to_account_info();
        let mint_info = ctx.accounts.token_mint.to_account_info();

        // Mint the whole supply into the vault, then freeze the supply for good.
        mint_to(
            &token_program,
            &mint_info,
            &ctx.accounts.token_vault.to_account_info(),
            &launch_info,
            params.supply,
            &[launch_seeds],
        )?;
        revoke_mint_authority(&token_program, &mint_info, &launch_info, &[launch_seeds])?;

        let launch = &mut ctx.accounts.launch;
        launch.quote_mint = ctx.accounts.quote_mint.key();
        launch.token_mint = token_mint_key;
        launch.quote_vault = ctx.accounts.quote_vault.key();
        launch.token_vault = ctx.accounts.token_vault.key();
        launch.supply = params.supply;
        launch.curve_supply = params.curve_supply;
        launch.virtual_quote = params.virtual_quote;
        launch.virtual_token = params.virtual_token;
        launch.pool_fee_bps = params.pool_fee_bps;
        launch.graduated = false;
        launch.quote_reserve = 0;
        launch.token_reserve = params.curve_supply;
        launch.graduation_slot = 0;
        launch.market_signer = expected_signer;
        launch.bump = ctx.bumps.launch;
        Ok(())
    }

    pub fn pipeline_market_info(ctx: Context<MarketInfoAccounts>) -> Result<()> {
        let launch = &ctx.accounts.launch;
        let info = psi::MarketInfo {
            mints: vec![launch.quote_mint.to_bytes(), launch.token_mint.to_bytes()],
            vaults: vec![launch.quote_vault.to_bytes(), launch.token_vault.to_bytes()],
            supports_exact_out: false,
        };
        set_return_data(&borsh_encode(&info)?);
        Ok(())
    }

    /// Strategy accounts for a trade: the output vault, in either phase.
    pub fn pipeline_resolve_accounts(
        ctx: Context<ReadTrade>,
        request: psi::ResolveRequest,
    ) -> Result<()> {
        let kind = TradeKind::of(&request.trade)?;
        let result = psi::ResolveResult {
            accounts: vec![psi::AccountRef {
                address: kind.output_vault(&ctx.accounts.launch).to_bytes(),
                writable: true,
            }],
            complete: true,
        };
        set_return_data(&borsh_encode(&result)?);
        Ok(())
    }

    pub fn pipeline_quote(ctx: Context<ReadTrade>, request: psi::TradeRequest) -> Result<()> {
        let applied = ctx.accounts.launch.apply(&request)?;
        set_return_data(&borsh_encode(&applied.quote)?);
        Ok(())
    }

    /// Remaining accounts: the output vault, then any transfer-hook accounts of the
    /// quote mint.
    pub fn pipeline_swap<'info>(
        ctx: Context<'info, SwapAccounts<'info>>,
        request: psi::SwapRequest,
    ) -> Result<()> {
        let launch = &ctx.accounts.launch;
        require_keys_eq!(
            ctx.accounts.market_signer.key(),
            launch.market_signer,
            LaunchError::NotPipeline
        );
        let trade = &request.trade;
        let kind = TradeKind::of(trade)?;
        require_keys_eq!(
            ctx.accounts.input_vault.key(),
            kind.input_vault(launch),
            LaunchError::WrongAccount
        );
        require!(
            !ctx.remaining_accounts.is_empty(),
            LaunchError::WrongAccount
        );
        let output_vault_info = &ctx.remaining_accounts[0];
        require_keys_eq!(
            output_vault_info.key(),
            kind.output_vault(launch),
            LaunchError::WrongAccount
        );
        let hook_accounts = &ctx.remaining_accounts[1..];

        let reserve_in = kind.reserve_in(launch);
        let vault_in_balance = token_balance(&ctx.accounts.input_vault.to_account_info())?;
        let received = psi::credited_input(vault_in_balance, reserve_in, request.amount_sent);
        let applied = launch.apply(&psi::TradeRequest {
            amount: received,
            ..trade.clone()
        })?;

        if applied.quote.amount_out > 0 {
            let token_mint_key = launch.token_mint;
            let bump = [launch.bump];
            let launch_seeds: &[&[u8]] = &[b"launch", token_mint_key.as_ref(), &bump];
            transfer_checked(
                &ctx.accounts.output_token_program.to_account_info(),
                output_vault_info,
                &ctx.accounts.output_mint.to_account_info(),
                &ctx.accounts.trader_output.to_account_info(),
                &ctx.accounts.launch.to_account_info(),
                hook_accounts,
                applied.quote.amount_out,
                ctx.accounts.output_mint.decimals,
                &[launch_seeds],
            )?;
        }

        // Record the reserves the trade produced, never vault balances, so tokens sent
        // straight to a vault stay out of the price.
        let launch = &mut ctx.accounts.launch;
        let graduated_now = applied.next.graduated && !launch.graduated;
        launch.graduated = applied.next.graduated;
        launch.quote_reserve = applied.next.quote_reserve;
        launch.token_reserve = applied.next.token_reserve;
        if graduated_now {
            launch.graduation_slot = Clock::get()?.slot;
            emit!(Graduated {
                launch: launch.key(),
                token_mint: launch.token_mint,
                quote_raised: applied.raised,
                pool_quote: launch.quote_reserve,
                pool_token: launch.token_reserve,
                slot: launch.graduation_slot,
            });
        }
        Ok(())
    }
}

/// Setup data of `pipeline_init_market`, Borsh-encoded.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct InitParams {
    /// Tokens minted, in base units (6 decimals). Fixed for good.
    pub supply: u64,
    /// Tokens sold on the curve. The rest, `supply − curve_supply`, is held back and
    /// becomes the pool's token side at graduation.
    pub curve_supply: u64,
    /// Virtual quote reserve: sets the opening price together with `virtual_token`.
    pub virtual_quote: u64,
    /// Virtual token reserve, added to the tokens still for sale when pricing. The
    /// curve ends with exactly this many (virtual) tokens, so the final price is
    /// `(virtual_quote + raise) / virtual_token`.
    pub virtual_token: u64,
    /// Fee on pool trades after graduation, in basis points of the input. Stays in the
    /// pool.
    pub pool_fee_bps: u16,
}

impl InitParams {
    fn validate(&self) -> Result<()> {
        require!(self.supply > 0, LaunchError::BadInitData);
        require!(
            self.curve_supply > 0 && self.curve_supply < self.supply,
            LaunchError::BadInitData
        );
        require!(self.virtual_quote > 0, LaunchError::BadInitData);
        require!(self.virtual_token > 0, LaunchError::BadInitData);
        require!((self.pool_fee_bps as u128) < BPS, LaunchError::BadInitData);
        Ok(())
    }
}

#[account]
#[derive(InitSpace)]
pub struct Launch {
    pub quote_mint: Pubkey,
    pub token_mint: Pubkey,
    pub quote_vault: Pubkey,
    pub token_vault: Pubkey,
    pub supply: u64,
    pub curve_supply: u64,
    pub virtual_quote: u64,
    pub virtual_token: u64,
    pub pool_fee_bps: u16,
    /// False while the curve is selling; true for good once it has sold out.
    pub graduated: bool,
    /// Quote tokens held from trades. After graduation, the pool's quote reserve.
    pub quote_reserve: u64,
    /// Tokens for sale in the current phase: what is left of `curve_supply` on the
    /// curve, then the pool's token reserve. The token vault also holds the held-back
    /// `supply − curve_supply` until graduation.
    pub token_reserve: u64,
    /// Slot of the graduating trade (0 before).
    pub graduation_slot: u64,
    /// The Pipeline market PDA allowed to call `pipeline_swap`.
    pub market_signer: Pubkey,
    pub bump: u8,
}

#[event]
pub struct Graduated {
    pub launch: Pubkey,
    pub token_mint: Pubkey,
    /// Quote the curve raised in total, before the graduating buy's pool leg.
    pub quote_raised: u64,
    /// The pool's reserves right after the graduating trade.
    pub pool_quote: u64,
    pub pool_token: u64,
    pub slot: u64,
}

/// What a trade is, from its mint indices.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TradeKind {
    Buy,
    Sell,
}

impl TradeKind {
    fn of(trade: &psi::TradeRequest) -> Result<Self> {
        match (trade.input_index, trade.output_index) {
            (QUOTE_INDEX, TOKEN_INDEX) => Ok(TradeKind::Buy),
            (TOKEN_INDEX, QUOTE_INDEX) => Ok(TradeKind::Sell),
            _ => err!(LaunchError::WrongAccount),
        }
    }

    fn input_vault(self, launch: &Launch) -> Pubkey {
        match self {
            TradeKind::Buy => launch.quote_vault,
            TradeKind::Sell => launch.token_vault,
        }
    }

    fn output_vault(self, launch: &Launch) -> Pubkey {
        match self {
            TradeKind::Buy => launch.token_vault,
            TradeKind::Sell => launch.quote_vault,
        }
    }

    fn reserve_in(self, launch: &Launch) -> u64 {
        match self {
            TradeKind::Buy => launch.quote_reserve,
            TradeKind::Sell => launch.token_reserve,
        }
    }
}

/// A priced trade: what the router sees, the state it leaves behind, and (when the
/// trade graduates the market) what the curve raised in total.
struct Applied {
    quote: psi::QuoteResult,
    next: Launch,
    raised: u64,
}

impl Launch {
    /// The held-back tokens that become the pool's token side at graduation.
    pub fn reserved(&self) -> u64 {
        self.supply - self.curve_supply
    }

    /// The single pricing function used by both `pipeline_quote` and `pipeline_swap`.
    fn apply(&self, trade: &psi::TradeRequest) -> Result<Applied> {
        require!(
            trade.mode == TradeMode::ExactIn,
            LaunchError::ExactOutUnsupported
        );
        let kind = TradeKind::of(trade)?;
        let mut next = self.clone();
        let mut out: u64 = 0;
        let mut raised = 0;
        match kind {
            TradeKind::Buy => {
                let mut remaining_in = trade.amount;
                if !next.graduated {
                    let x = next.virtual_quote as u128 + next.quote_reserve as u128;
                    let y = next.virtual_token as u128 + next.token_reserve as u128;
                    let curve_out = out_given_in(x, y, remaining_in);
                    if curve_out < next.token_reserve as u128 {
                        out = curve_out as u64;
                        next.quote_reserve = add(next.quote_reserve, remaining_in)?;
                        next.token_reserve -= out;
                        remaining_in = 0;
                    } else {
                        // Sell out the curve at the exact cost of what is left, then
                        // graduate; the rest of the input trades on the new pool.
                        let left = next.token_reserve;
                        let cost = cost_for(x, y, left)?.min(remaining_in);
                        out = left;
                        next.quote_reserve = add(next.quote_reserve, cost)?;
                        next.token_reserve = next.reserved();
                        next.graduated = true;
                        raised = next.quote_reserve;
                        remaining_in -= cost;
                    }
                }
                if next.graduated && remaining_in > 0 {
                    require!(next.quote_reserve > 0, LaunchError::InsufficientLiquidity);
                    let net = after_fee(remaining_in, next.pool_fee_bps)?;
                    let pool_out = pool_out_given_in(
                        next.quote_reserve as u128,
                        next.token_reserve as u128,
                        net,
                    );
                    out = add(out, pool_out)?;
                    next.quote_reserve = add(next.quote_reserve, remaining_in)?;
                    next.token_reserve -= pool_out;
                }
            }
            TradeKind::Sell => {
                require!(next.quote_reserve > 0, LaunchError::InsufficientLiquidity);
                if !next.graduated {
                    let x = next.virtual_quote as u128 + next.quote_reserve as u128;
                    let y = next.virtual_token as u128 + next.token_reserve as u128;
                    out = out_given_in(y, x, trade.amount).min(next.quote_reserve as u128) as u64;
                } else {
                    let net = after_fee(trade.amount, next.pool_fee_bps)?;
                    out = pool_out_given_in(
                        next.token_reserve as u128,
                        next.quote_reserve as u128,
                        net,
                    );
                }
                next.token_reserve = add(next.token_reserve, trade.amount)?;
                next.quote_reserve -= out;
            }
        }
        Ok(Applied {
            quote: psi::QuoteResult {
                amount_in: trade.amount,
                amount_out: out,
            },
            next,
            raised,
        })
    }
}

/// `floor(reserve_out × in / (reserve_in + in))`.
pub fn out_given_in(reserve_in: u128, reserve_out: u128, amount_in: u64) -> u128 {
    let denominator = reserve_in + amount_in as u128;
    if denominator == 0 {
        return 0;
    }
    mul_div(reserve_out, amount_in as u128, denominator).0
}

/// A pool trade on real reserves: [`out_given_in`], never the whole reserve.
fn pool_out_given_in(reserve_in: u128, reserve_out: u128, amount_in: u64) -> u64 {
    out_given_in(reserve_in, reserve_out, amount_in).min(reserve_out.saturating_sub(1)) as u64
}

/// The exact quote needed to take `out` from a curve at `(x, y)`:
/// `ceil(x × out / (y − out))`. Fails if it does not fit in a u64.
pub fn cost_for(x: u128, y: u128, out: u64) -> Result<u64> {
    let left = y
        .checked_sub(out as u128)
        .filter(|left| *left > 0)
        .ok_or(LaunchError::InsufficientLiquidity)?;
    let (q, r) = mul_div(x, out as u128, left);
    let cost = q + (r > 0) as u128;
    u64::try_from(cost).map_err(|_| LaunchError::Overflow.into())
}

/// `amount` less the pool fee on it.
fn after_fee(amount: u64, fee_bps: u16) -> Result<u64> {
    let fee = fees::ceil_bps(amount, fee_bps as u64).ok_or(LaunchError::Overflow)?;
    Ok(amount - fee)
}

fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).ok_or(LaunchError::Overflow.into())
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

/// The Pipeline market PDA for a launch of this program.
fn pipeline_market_signer(launch: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[pipeline_seeds::MARKET, crate::ID.as_ref(), launch.as_ref()],
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
    require!(data.len() >= 72, LaunchError::WrongAccount);
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
        space = 8 + Launch::INIT_SPACE,
        seeds = [b"launch", token_mint.key().as_ref()],
        bump,
    )]
    pub launch: Account<'info, Launch>,
    /// The router's setup payer, funded by the market creator.
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub quote_mint: InterfaceAccount<'info, Mint>,
    #[account(
        init,
        payer = payer,
        mint::decimals = TOKEN_DECIMALS,
        mint::authority = launch,
        mint::token_program = token_program,
    )]
    pub token_mint: InterfaceAccount<'info, Mint>,
    #[account(
        init,
        payer = payer,
        seeds = [b"quote_vault", launch.key().as_ref()],
        bump,
        token::mint = quote_mint,
        token::authority = launch,
        token::token_program = quote_token_program,
    )]
    pub quote_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init,
        payer = payer,
        seeds = [b"token_vault", launch.key().as_ref()],
        bump,
        token::mint = token_mint,
        token::authority = launch,
        token::token_program = token_program,
    )]
    pub token_vault: InterfaceAccount<'info, TokenAccount>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct MarketInfoAccounts<'info> {
    pub launch: Account<'info, Launch>,
}

/// The fixed trade accounts, read-only, for quotes and account resolution. Only the
/// launch is read; the others may be placeholders.
#[derive(Accounts)]
pub struct ReadTrade<'info> {
    /// CHECK: unused in reads.
    pub market_signer: UncheckedAccount<'info>,
    pub launch: Account<'info, Launch>,
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
    pub launch: Account<'info, Launch>,
    /// CHECK: identity only; never a signer here.
    pub trader: UncheckedAccount<'info>,
    /// CHECK: unused; this strategy never refunds.
    #[account(mut)]
    pub trader_input: UncheckedAccount<'info>,
    /// CHECK: receives output; the token program checks it.
    #[account(mut)]
    pub trader_output: UncheckedAccount<'info>,
    /// CHECK: compared with the launch's vault for the input mint.
    #[account(mut)]
    pub input_vault: UncheckedAccount<'info>,
    pub input_mint: InterfaceAccount<'info, Mint>,
    pub output_mint: InterfaceAccount<'info, Mint>,
    pub input_token_program: Interface<'info, TokenInterface>,
    pub output_token_program: Interface<'info, TokenInterface>,
}

#[error_code]
pub enum LaunchError {
    #[msg("setup data is not valid InitParams")]
    BadInitData,
    #[msg("caller is not the Pipeline market for this launch")]
    NotPipeline,
    #[msg("an account does not match the launch")]
    WrongAccount,
    #[msg("the launch cannot fill this trade")]
    InsufficientLiquidity,
    #[msg("only exact-in trades are supported")]
    ExactOutUnsupported,
    #[msg("arithmetic overflow")]
    Overflow,
}
