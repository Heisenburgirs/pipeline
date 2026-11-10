//! Perp-backed curve strategy for Pipeline: the launch curve, with a fee that funds a
//! perpetual position every holder owns a share of.
//!
//! A market launches a new token against a quote token on a constant-product curve with
//! virtual reserves that graduates into a locked pool once it sells out, exactly as
//! `programs/launch-curve` does (see its docs for the curve, graduation and the creator
//! and protocol fees). On top, every trade pays `perp_fee_bps` into a **treasury**:
//!
//! - on a buy, in quote, accruing in the quote vault until anyone calls `deploy`, which
//!   pushes it into the market's **position** on `programs/test-perp`: the perp market,
//!   long or short, and leverage chosen at creation;
//! - on a sell, in tokens, which `deploy` **burns**: fewer tokens, the same position, so
//!   every holder's share grows.
//!
//! Holders own the position pro rata. `burn_to_close` burns `T` of a holder's tokens
//! and closes `T / circulating` of the position, paying that share of its equity (its
//! collateral plus profit or loss at the venue's price) in quote straight to the holder.
//! A holder thus has two exits, the curve and the position, and takes whichever pays
//! more; the curve's price is never touched by a burn, since burned tokens were never
//! its reserves.
//!
//! Which perp market, which direction and what leverage are market parameters, so a
//! project can launch a 1x long SOL treasury, a 5x short, or anything the venue lists.
//! The venue here is the repo's minimal one; a real one plugs in behind the same three
//! calls (open, increase, decrease a share) and account layouts.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::{invoke_signed, set_return_data};
use anchor_spl::token::Token;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use pipeline_strategy_interface::{self as psi, fees, seeds as pipeline_seeds, TradeMode};

declare_id!("4LHmX6x7xgB2eNeuRsuF3U3huhFoyfYeddRLzsBwzvPc");

const BPS: u128 = 10_000;
/// Decimals of every token this strategy launches.
pub const TOKEN_DECIMALS: u8 = 6;
/// Mint index of the quote token (what buyers pay with) and of the launched token.
pub const QUOTE_INDEX: u8 = 0;
pub const TOKEN_INDEX: u8 = 1;
/// Fee on every trade, in basis points of the input, for the program's upgrade
/// authority. Compiled in, so a market creator cannot switch it off.
pub const PROTOCOL_FEE_BPS: u16 = 20;
/// Cap on the creator's fee.
pub const MAX_CREATOR_FEE_BPS: u16 = 500;
/// The upgradeable BPF loader, owner of every upgradeable program's ProgramData.
pub const BPF_LOADER_UPGRADEABLE: Pubkey = pubkey!("BPFLoaderUpgradeab1e11111111111111111111111");
/// The perpetuals venue the position lives on (`programs/test-perp`).
pub const PERP_VENUE: Pubkey = pubkey!("8FcD14vCkywa5zPAMmy3g9VpnESJYUZM36c6hzJBdsYi");
/// Cap on the treasury fee.
pub const MAX_PERP_FEE_BPS: u16 = 500;
pub const LONG: u8 = 0;
pub const SHORT: u8 = 1;
pub const MAX_LEVERAGE: u8 = 20;
/// Anchor discriminators of the venue's instructions.
const PERP_OPEN_POSITION: [u8; 8] = [135, 128, 47, 77, 15, 152, 240, 49];
const PERP_INCREASE: [u8; 8] = [254, 95, 127, 165, 140, 103, 125, 124];
const PERP_DECREASE: [u8; 8] = [38, 18, 223, 121, 192, 247, 84, 9];

#[program]
pub mod perp_curve {
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
        launch.creator = params.creator;
        launch.creator_fee_bps = params.creator_fee_bps;
        launch.graduated = false;
        launch.quote_reserve = 0;
        launch.token_reserve = params.curve_supply;
        launch.graduation_slot = 0;
        launch.creator_owed_quote = 0;
        launch.creator_owed_token = 0;
        launch.protocol_owed_quote = 0;
        launch.protocol_owed_token = 0;
        launch.perp_market = params.perp_market;
        launch.position = position_address(&params.perp_market, &launch_key);
        launch.perp_direction = params.perp_direction;
        launch.perp_leverage = params.perp_leverage;
        launch.perp_fee_bps = params.perp_fee_bps;
        launch.min_deploy = params.min_deploy;
        launch.perp_owed_quote = 0;
        launch.perp_owed_token = 0;
        launch.burned = 0;
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

        // Unclaimed fees sit in the vault next to the reserve; neither is the trade's.
        let held_in = add(kind.reserve_in(launch), kind.owed_in(launch))?;
        let vault_in_balance = token_balance(&ctx.accounts.input_vault.to_account_info())?;
        let received = psi::credited_input(vault_in_balance, held_in, request.amount_sent);
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
        launch.creator_owed_quote = applied.next.creator_owed_quote;
        launch.creator_owed_token = applied.next.creator_owed_token;
        launch.protocol_owed_quote = applied.next.protocol_owed_quote;
        launch.protocol_owed_token = applied.next.protocol_owed_token;
        launch.perp_owed_quote = applied.next.perp_owed_quote;
        launch.perp_owed_token = applied.next.perp_owed_token;
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

    /// Custom action: pays the claimant's accrued fees in both mints to the token
    /// accounts given. The market's creator claims the creator fee; the program's
    /// upgrade authority claims the protocol fee. Remaining accounts: transfer-hook
    /// accounts of the quote mint, if any.
    pub fn claim_fees<'info>(ctx: Context<'info, ClaimFees<'info>>) -> Result<()> {
        let launch = &ctx.accounts.launch;
        let claimant = ctx.accounts.claimant.key();
        let is_creator = claimant == launch.creator;
        let is_protocol = {
            let data = ctx.accounts.program_data.try_borrow_data()?;
            upgrade_authority(&data) == Some(Some(claimant))
        };
        require!(is_creator || is_protocol, LaunchError::NotFeeRecipient);

        let mut quote = 0u64;
        let mut token = 0u64;
        if is_creator {
            quote = add(quote, launch.creator_owed_quote)?;
            token = add(token, launch.creator_owed_token)?;
        }
        if is_protocol {
            quote = add(quote, launch.protocol_owed_quote)?;
            token = add(token, launch.protocol_owed_token)?;
        }

        let token_mint_key = launch.token_mint;
        let bump = [launch.bump];
        let launch_seeds: &[&[u8]] = &[b"launch", token_mint_key.as_ref(), &bump];
        let launch_info = ctx.accounts.launch.to_account_info();
        if quote > 0 {
            transfer_checked(
                &ctx.accounts.quote_token_program.to_account_info(),
                &ctx.accounts.quote_vault.to_account_info(),
                &ctx.accounts.quote_mint.to_account_info(),
                &ctx.accounts.claimant_quote.to_account_info(),
                &launch_info,
                ctx.remaining_accounts,
                quote,
                ctx.accounts.quote_mint.decimals,
                &[launch_seeds],
            )?;
        }
        if token > 0 {
            transfer_checked(
                &ctx.accounts.token_program.to_account_info(),
                &ctx.accounts.token_vault.to_account_info(),
                &ctx.accounts.token_mint.to_account_info(),
                &ctx.accounts.claimant_token.to_account_info(),
                &launch_info,
                &[],
                token,
                TOKEN_DECIMALS,
                &[launch_seeds],
            )?;
        }

        let launch = &mut ctx.accounts.launch;
        if is_creator {
            launch.creator_owed_quote = 0;
            launch.creator_owed_token = 0;
        }
        if is_protocol {
            launch.protocol_owed_quote = 0;
            launch.protocol_owed_token = 0;
        }
        emit!(FeesClaimed {
            launch: launch.key(),
            claimant,
            quote,
            token,
        });
        Ok(())
    }

    /// Custom action, callable by anyone: burns the treasury's accrued tokens (the fee
    /// on sells) and pushes its accrued quote (the fee on buys) into the market's
    /// position on the venue, opening the position first if this is the first deploy
    /// (the caller pays its rent). The quote leg waits until it reaches `min_deploy`;
    /// the call is refused only when there is nothing to do at all.
    pub fn deploy(ctx: Context<Deploy>) -> Result<()> {
        let launch = &ctx.accounts.launch;
        let amount = launch.perp_owed_quote;
        let to_burn = launch.perp_owed_token;
        let deploy_quote = amount > 0 && amount >= launch.min_deploy;
        require!(deploy_quote || to_burn > 0, LaunchError::NothingToDeploy);
        let token_mint_key = launch.token_mint;
        let bump = [launch.bump];
        let launch_seeds: &[&[u8]] = &[b"launch", token_mint_key.as_ref(), &bump];
        let launch_info = ctx.accounts.launch.to_account_info();

        if to_burn > 0 {
            burn(
                &ctx.accounts.token_program.to_account_info(),
                &ctx.accounts.token_vault.to_account_info(),
                &ctx.accounts.token_mint.to_account_info(),
                &launch_info,
                to_burn,
                &[launch_seeds],
            )?;
        }
        if !deploy_quote {
            let launch = &mut ctx.accounts.launch;
            launch.perp_owed_token = 0;
            launch.burned = add(launch.burned, to_burn)?;
            emit!(Deployed {
                launch: launch.key(),
                position: launch.position,
                amount: 0,
                burned: to_burn,
            });
            return Ok(());
        }

        if ctx.accounts.position.data_is_empty() {
            perp_open_position(
                &ctx.accounts.perp_program.to_account_info(),
                &ctx.accounts.payer.to_account_info(),
                &launch_info,
                &ctx.accounts.perp_market.to_account_info(),
                &ctx.accounts.position.to_account_info(),
                &ctx.accounts.system_program.to_account_info(),
                launch.perp_direction,
                launch.perp_leverage,
                &[launch_seeds],
            )?;
        }
        perp_increase(
            &ctx.accounts.perp_program.to_account_info(),
            &launch_info,
            &ctx.accounts.perp_market.to_account_info(),
            &ctx.accounts.position.to_account_info(),
            &ctx.accounts.quote_vault.to_account_info(),
            &ctx.accounts.perp_vault.to_account_info(),
            &ctx.accounts.quote_mint.to_account_info(),
            &ctx.accounts.quote_token_program.to_account_info(),
            amount,
            &[launch_seeds],
        )?;

        let launch = &mut ctx.accounts.launch;
        launch.perp_owed_quote = 0;
        launch.perp_owed_token = 0;
        launch.burned = add(launch.burned, to_burn)?;
        emit!(Deployed {
            launch: launch.key(),
            position: launch.position,
            amount,
            burned: to_burn,
        });
        Ok(())
    }

    /// Custom action: burns `amount` of the holder's tokens and closes that share of
    /// the circulating supply's position, paying the share's equity in quote to the
    /// holder. The curve is untouched.
    pub fn burn_to_close(ctx: Context<BurnToClose>, amount: u64) -> Result<()> {
        let launch = &ctx.accounts.launch;
        require!(amount > 0, LaunchError::BadBurn);
        let circulating = launch.circulating();
        require!(amount <= circulating, LaunchError::BadBurn);
        require!(
            !ctx.accounts.position.data_is_empty(),
            LaunchError::NoPosition
        );

        burn(
            &ctx.accounts.token_program.to_account_info(),
            &ctx.accounts.holder_token.to_account_info(),
            &ctx.accounts.token_mint.to_account_info(),
            &ctx.accounts.holder.to_account_info(),
            amount,
            &[],
        )?;

        let token_mint_key = launch.token_mint;
        let bump = [launch.bump];
        let launch_seeds: &[&[u8]] = &[b"launch", token_mint_key.as_ref(), &bump];
        let before = token_balance(&ctx.accounts.holder_quote.to_account_info())?;
        perp_decrease(
            &ctx.accounts.perp_program.to_account_info(),
            &ctx.accounts.launch.to_account_info(),
            &ctx.accounts.perp_market.to_account_info(),
            &ctx.accounts.position.to_account_info(),
            &ctx.accounts.perp_vault.to_account_info(),
            &ctx.accounts.holder_quote.to_account_info(),
            &ctx.accounts.quote_mint.to_account_info(),
            &ctx.accounts.quote_token_program.to_account_info(),
            amount,
            circulating,
            &[launch_seeds],
        )?;
        let payout = token_balance(&ctx.accounts.holder_quote.to_account_info())? - before;

        let launch = &mut ctx.accounts.launch;
        launch.burned = add(launch.burned, amount)?;
        emit!(BurnedForPosition {
            launch: launch.key(),
            holder: ctx.accounts.holder.key(),
            burned: amount,
            circulating,
            payout,
        });
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
    /// Fee on every trade, in basis points of the input, for `creator`. At most
    /// [`MAX_CREATOR_FEE_BPS`]. [`PROTOCOL_FEE_BPS`] is charged on top.
    pub creator_fee_bps: u16,
    /// Who claims the creator fee.
    pub creator: Pubkey,
    /// The venue market the treasury takes its position on (for example SOL-PERP).
    pub perp_market: Pubkey,
    /// [`LONG`] or [`SHORT`].
    pub perp_direction: u8,
    /// 1 to [`MAX_LEVERAGE`].
    pub perp_leverage: u8,
    /// Fee on every trade, in basis points of the input, for the treasury. At most
    /// [`MAX_PERP_FEE_BPS`].
    pub perp_fee_bps: u16,
    /// `deploy` is refused while the treasury holds less quote than this.
    pub min_deploy: u64,
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
        require!(
            self.creator_fee_bps <= MAX_CREATOR_FEE_BPS,
            LaunchError::BadInitData
        );
        require!(
            self.perp_direction == LONG || self.perp_direction == SHORT,
            LaunchError::BadInitData
        );
        require!(
            (1..=MAX_LEVERAGE).contains(&self.perp_leverage),
            LaunchError::BadInitData
        );
        require!(
            self.perp_fee_bps <= MAX_PERP_FEE_BPS,
            LaunchError::BadInitData
        );
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
    /// Claims the creator fee.
    pub creator: Pubkey,
    pub supply: u64,
    pub curve_supply: u64,
    pub virtual_quote: u64,
    pub virtual_token: u64,
    pub pool_fee_bps: u16,
    pub creator_fee_bps: u16,
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
    /// Fees accrued and not yet claimed. They sit in the vaults next to the reserves.
    pub creator_owed_quote: u64,
    pub creator_owed_token: u64,
    pub protocol_owed_quote: u64,
    pub protocol_owed_token: u64,
    /// The venue market and this market's position on it.
    pub perp_market: Pubkey,
    pub position: Pubkey,
    pub perp_direction: u8,
    pub perp_leverage: u8,
    pub perp_fee_bps: u16,
    pub min_deploy: u64,
    /// Treasury fee collected on buys and not yet deployed; sits in the quote vault.
    pub perp_owed_quote: u64,
    /// Treasury fee collected on sells and not yet burned; sits in the token vault.
    pub perp_owed_token: u64,
    /// Tokens burned so far: the sell-side treasury fee and holders' burns.
    pub burned: u64,
    /// The Pipeline market PDA allowed to call `pipeline_swap`.
    pub market_signer: Pubkey,
    pub bump: u8,
}

#[event]
pub struct FeesClaimed {
    pub launch: Pubkey,
    pub claimant: Pubkey,
    pub quote: u64,
    pub token: u64,
}

#[event]
pub struct Deployed {
    pub launch: Pubkey,
    pub position: Pubkey,
    /// Quote pushed into the position.
    pub amount: u64,
    /// Tokens burned.
    pub burned: u64,
}

#[event]
pub struct BurnedForPosition {
    pub launch: Pubkey,
    pub holder: Pubkey,
    pub burned: u64,
    /// The circulating supply the burn was a share of.
    pub circulating: u64,
    pub payout: u64,
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

    /// Unclaimed fees held in the input vault.
    fn owed_in(self, launch: &Launch) -> u64 {
        match self {
            TradeKind::Buy => {
                launch.creator_owed_quote + launch.protocol_owed_quote + launch.perp_owed_quote
            }
            TradeKind::Sell => {
                launch.creator_owed_token + launch.protocol_owed_token + launch.perp_owed_token
            }
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

    /// Tokens in holders' hands: minted, less burned, less what the market itself holds
    /// (the reserve, the held-back pool share before graduation, and unclaimed fees).
    pub fn circulating(&self) -> u64 {
        let held = self.token_reserve
            + if self.graduated { 0 } else { self.reserved() }
            + self.creator_owed_token
            + self.protocol_owed_token
            + self.perp_owed_token;
        self.supply.saturating_sub(self.burned).saturating_sub(held)
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
        // The creator's and the protocol's fees come off the input first, in the input
        // mint, and stay in the input vault until claimed.
        let creator_fee = fees::ceil_bps(trade.amount, self.creator_fee_bps as u64)
            .ok_or(LaunchError::Overflow)?;
        let protocol_fee =
            fees::ceil_bps(trade.amount, PROTOCOL_FEE_BPS as u64).ok_or(LaunchError::Overflow)?;
        let perp_fee =
            fees::ceil_bps(trade.amount, self.perp_fee_bps as u64).ok_or(LaunchError::Overflow)?;
        let net_in = trade
            .amount
            .checked_sub(add(add(creator_fee, protocol_fee)?, perp_fee)?)
            .ok_or(LaunchError::InsufficientLiquidity)?;
        match kind {
            TradeKind::Buy => {
                next.creator_owed_quote = add(next.creator_owed_quote, creator_fee)?;
                next.protocol_owed_quote = add(next.protocol_owed_quote, protocol_fee)?;
                next.perp_owed_quote = add(next.perp_owed_quote, perp_fee)?;
                let mut remaining_in = net_in;
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
                next.creator_owed_token = add(next.creator_owed_token, creator_fee)?;
                next.protocol_owed_token = add(next.protocol_owed_token, protocol_fee)?;
                next.perp_owed_token = add(next.perp_owed_token, perp_fee)?;
                if !next.graduated {
                    let x = next.virtual_quote as u128 + next.quote_reserve as u128;
                    let y = next.virtual_token as u128 + next.token_reserve as u128;
                    out = out_given_in(y, x, net_in).min(next.quote_reserve as u128) as u64;
                } else {
                    let net = after_fee(net_in, next.pool_fee_bps)?;
                    out = pool_out_given_in(
                        next.token_reserve as u128,
                        next.quote_reserve as u128,
                        net,
                    );
                }
                next.token_reserve = add(next.token_reserve, net_in)?;
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

/// The venue's position account for `owner` on `market`.
pub fn position_address(market: &Pubkey, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"position", market.as_ref(), owner.as_ref()], &PERP_VENUE).0
}

/// SPL Token `Burn` (instruction 8) from `account`, signed by `authority`.
fn burn<'info>(
    token_program: &AccountInfo<'info>,
    account: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    amount: u64,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    let mut data = vec![8u8];
    data.extend_from_slice(&amount.to_le_bytes());
    invoke_signed(
        &Instruction {
            program_id: token_program.key(),
            accounts: vec![
                AccountMeta::new(account.key(), false),
                AccountMeta::new(mint.key(), false),
                AccountMeta::new_readonly(authority.key(), true),
            ],
            data,
        },
        &[
            account.clone(),
            mint.clone(),
            authority.clone(),
            token_program.clone(),
        ],
        signer_seeds,
    )?;
    Ok(())
}

/// Venue `open_position(direction, leverage)`: payer, owner, market, position, system.
#[allow(clippy::too_many_arguments)]
fn perp_open_position<'info>(
    program: &AccountInfo<'info>,
    payer: &AccountInfo<'info>,
    owner: &AccountInfo<'info>,
    market: &AccountInfo<'info>,
    position: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    direction: u8,
    leverage: u8,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    let mut data = PERP_OPEN_POSITION.to_vec();
    data.push(direction);
    data.push(leverage);
    invoke_signed(
        &Instruction {
            program_id: program.key(),
            accounts: vec![
                AccountMeta::new(payer.key(), true),
                AccountMeta::new_readonly(owner.key(), true),
                AccountMeta::new_readonly(market.key(), false),
                AccountMeta::new(position.key(), false),
                AccountMeta::new_readonly(system_program.key(), false),
            ],
            data,
        },
        &[
            payer.clone(),
            owner.clone(),
            market.clone(),
            position.clone(),
            system_program.clone(),
            program.clone(),
        ],
        signer_seeds,
    )?;
    Ok(())
}

/// Venue `increase(amount)`: owner, market, position, owner collateral, vault,
/// collateral mint, token program.
#[allow(clippy::too_many_arguments)]
fn perp_increase<'info>(
    program: &AccountInfo<'info>,
    owner: &AccountInfo<'info>,
    market: &AccountInfo<'info>,
    position: &AccountInfo<'info>,
    owner_collateral: &AccountInfo<'info>,
    vault: &AccountInfo<'info>,
    collateral_mint: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    amount: u64,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    let mut data = PERP_INCREASE.to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    invoke_signed(
        &Instruction {
            program_id: program.key(),
            accounts: vec![
                AccountMeta::new_readonly(owner.key(), true),
                AccountMeta::new_readonly(market.key(), false),
                AccountMeta::new(position.key(), false),
                AccountMeta::new(owner_collateral.key(), false),
                AccountMeta::new(vault.key(), false),
                AccountMeta::new_readonly(collateral_mint.key(), false),
                AccountMeta::new_readonly(token_program.key(), false),
            ],
            data,
        },
        &[
            owner.clone(),
            market.clone(),
            position.clone(),
            owner_collateral.clone(),
            vault.clone(),
            collateral_mint.clone(),
            token_program.clone(),
            program.clone(),
        ],
        signer_seeds,
    )?;
    Ok(())
}

/// Venue `decrease(num, den)`: owner, market, position, vault, to, collateral mint,
/// token program.
#[allow(clippy::too_many_arguments)]
fn perp_decrease<'info>(
    program: &AccountInfo<'info>,
    owner: &AccountInfo<'info>,
    market: &AccountInfo<'info>,
    position: &AccountInfo<'info>,
    vault: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    collateral_mint: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    num: u64,
    den: u64,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    let mut data = PERP_DECREASE.to_vec();
    data.extend_from_slice(&num.to_le_bytes());
    data.extend_from_slice(&den.to_le_bytes());
    invoke_signed(
        &Instruction {
            program_id: program.key(),
            accounts: vec![
                AccountMeta::new_readonly(owner.key(), true),
                AccountMeta::new_readonly(market.key(), false),
                AccountMeta::new(position.key(), false),
                AccountMeta::new(vault.key(), false),
                AccountMeta::new(to.key(), false),
                AccountMeta::new_readonly(collateral_mint.key(), false),
                AccountMeta::new_readonly(token_program.key(), false),
            ],
            data,
        },
        &[
            owner.clone(),
            market.clone(),
            position.clone(),
            vault.clone(),
            to.clone(),
            collateral_mint.clone(),
            token_program.clone(),
            program.clone(),
        ],
        signer_seeds,
    )?;
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

#[derive(Accounts)]
pub struct ClaimFees<'info> {
    pub claimant: Signer<'info>,
    #[account(mut, has_one = quote_mint, has_one = token_mint, has_one = quote_vault, has_one = token_vault)]
    pub launch: Account<'info, Launch>,
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

#[derive(Accounts)]
pub struct Deploy<'info> {
    /// Pays the position's rent on the first deploy.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, has_one = quote_mint, has_one = token_mint, has_one = quote_vault, has_one = token_vault, has_one = perp_market, has_one = position)]
    pub launch: Account<'info, Launch>,
    #[account(mut)]
    pub quote_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub token_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub token_mint: InterfaceAccount<'info, Mint>,
    /// CHECK: the venue market, matched against the launch; the venue validates it.
    pub perp_market: UncheckedAccount<'info>,
    /// CHECK: the launch's position on the venue, created by the first deploy.
    #[account(mut)]
    pub position: UncheckedAccount<'info>,
    /// CHECK: the venue's collateral vault; the venue matches it to the market.
    #[account(mut)]
    pub perp_vault: UncheckedAccount<'info>,
    pub quote_mint: InterfaceAccount<'info, Mint>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub token_program: Program<'info, Token>,
    /// CHECK: the venue program, by address.
    #[account(address = PERP_VENUE)]
    pub perp_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct BurnToClose<'info> {
    pub holder: Signer<'info>,
    #[account(mut, has_one = quote_mint, has_one = token_mint, has_one = perp_market, has_one = position)]
    pub launch: Account<'info, Launch>,
    #[account(mut, token::mint = token_mint, token::authority = holder)]
    pub holder_token: InterfaceAccount<'info, TokenAccount>,
    /// Where the closed share's equity is paid: any token account of the quote mint.
    #[account(mut, token::mint = quote_mint, token::token_program = quote_token_program)]
    pub holder_quote: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub token_mint: InterfaceAccount<'info, Mint>,
    pub quote_mint: InterfaceAccount<'info, Mint>,
    /// CHECK: the venue market, matched against the launch; the venue validates it.
    pub perp_market: UncheckedAccount<'info>,
    /// CHECK: the launch's position on the venue.
    #[account(mut)]
    pub position: UncheckedAccount<'info>,
    /// CHECK: the venue's collateral vault; the venue matches it to the market.
    #[account(mut)]
    pub perp_vault: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    /// CHECK: the venue program, by address.
    #[account(address = PERP_VENUE)]
    pub perp_program: UncheckedAccount<'info>,
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
    #[msg("only the market's creator or the program's upgrade authority may claim fees")]
    NotFeeRecipient,
    #[msg("nothing to deploy: no tokens to burn and less quote than min_deploy")]
    NothingToDeploy,
    #[msg("the position has not been opened yet; deploy first")]
    NoPosition,
    #[msg("burn between 1 and the circulating supply")]
    BadBurn,
}
