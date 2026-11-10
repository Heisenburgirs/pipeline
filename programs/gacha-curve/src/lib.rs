//! Gacha bonding-curve strategy for Pipeline.
//!
//! A market is a new token sold along a constant-product curve with virtual reserves
//! (the pump.fun shape) against a quote token such as wrapped SOL. The strategy mints
//! the whole supply into its own vault at market creation and then gives up the mint
//! authority, so the supply is fixed. Buys move quote into the curve and tokens out;
//! sells do the reverse. Every trade reprices the curve. Only `curve_supply` tokens
//! sell on the curve: the buy that takes the last of them **graduates** the market, and
//! the quote raised plus the `supply − curve_supply` tokens held back become a plain
//! constant-product pool with real reserves and a `pool_fee_bps` fee kept in the pool,
//! owned by the market for good, exactly as in `programs/launch-curve` (whose docs
//! derive the parameters). The pot keeps buying from whichever phase is live.
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
//! Every buy that names one of the trader's **packs** (the pack index in the trade's
//! `extra`) also drops a **draw** into it, for free: the draw records the purchase size,
//! the pot's size at that moment and a fresh randomness **seed**. A pack holds
//! [`PACK_DRAWS`] draws; a full pack means opening another. Draws accumulate and are
//! opened whenever the trader likes: they (or anyone) ask the ORAO VRF for the draw's
//! seed, and once the oracles have fulfilled it anyone may open the draw. A
//! 1-in-`win_one_in` win pays `bonus_bps` of the purchase from the pot, capped at
//! `max_win_pot_bps` of the pot; a rarer 1-in-`jackpot_one_in` **jackpot** pays
//! `jackpot_bps` of the purchase, a multiple of it, capped at `max_jackpot_pot_bps`. A
//! draw opened within `fresh_window_slots` of its buy is capped by the pot as it is; a
//! draw left longer is also capped by the pot as it was at the buy, so sitting on a
//! draw never makes it worth more.
//!
//! Why a separate reveal: a roll decided inside the buy transaction can be filtered for
//! free. A trader sets `min_amount_out` to the jackpot and lets losing rolls revert,
//! paying the fee only when they win. Deciding with randomness that does not exist yet
//! at buy time closes that. A draw's seed is fixed by the pack, a nonce it got when it
//! was opened and the draw's index, and the buy refuses to add the draw when that seed
//! has already been requested, so nobody can learn an outcome before committing.
//!
//! Quotes return the deterministic base amount. The bonus never passes through the
//! router: it is paid by `open_draw`, a custom action anyone can call.

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
/// Draws a pack holds.
pub const PACK_DRAWS: usize = 16;
/// Mint index of the quote token (what buyers pay with) and of the launched token.
pub const QUOTE_INDEX: u8 = 0;
pub const TOKEN_INDEX: u8 = 1;
/// The ORAO VRF program, whose fulfilled randomness decides draws.
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
        curve.token_reserve = params.curve_supply;
        curve.virtual_quote = params.virtual_quote;
        curve.virtual_token = params.virtual_token;
        curve.win_one_in = params.win_one_in;
        curve.bonus_bps = params.bonus_bps;
        curve.max_win_pot_bps = params.max_win_pot_bps;
        curve.fresh_window_slots = params.fresh_window_slots;
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
        curve.pack_nonce = 0;
        curve.curve_supply = params.curve_supply;
        curve.pool_fee_bps = params.pool_fee_bps;
        curve.graduated = false;
        curve.graduation_slot = 0;
        curve.supply = params.supply;
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

    /// Strategy accounts for a trade: the output vault and the pot vault, and for a buy
    /// into a pack also that pack and, in a second round once the pack's data is in
    /// hand, the ORAO randomness account for its next draw's seed.
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
        if let (TradeKind::Buy { pack: Some(index) }, Some(trader)) = (kind, request.trade.trader) {
            let trader = Pubkey::new_from_array(trader);
            let pack = pack_address(&curve.key(), &trader, index);
            accounts.push(psi::AccountRef {
                address: pack.to_bytes(),
                writable: true,
            });
            if request.round == 0 {
                complete = false;
            } else {
                // The accounts of round 0 follow the fixed ones; the pack is third. A
                // pack not opened yet (opened in the swap's transaction) will get the
                // curve's next nonce and start with draw 0.
                let (nonce, draws) = ctx
                    .remaining_accounts
                    .get(2)
                    .and_then(pack_nonce_and_draws)
                    .unwrap_or((curve.pack_nonce.saturating_add(1), 0));
                let seed = draw_seed(&pack, nonce, draws);
                accounts.push(psi::AccountRef {
                    address: randomness_address(&seed).to_bytes(),
                    writable: false,
                });
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

    /// Remaining accounts: the output vault, the pot vault, for a buy into a pack that
    /// pack and the ORAO randomness account for its next draw's seed (which must not
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
        // What the pot held before this trade: the ceiling of this draw's prizes.
        let pot_before = token_balance(pot_vault_info)?;
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

        if let TradeKind::Buy { pack: Some(index) } = kind {
            add_draw(
                &ctx.remaining_accounts[2],
                &ctx.remaining_accounts[3],
                &ctx.accounts.curve.key(),
                &ctx.accounts.trader.key(),
                index,
                priced.quote.amount_out,
                pot_before,
            )?;
        }

        // Record the reserves the trade produced, never vault balances, so tokens sent
        // straight to a vault stay out of the price.
        let curve = &mut ctx.accounts.curve;
        let graduated_now = priced.next.graduated && !curve.graduated;
        curve.quote_reserve = priced.next.quote_reserve;
        curve.token_reserve = priced.next.token_reserve;
        curve.creator_owed_quote = priced.next.creator_owed_quote;
        curve.creator_owed_token = priced.next.creator_owed_token;
        curve.protocol_owed_quote = priced.next.protocol_owed_quote;
        curve.protocol_owed_token = priced.next.protocol_owed_token;
        curve.graduated = priced.next.graduated;
        if graduated_now {
            curve.graduation_slot = Clock::get()?.slot;
            emit!(Graduated {
                curve: curve.key(),
                token_mint: curve.token_mint,
                quote_raised: priced.raised,
                pool_quote: curve.quote_reserve,
                pool_token: curve.token_reserve,
                slot: curve.graduation_slot,
            });
        }
        Ok(())
    }

    /// Custom action: opens a pack for the trader on this curve, `index` being the
    /// trader's next pack number. A buy that names the pack drops a draw into it. Open
    /// it in the same transaction as the first such buy, before the swap.
    pub fn open_pack(ctx: Context<OpenPack>, index: u16) -> Result<()> {
        let curve = &mut ctx.accounts.curve;
        curve.pack_nonce = curve
            .pack_nonce
            .checked_add(1)
            .ok_or(GachaError::Overflow)?;
        let nonce = curve.pack_nonce;
        let pack = &mut ctx.accounts.pack;
        pack.curve = ctx.accounts.curve.key();
        pack.trader = ctx.accounts.trader.key();
        pack.index = index;
        pack.nonce = nonce;
        pack.draws = 0;
        pack.opened = 0;
        pack.slots = [Draw::default(); PACK_DRAWS];
        pack.bump = ctx.bumps.pack;
        Ok(())
    }

    /// Custom action: closes a pack whose every draw has been opened (or that has
    /// none) and returns its rent to the trader.
    pub fn close_pack(ctx: Context<ClosePack>) -> Result<()> {
        let pack = &ctx.accounts.pack;
        require!(pack.opened == pack.draws, GachaError::PackHasUnopenedDraws);
        Ok(())
    }

    /// Custom action, callable by anyone: opens draw `draw` of a pack from the ORAO
    /// randomness fulfilled for its seed and pays a win from the pot. A fresh draw's prize
    /// is capped by the pot now, a stale one's also by the pot at its buy.
    pub fn open_draw(ctx: Context<OpenDraw>, draw: u8) -> Result<()> {
        let pack = &ctx.accounts.pack;
        let curve = &ctx.accounts.curve;
        let entry = *pack
            .slots
            .get(draw as usize)
            .ok_or(GachaError::NoSuchDraw)?;
        require!(entry.state == DRAW_ARMED, GachaError::DrawNotArmed);
        let randomness_info = ctx.accounts.randomness.to_account_info();
        require_keys_eq!(
            randomness_info.key(),
            randomness_address(&entry.seed),
            GachaError::WrongAccount
        );
        let randomness =
            fulfilled_randomness(&randomness_info, &entry.seed)?.ok_or(GachaError::NotFulfilled)?;

        let tier = roll(&randomness, curve);
        let mut outcome = Outcome {
            won: tier != Tier::Lose,
            jackpot: tier == Tier::Jackpot,
            payout: 0,
            resolved_slot: Clock::get()?.slot,
        };
        if outcome.won {
            // A fresh draw is capped by the pot now; a stale one also by the pot at
            // its buy, so waiting cannot raise a prize.
            let fresh =
                outcome.resolved_slot <= entry.slot.saturating_add(curve.fresh_window_slots);
            let pot_now = ctx.accounts.pot_vault.amount;
            let pot_cap = if fresh {
                pot_now
            } else {
                pot_now.min(entry.pot_at_buy)
            };
            outcome.payout = payout(entry.stake, pot_cap, curve, tier);
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

        let pack = &mut ctx.accounts.pack;
        pack.slots[draw as usize].state = DRAW_OPENED;
        pack.opened += 1;
        emit!(DrawResolved {
            pack: pack.key(),
            draw,
            curve: curve.key(),
            trader: pack.trader,
            stake: entry.stake,
            won: outcome.won,
            jackpot: outcome.jackpot,
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
    /// Tokens sold on the curve. The rest, `supply − curve_supply`, is held back and
    /// becomes the pool's token side at graduation.
    pub curve_supply: u64,
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
    /// A draw opened within this many slots of its buy is capped by the pot as it is;
    /// opened later, also by the pot as it was at the buy. Stops a known winner from
    /// waiting for a bigger pot without punishing early draws.
    pub fresh_window_slots: u64,
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
    /// Fee on pool trades after graduation, in basis points of the input, kept in the
    /// pool.
    pub pool_fee_bps: u16,
}

impl InitParams {
    fn validate(&self) -> Result<()> {
        require!(self.supply > 0, GachaError::BadInitData);
        require!(
            self.curve_supply > 0 && self.curve_supply < self.supply,
            GachaError::BadInitData
        );
        require!(self.virtual_quote > 0, GachaError::BadInitData);
        require!(self.virtual_token > 0, GachaError::BadInitData);
        require!((self.pool_fee_bps as u128) < BPS, GachaError::BadInitData);
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
        require!(self.fresh_window_slots >= 1, GachaError::BadInitData);
        require!(
            self.max_win_pot_bps > 0 && (self.max_win_pot_bps as u128) <= BPS,
            GachaError::BadInitData
        );
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
    /// Tokens for sale in the current phase: what is left of `curve_supply` on the
    /// curve, then the pool's token reserve. The token vault also holds the held-back
    /// `supply − curve_supply` until graduation.
    pub token_reserve: u64,
    pub virtual_quote: u64,
    pub virtual_token: u64,
    pub win_one_in: u32,
    pub bonus_bps: u16,
    pub max_win_pot_bps: u16,
    pub fresh_window_slots: u64,
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
    /// Packs opened so far; each gets the next value as its nonce, so that seeds never
    /// repeat even when a pack is closed and another opened.
    pub pack_nonce: u64,
    /// Tokens sold on the curve; the rest is held back for the pool.
    pub curve_supply: u64,
    /// Fee on pool trades after graduation, kept in the pool.
    pub pool_fee_bps: u16,
    /// False while the curve is selling; true for good once it has sold out.
    pub graduated: bool,
    /// Slot of the graduating trade (0 before).
    pub graduation_slot: u64,
    /// Tokens minted.
    pub supply: u64,
    /// The Pipeline market PDA allowed to call `pipeline_swap`.
    pub market_signer: Pubkey,
    pub bump: u8,
}

/// One of a trader's packs on a curve: up to [`PACK_DRAWS`] draws, one per buy.
#[account]
#[derive(InitSpace)]
pub struct Pack {
    pub curve: Pubkey,
    pub trader: Pubkey,
    /// The trader's pack number, part of the address.
    pub index: u16,
    /// Assigned from the curve's counter when the pack was opened; part of every seed.
    pub nonce: u64,
    /// Draws added so far (slots `0..draws` are used).
    pub draws: u8,
    /// Draws opened so far.
    pub opened: u8,
    pub slots: [Draw; PACK_DRAWS],
    pub bump: u8,
}

pub const DRAW_EMPTY: u8 = 0;
pub const DRAW_ARMED: u8 = 1;
pub const DRAW_OPENED: u8 = 2;

/// A draw: what one buy put into a pack.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, InitSpace, PartialEq, Eq)]
pub struct Draw {
    /// [`DRAW_EMPTY`], [`DRAW_ARMED`] or [`DRAW_OPENED`].
    pub state: u8,
    /// The buyer's base output, which prizes are a share or a multiple of.
    pub stake: u64,
    /// The pot's balance when the draw was bought: the ceiling of its prizes.
    pub pot_at_buy: u64,
    /// Slot of the buy.
    pub slot: u64,
    /// The ORAO randomness seed: `draw_seed(pack, nonce, index)`.
    pub seed: [u8; 32],
}

#[event]
pub struct Graduated {
    pub curve: Pubkey,
    pub token_mint: Pubkey,
    /// Quote the curve raised in total, before the graduating buy's pool leg.
    pub quote_raised: u64,
    /// The pool's reserves right after the graduating trade.
    pub pool_quote: u64,
    pub pool_token: u64,
    pub slot: u64,
}

#[event]
pub struct FeesClaimed {
    pub curve: Pubkey,
    pub claimant: Pubkey,
    pub quote: u64,
    pub token: u64,
}

#[event]
pub struct DrawResolved {
    pub pack: Pubkey,
    pub draw: u8,
    pub curve: Pubkey,
    pub trader: Pubkey,
    pub stake: u64,
    pub won: bool,
    /// Set with `won` when the roll hit the jackpot tier.
    pub jackpot: bool,
    pub payout: u64,
    /// The slot the draw was opened in.
    pub resolved_slot: u64,
}

struct Outcome {
    won: bool,
    jackpot: bool,
    payout: u64,
    resolved_slot: u64,
}

/// What a trade is, from its mint indices and `extra`: a buy may name one of the
/// trader's packs (its index, two bytes little-endian) to drop a draw into.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TradeKind {
    Buy { pack: Option<u16> },
    Sell,
}

impl TradeKind {
    fn of(trade: &psi::TradeRequest) -> Result<Self> {
        let pack = match trade.extra.as_slice() {
            [] => None,
            [lo, hi] => Some(u16::from_le_bytes([*lo, *hi])),
            _ => return err!(GachaError::BadExtra),
        };
        match (trade.input_index, trade.output_index) {
            (QUOTE_INDEX, TOKEN_INDEX) => Ok(TradeKind::Buy { pack }),
            (TOKEN_INDEX, QUOTE_INDEX) if pack.is_some() => err!(GachaError::BadExtra),
            (TOKEN_INDEX, QUOTE_INDEX) => Ok(TradeKind::Sell),
            _ => err!(GachaError::WrongAccount),
        }
    }

    fn input_vault(self, curve: &Curve) -> Pubkey {
        match self {
            TradeKind::Buy { .. } => curve.quote_vault,
            TradeKind::Sell => curve.token_vault,
        }
    }

    fn output_vault(self, curve: &Curve) -> Pubkey {
        match self {
            TradeKind::Buy { .. } => curve.token_vault,
            TradeKind::Sell => curve.quote_vault,
        }
    }

    fn reserve_in(self, curve: &Curve) -> u64 {
        match self {
            TradeKind::Buy { .. } => curve.quote_reserve,
            TradeKind::Sell => curve.token_reserve,
        }
    }

    /// Unclaimed fees held in the input vault.
    fn owed_in(self, curve: &Curve) -> u64 {
        match self {
            TradeKind::Buy { .. } => curve.creator_owed_quote + curve.protocol_owed_quote,
            TradeKind::Sell => curve.creator_owed_token + curve.protocol_owed_token,
        }
    }

    /// Strategy accounts that precede the transfer-hook accounts in `pipeline_swap`.
    fn own_account_count(self) -> usize {
        match self {
            TradeKind::Buy { pack: Some(_) } => 4,
            TradeKind::Buy { pack: None } | TradeKind::Sell => 2,
        }
    }
}

/// A priced trade: what the router sees, what the pot receives (tokens the pot fee
/// bought from the token vault on a buy, `pot_out`, or the pot fee in tokens on a sell,
/// `pot_in`), the state the trade leaves behind, and (when it graduates the market)
/// what the curve raised in total.
struct Priced {
    quote: psi::QuoteResult,
    pot_out: u64,
    pot_in: u64,
    next: Curve,
    raised: u64,
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

    /// The held-back tokens that become the pool's token side at graduation.
    pub fn reserved(&self) -> u64 {
        self.supply - self.curve_supply
    }

    /// The single pricing function used by both `pipeline_quote` and `pipeline_swap`.
    fn quote(&self, trade: &psi::TradeRequest) -> Result<Priced> {
        require!(
            trade.mode == TradeMode::ExactIn,
            GachaError::ExactOutUnsupported
        );
        let kind = TradeKind::of(trade)?;
        let mut next = self.clone();
        let fees = self.fees_on(trade.amount)?;
        let net = trade.amount - fees.total();
        let mut raised = 0;
        let (out, pot_out, pot_in) = match kind {
            TradeKind::Buy { .. } => {
                require!(next.token_reserve > 0, GachaError::InsufficientLiquidity);
                // All four fees come off the input; a gacha buy pays nothing extra. The
                // buyer's own purchase first, then the pot's with the pot fee, from
                // whichever phase is live; the curve fee stays in the quote reserve and
                // the creator's and protocol's accrue in the quote vault until claimed.
                next.creator_owed_quote = add(next.creator_owed_quote, fees.creator)?;
                next.protocol_owed_quote = add(next.protocol_owed_quote, fees.protocol)?;
                let out = next.buy(net, &mut raised)?;
                let pot_out = next.buy(fees.pot, &mut raised)?;
                next.quote_reserve = add(next.quote_reserve, fees.curve)?;
                (out, pot_out, 0)
            }
            TradeKind::Sell => {
                require!(next.quote_reserve > 0, GachaError::InsufficientLiquidity);
                // All four fees are taken in tokens: the pot fee goes to the pot, the
                // curve fee stays in the token reserve unpaid, the creator's and
                // protocol's accrue in the token vault, and the rest sells.
                next.creator_owed_token = add(next.creator_owed_token, fees.creator)?;
                next.protocol_owed_token = add(next.protocol_owed_token, fees.protocol)?;
                let out = next.sell(net)?;
                next.token_reserve = add(next.token_reserve, fees.curve)?;
                (out, 0, fees.pot)
            }
        };
        Ok(Priced {
            quote: psi::QuoteResult {
                amount_in: trade.amount,
                amount_out: out,
            },
            pot_out,
            pot_in,
            next,
            raised,
        })
    }

    /// Spends `amount` of quote in the live phase and returns the tokens it buys. A buy
    /// that takes the last curve token pays exactly for it, graduates the market, and
    /// spends the rest on the new pool. `raised` is set to the curve's total raise
    /// when that happens.
    fn buy(&mut self, amount: u64, raised: &mut u64) -> Result<u64> {
        let mut out: u64 = 0;
        let mut remaining = amount;
        if !self.graduated && remaining > 0 {
            let x = self.virtual_quote as u128 + self.quote_reserve as u128;
            let y = self.virtual_token as u128 + self.token_reserve as u128;
            let curve_out = out_given_in(x, y, remaining);
            if curve_out < self.token_reserve as u128 {
                out = curve_out as u64;
                self.quote_reserve = add(self.quote_reserve, remaining)?;
                self.token_reserve -= out;
                remaining = 0;
            } else {
                let left = self.token_reserve;
                let cost = cost_for(x, y, left)?.min(remaining);
                out = left;
                self.quote_reserve = add(self.quote_reserve, cost)?;
                self.token_reserve = self.reserved();
                self.graduated = true;
                *raised = self.quote_reserve;
                remaining -= cost;
            }
        }
        if self.graduated && remaining > 0 {
            require!(self.quote_reserve > 0, GachaError::InsufficientLiquidity);
            let net = after_fee(remaining, self.pool_fee_bps)?;
            let pool_out =
                pool_out_given_in(self.quote_reserve as u128, self.token_reserve as u128, net);
            out = add(out, pool_out)?;
            self.quote_reserve = add(self.quote_reserve, remaining)?;
            self.token_reserve -= pool_out;
        }
        Ok(out)
    }

    /// Sells `amount` of tokens in the live phase and returns the quote it pays.
    fn sell(&mut self, amount: u64) -> Result<u64> {
        let out = if !self.graduated {
            let x = self.virtual_quote as u128 + self.quote_reserve as u128;
            let y = self.virtual_token as u128 + self.token_reserve as u128;
            out_given_in(y, x, amount).min(self.quote_reserve as u128) as u64
        } else {
            let net = after_fee(amount, self.pool_fee_bps)?;
            pool_out_given_in(self.token_reserve as u128, self.quote_reserve as u128, net)
        };
        self.token_reserve = add(self.token_reserve, amount)?;
        self.quote_reserve -= out;
        Ok(out)
    }
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
        .ok_or(GachaError::InsufficientLiquidity)?;
    let (q, r) = mul_div(x, out as u128, left);
    let cost = q + (r > 0) as u128;
    u64::try_from(cost).map_err(|_| GachaError::Overflow.into())
}

/// `amount` less the pool fee on it.
fn after_fee(amount: u64, fee_bps: u16) -> Result<u64> {
    let fee = fees::ceil_bps(amount, fee_bps as u64).ok_or(GachaError::Overflow)?;
    Ok(amount - fee)
}

fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).ok_or(GachaError::Overflow.into())
}

/// `floor(reserve_out × in / (reserve_in + in))` on virtual reserves.
pub fn out_given_in(reserve_in: u128, reserve_out: u128, amount_in: u64) -> u128 {
    let denominator = reserve_in + amount_in as u128;
    if denominator == 0 {
        return 0;
    }
    mul_div(reserve_out, amount_in as u128, denominator).0
}

/// What a draw's roll came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Lose,
    Win,
    Jackpot,
}

/// What a draw pays: the win or jackpot share of the stake, capped at the tier's
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

/// The randomness seed of draw `index` of a pack opened with `nonce`.
pub fn draw_seed(pack: &Pubkey, nonce: u64, index: u8) -> [u8; 32] {
    solana_sha256_hasher::hashv(&[b"gacha-vrf", pack.as_ref(), &nonce.to_le_bytes(), &[index]])
        .to_bytes()
}

/// The ORAO randomness request account for a seed.
pub fn randomness_address(seed: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[ORAO_RANDOMNESS_SEED, seed], &ORAO_VRF_PROGRAM).0
}

/// A pack's nonce and how many draws it holds, or `None` if the account is not a pack
/// of this program (for example not opened yet).
fn pack_nonce_and_draws(info: &AccountInfo) -> Option<(u64, u8)> {
    if *info.owner != crate::ID {
        return None;
    }
    let data = info.try_borrow_data().ok()?;
    Pack::try_deserialize(&mut &data[..])
        .ok()
        .map(|pack| (pack.nonce, pack.draws))
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

/// Drops this buy's draw into the trader's pack. The pack must belong to this curve and
/// trader, carry `index`, have room, and the ORAO randomness account for the draw's
/// seed must not exist yet, so the outcome cannot have been learned before the buy.
#[allow(clippy::too_many_arguments)]
fn add_draw(
    info: &AccountInfo,
    randomness: &AccountInfo,
    curve: &Pubkey,
    trader: &Pubkey,
    index: u16,
    stake: u64,
    pot_at_buy: u64,
) -> Result<()> {
    require_keys_eq!(*info.owner, crate::ID, GachaError::WrongAccount);
    require!(info.is_writable, GachaError::WrongAccount);
    let mut pack = {
        let data = info.try_borrow_data()?;
        Pack::try_deserialize(&mut &data[..])?
    };
    require_keys_eq!(pack.curve, *curve, GachaError::WrongAccount);
    require_keys_eq!(pack.trader, *trader, GachaError::WrongAccount);
    require!(pack.index == index, GachaError::WrongAccount);
    let draw = pack.draws;
    require!((draw as usize) < PACK_DRAWS, GachaError::PackFull);
    let seed = draw_seed(&info.key(), pack.nonce, draw);
    require_keys_eq!(
        randomness.key(),
        randomness_address(&seed),
        GachaError::WrongAccount
    );
    require!(randomness.data_is_empty(), GachaError::SeedAlreadyRequested);
    pack.slots[draw as usize] = Draw {
        state: DRAW_ARMED,
        stake,
        pot_at_buy,
        slot: Clock::get()?.slot,
        seed,
    };
    pack.draws = draw + 1;
    let mut data = info.try_borrow_mut_data()?;
    let mut cursor: &mut [u8] = &mut data;
    pack.try_serialize(&mut cursor)?;
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

pub fn pack_address(curve: &Pubkey, trader: &Pubkey, index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"pack",
            curve.as_ref(),
            trader.as_ref(),
            &index.to_le_bytes(),
        ],
        &crate::ID,
    )
    .0
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
#[instruction(index: u16)]
pub struct OpenPack<'info> {
    #[account(mut)]
    pub trader: Signer<'info>,
    #[account(mut)]
    pub curve: Account<'info, Curve>,
    #[account(
        init,
        payer = trader,
        space = 8 + Pack::INIT_SPACE,
        seeds = [b"pack", curve.key().as_ref(), trader.key().as_ref(), &index.to_le_bytes()],
        bump,
    )]
    pub pack: Box<Account<'info, Pack>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClosePack<'info> {
    #[account(mut)]
    pub trader: Signer<'info>,
    #[account(mut, has_one = trader, close = trader)]
    pub pack: Box<Account<'info, Pack>>,
}

#[derive(Accounts)]
pub struct OpenDraw<'info> {
    #[account(mut, has_one = curve, has_one = trader)]
    pub pack: Box<Account<'info, Pack>>,
    #[account(has_one = pot_vault, has_one = token_mint)]
    pub curve: Box<Account<'info, Curve>>,
    /// CHECK: matched against the pack.
    pub trader: UncheckedAccount<'info>,
    #[account(mut)]
    pub pot_vault: InterfaceAccount<'info, TokenAccount>,
    /// Where a win is paid: any token account of the launched mint owned by the trader.
    #[account(mut, token::mint = token_mint, token::authority = trader)]
    pub trader_token: InterfaceAccount<'info, TokenAccount>,
    pub token_mint: InterfaceAccount<'info, Mint>,
    pub token_program: Program<'info, Token>,
    /// CHECK: the ORAO randomness account for the draw's seed, checked by address and
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
    #[msg("extra must be empty, or a pack index (two bytes, little-endian) on a buy")]
    BadExtra,
    #[msg("The pack has no room for another draw; open another pack")]
    PackFull,
    #[msg("No such draw")]
    NoSuchDraw,
    #[msg("That draw is empty or already opened")]
    DrawNotArmed,
    #[msg("Open every draw before closing the pack")]
    PackHasUnopenedDraws,
    #[msg("The draw's randomness has not been requested from the VRF, or not fulfilled yet")]
    NotFulfilled,
    #[msg("Randomness for this draw's seed was requested before the buy")]
    SeedAlreadyRequested,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("Only the market's creator or the program's upgrade authority may claim fees")]
    NotFeeRecipient,
}
