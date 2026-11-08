//! Constant-product strategy for Pipeline.
//!
//! Used to test the router and as the basis of the strategy template. It implements
//! all five Pipeline entry points, plus a `deposit` custom action to add liquidity.
//!
//! The pool owns one vault per mint (associated token accounts of the pool PDA) and
//! records reserves. A swap credits what arrived in the input vault beyond the recorded
//! reserve, capped at what the router sent in this call (`credited_input`), so tokens
//! with transfer fees work and tokens pre-deposited outside Pipeline count as donations
//! rather than as fee-free input. Swaps and deposits move the reserves by exactly what
//! they took in and paid out, so donations never enter the price. Swaps reuse the quote
//! maths, so quotes and swaps agree.
//!
//! One pool exists per (mint A, mint B, fee) combination. The pool has no owner, so
//! creating it first gives no control; vaults are created idempotently so nobody can
//! block a pool by creating its vault accounts in advance.
//!
//! Pools can also be created and funded in one call by another program through the
//! optional `pipeline_seed_liquidity` entry point (a launchpad graduating a token, for
//! example). Those pools live at an address derived from the seeder, so nobody can
//! create or fund them first at a bad price.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::{invoke_signed, set_return_data};
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use pipeline_strategy_interface::{self as psi, seeds as pipeline_seeds, TradeMode};

declare_id!("yRptCdde1du1JYR2DcNFtLC1fkp5Asd6rE3XSvZoUFx");

const BPS: u128 = 10_000;

#[program]
pub mod test_strategy_cp {
    use super::*;

    /// Called by the Pipeline router from `create_market`. `data` is a Borsh-encoded
    /// [`InitParams`].
    pub fn pipeline_init_market(ctx: Context<InitMarket>, data: Vec<u8>) -> Result<()> {
        let params = InitParams::try_from_slice(&data).map_err(|_| CpError::BadInitData)?;
        require!((params.fee_bps as u128) < BPS, CpError::BadInitData);

        let pool_key = ctx.accounts.pool.key();
        let expected_signer = pipeline_market_signer(&pool_key);
        require_keys_eq!(
            ctx.accounts.market_signer.key(),
            expected_signer,
            CpError::NotPipeline
        );

        let pool = &mut ctx.accounts.pool;
        pool.mint_a = ctx.accounts.mint_a.key();
        pool.mint_b = ctx.accounts.mint_b.key();
        pool.vault_a = ctx.accounts.vault_a.key();
        pool.vault_b = ctx.accounts.vault_b.key();
        pool.reserve_a = 0;
        pool.reserve_b = 0;
        pool.fee_bps = params.fee_bps;
        pool.market_signer = expected_signer;
        pool.seeder = Pubkey::default();
        pool.bump = ctx.bumps.pool;
        Ok(())
    }

    /// Optional Pipeline extension: creates a pool at `["seeded", seeder, mint_a,
    /// mint_b, fee]` and funds it from the seeder's escrows in one call. `request.data`
    /// is a Borsh-encoded [`InitParams`]. Remaining accounts are transfer-hook accounts.
    /// The pool has no LP shares, so seeded liquidity stays in the pool for good.
    pub fn pipeline_seed_liquidity<'info>(
        ctx: Context<'info, SeedLiquidity<'info>>,
        request: psi::SeedRequest,
    ) -> Result<()> {
        let params = InitParams::try_from_slice(&request.data).map_err(|_| CpError::BadInitData)?;
        require!((params.fee_bps as u128) < BPS, CpError::BadInitData);
        let [amount_a, amount_b] = request.amounts[..] else {
            return err!(CpError::BadInitData);
        };
        require!(amount_a > 0 && amount_b > 0, CpError::OneSidedDeposit);

        // Reserves are what arrived from the escrows, so tokens sent to the vaults
        // beforehand can't change the opening price.
        let accounts = &ctx.accounts;
        let mut arrived = [0u64; 2];
        for (i, (escrow, mint, vault, program, amount)) in [
            (
                &accounts.escrow_a,
                &accounts.mint_a,
                &accounts.vault_a,
                &accounts.token_program_a,
                amount_a,
            ),
            (
                &accounts.escrow_b,
                &accounts.mint_b,
                &accounts.vault_b,
                &accounts.token_program_b,
                amount_b,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let before = token_balance(&vault.to_account_info())?;
            transfer_checked(
                &program.to_account_info(),
                &escrow.to_account_info(),
                &mint.to_account_info(),
                &vault.to_account_info(),
                &accounts.seeder.to_account_info(),
                ctx.remaining_accounts,
                amount,
                mint.decimals,
                &[],
            )?;
            arrived[i] = token_balance(&vault.to_account_info())? - before;
        }
        let [balance_a, balance_b] = arrived;
        require!(balance_a > 0 && balance_b > 0, CpError::OneSidedDeposit);

        let pool_key = ctx.accounts.pool.key();
        let pool = &mut ctx.accounts.pool;
        pool.mint_a = ctx.accounts.mint_a.key();
        pool.mint_b = ctx.accounts.mint_b.key();
        pool.vault_a = ctx.accounts.vault_a.key();
        pool.vault_b = ctx.accounts.vault_b.key();
        pool.reserve_a = balance_a;
        pool.reserve_b = balance_b;
        pool.fee_bps = params.fee_bps;
        pool.market_signer = pipeline_market_signer(&pool_key);
        pool.seeder = ctx.accounts.seeder.key();
        pool.bump = ctx.bumps.pool;

        let result = psi::SeedResult {
            taken: vec![amount_a, amount_b],
            lp_amount: 0,
        };
        set_return_data(&borsh_encode(&result)?);
        Ok(())
    }

    pub fn pipeline_market_info(ctx: Context<MarketInfoAccounts>) -> Result<()> {
        let pool = &ctx.accounts.pool;
        let info = psi::MarketInfo {
            mints: vec![pool.mint_a.to_bytes(), pool.mint_b.to_bytes()],
            vaults: vec![pool.vault_a.to_bytes(), pool.vault_b.to_bytes()],
            supports_exact_out: true,
        };
        set_return_data(&borsh_encode(&info)?);
        Ok(())
    }

    /// The only strategy-specific account a swap needs is the output vault.
    pub fn pipeline_resolve_accounts(
        ctx: Context<ReadTrade>,
        request: psi::ResolveRequest,
    ) -> Result<()> {
        let pool = &ctx.accounts.pool;
        let (_, output_vault) = pool.vaults(&request.trade)?;
        let result = psi::ResolveResult {
            accounts: vec![psi::AccountRef {
                address: output_vault.to_bytes(),
                writable: true,
            }],
            complete: true,
        };
        set_return_data(&borsh_encode(&result)?);
        Ok(())
    }

    pub fn pipeline_quote(ctx: Context<ReadTrade>, request: psi::TradeRequest) -> Result<()> {
        let quote = ctx.accounts.pool.quote(&request)?;
        set_return_data(&borsh_encode(&quote)?);
        Ok(())
    }

    /// Remaining accounts: the output vault, then any transfer-hook accounts.
    pub fn pipeline_swap<'info>(
        ctx: Context<'info, SwapAccounts<'info>>,
        request: psi::SwapRequest,
    ) -> Result<()> {
        let pool = &ctx.accounts.pool;
        require_keys_eq!(
            ctx.accounts.market_signer.key(),
            pool.market_signer,
            CpError::NotPipeline
        );
        let trade = &request.trade;
        let (input_vault, output_vault) = pool.vaults(trade)?;
        require_keys_eq!(
            ctx.accounts.input_vault.key(),
            input_vault,
            CpError::WrongAccount
        );
        let output_vault_info = ctx
            .remaining_accounts
            .first()
            .ok_or(CpError::WrongAccount)?;
        require_keys_eq!(output_vault_info.key(), output_vault, CpError::WrongAccount);
        let hook_accounts = &ctx.remaining_accounts[1..];

        let (reserve_in, _) = pool.reserves(trade);
        let vault_in_balance = token_balance(&ctx.accounts.input_vault.to_account_info())?;
        let received = psi::credited_input(vault_in_balance, reserve_in, request.amount_sent);

        let quote = pool.quote(&psi::TradeRequest {
            amount: match trade.mode {
                TradeMode::ExactIn => received,
                TradeMode::ExactOut => trade.amount,
            },
            ..trade.clone()
        })?;
        require!(quote.amount_in <= received, CpError::InsufficientInput);
        let refund = received - quote.amount_in;

        let fee_seed = pool.fee_bps.to_le_bytes();
        let bump = [pool.bump];
        let pool_seeds = pool.signer_seeds(&fee_seed, &bump);
        let pool_seeds: &[&[u8]] = &pool_seeds;
        let pool_info = ctx.accounts.pool.to_account_info();
        if refund > 0 {
            transfer_checked(
                &ctx.accounts.input_token_program.to_account_info(),
                &ctx.accounts.input_vault.to_account_info(),
                &ctx.accounts.input_mint.to_account_info(),
                &ctx.accounts.trader_input.to_account_info(),
                &pool_info,
                hook_accounts,
                refund,
                ctx.accounts.input_mint.decimals,
                &[pool_seeds],
            )?;
        }
        if quote.amount_out > 0 {
            transfer_checked(
                &ctx.accounts.output_token_program.to_account_info(),
                output_vault_info,
                &ctx.accounts.output_mint.to_account_info(),
                &ctx.accounts.trader_output.to_account_info(),
                &pool_info,
                hook_accounts,
                quote.amount_out,
                ctx.accounts.output_mint.decimals,
                &[pool_seeds],
            )?;
        }

        // Update reserves by the trade itself, not from vault balances, so tokens sent
        // straight to a vault stay out of the price.
        let (reserve_in, reserve_out) = ctx.accounts.pool.reserves(trade);
        ctx.accounts.pool.set_reserves(
            trade,
            reserve_in + quote.amount_in,
            reserve_out - quote.amount_out,
        );
        Ok(())
    }

    /// Custom action: add liquidity. Anyone can deposit; there are no LP shares, because
    /// this strategy exists for testing. Remaining accounts are transfer-hook accounts.
    ///
    /// While either side is empty, a deposit must add both tokens, so a pool never
    /// trades with one side empty.
    pub fn deposit<'info>(
        ctx: Context<'info, Deposit<'info>>,
        amount_a: u64,
        amount_b: u64,
    ) -> Result<()> {
        let accounts = &ctx.accounts;
        let pool = &accounts.pool;
        if pool.reserve_a == 0 || pool.reserve_b == 0 {
            require!(amount_a > 0 && amount_b > 0, CpError::OneSidedDeposit);
        }
        // Credit what arrived, so tokens donated to the vaults earlier stay out of the
        // price even when a deposit is tiny or empty.
        let mut arrived = [0u64; 2];
        for (i, (from, mint, vault, program, amount)) in [
            (
                &accounts.depositor_a,
                &accounts.mint_a,
                &accounts.vault_a,
                &accounts.token_program_a,
                amount_a,
            ),
            (
                &accounts.depositor_b,
                &accounts.mint_b,
                &accounts.vault_b,
                &accounts.token_program_b,
                amount_b,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            if amount > 0 {
                let before = token_balance(&vault.to_account_info())?;
                transfer_checked(
                    &program.to_account_info(),
                    &from.to_account_info(),
                    &mint.to_account_info(),
                    &vault.to_account_info(),
                    &accounts.depositor.to_account_info(),
                    ctx.remaining_accounts,
                    amount,
                    mint.decimals,
                    &[],
                )?;
                arrived[i] = token_balance(&vault.to_account_info())? - before;
            }
        }
        let pool = &mut ctx.accounts.pool;
        pool.reserve_a += arrived[0];
        pool.reserve_b += arrived[1];
        Ok(())
    }
}

#[derive(AnchorSerialize, AnchorDeserialize)]
pub struct InitParams {
    pub fee_bps: u16,
}

#[account]
#[derive(InitSpace)]
pub struct Pool {
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    pub reserve_a: u64,
    pub reserve_b: u64,
    pub fee_bps: u16,
    /// The Pipeline market PDA allowed to call `pipeline_swap`.
    pub market_signer: Pubkey,
    /// The program PDA that created this pool through `pipeline_seed_liquidity`, or the
    /// default key for pools created with `create_market`. It decides the pool's seeds.
    pub seeder: Pubkey,
    pub bump: u8,
}

impl Pool {
    /// The pool's PDA seeds, for signing as the owner of its vaults.
    fn signer_seeds<'a>(&'a self, fee_seed: &'a [u8], bump: &'a [u8]) -> Vec<&'a [u8]> {
        if self.seeder == Pubkey::default() {
            vec![
                b"pool",
                self.mint_a.as_ref(),
                self.mint_b.as_ref(),
                fee_seed,
                bump,
            ]
        } else {
            vec![
                b"seeded",
                self.seeder.as_ref(),
                self.mint_a.as_ref(),
                self.mint_b.as_ref(),
                fee_seed,
                bump,
            ]
        }
    }

    /// Input and output vaults for a trade; mint index 0 is A, 1 is B.
    fn vaults(&self, trade: &psi::TradeRequest) -> Result<(Pubkey, Pubkey)> {
        match (trade.input_index, trade.output_index) {
            (0, 1) => Ok((self.vault_a, self.vault_b)),
            (1, 0) => Ok((self.vault_b, self.vault_a)),
            _ => err!(CpError::WrongAccount),
        }
    }

    fn reserves(&self, trade: &psi::TradeRequest) -> (u64, u64) {
        if trade.input_index == 0 {
            (self.reserve_a, self.reserve_b)
        } else {
            (self.reserve_b, self.reserve_a)
        }
    }

    fn set_reserves(&mut self, trade: &psi::TradeRequest, reserve_in: u64, reserve_out: u64) {
        if trade.input_index == 0 {
            self.reserve_a = reserve_in;
            self.reserve_b = reserve_out;
        } else {
            self.reserve_b = reserve_in;
            self.reserve_a = reserve_out;
        }
    }

    /// The single pricing function used by both `pipeline_quote` and `pipeline_swap`.
    fn quote(&self, trade: &psi::TradeRequest) -> Result<psi::QuoteResult> {
        self.vaults(trade)?;
        let (reserve_in, reserve_out) = self.reserves(trade);
        // A one-sided pool has no meaningful price: either side could be drained.
        require!(
            reserve_in > 0 && reserve_out > 0,
            CpError::InsufficientLiquidity
        );
        match trade.mode {
            TradeMode::ExactIn => Ok(psi::QuoteResult {
                amount_in: trade.amount,
                amount_out: out_given_in(reserve_in, reserve_out, trade.amount, self.fee_bps),
            }),
            TradeMode::ExactOut => Ok(psi::QuoteResult {
                amount_in: in_given_out(reserve_in, reserve_out, trade.amount, self.fee_bps)
                    .ok_or(CpError::InsufficientLiquidity)?,
                amount_out: trade.amount,
            }),
        }
    }
}

/// `floor(r_out × in × (1 − fee) / (r_in + in × (1 − fee)))`.
pub fn out_given_in(reserve_in: u64, reserve_out: u64, amount_in: u64, fee_bps: u16) -> u64 {
    let in_after_fee = amount_in as u128 * (BPS - fee_bps as u128);
    let denominator = reserve_in as u128 * BPS + in_after_fee;
    if denominator == 0 {
        return 0;
    }
    // At most `reserve_out`, so it fits in a u64.
    mul_div(reserve_out as u128, in_after_fee, denominator).0 as u64
}

/// The least input for which [`out_given_in`] returns at least `amount_out`, or `None`
/// if the pool cannot provide it.
pub fn in_given_out(
    reserve_in: u64,
    reserve_out: u64,
    amount_out: u64,
    fee_bps: u16,
) -> Option<u64> {
    if amount_out == 0 {
        return Some(0);
    }
    if amount_out >= reserve_out {
        return None;
    }
    let denominator = (reserve_out - amount_out) as u128 * (BPS - fee_bps as u128);
    let (quotient, remainder) = mul_div(reserve_in as u128 * BPS, amount_out as u128, denominator);
    let rounded_up = quotient.checked_add((remainder > 0) as u128)?;
    u64::try_from(rounded_up).ok()
}

/// `a × b / d` as (quotient, remainder), with a 256-bit product so large pools and
/// trades can't overflow. The quotient saturates at `u128::MAX`; `d` must not be 0.
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
    // Long division of (hi, lo) by d. The running remainder stays below 2d, so a carry
    // bit plus a wrapping subtraction keeps it exact.
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

/// The Pipeline market PDA for a pool of this program.
fn pipeline_market_signer(pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[pipeline_seeds::MARKET, crate::ID.as_ref(), pool.as_ref()],
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
    require!(data.len() >= 72, CpError::WrongAccount);
    Ok(u64::from_le_bytes(data[64..72].try_into().unwrap()))
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

/// The fee part of a pool's seeds: the first two bytes of the init data, which are the
/// Borsh-encoded `fee_bps`.
fn fee_seed(data: &[u8]) -> &[u8] {
    data.get(..2).unwrap_or(&[0, 0])
}

/// Accounts passed by the router to `pipeline_init_market`: the four fixed accounts,
/// then this strategy's own.
#[derive(Accounts)]
#[instruction(data: Vec<u8>)]
pub struct InitMarket<'info> {
    pub market_signer: Signer<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + Pool::INIT_SPACE,
        seeds = [b"pool", mint_a.key().as_ref(), mint_b.key().as_ref(), fee_seed(&data)],
        bump,
    )]
    pub pool: Account<'info, Pool>,
    /// The router's setup payer, funded by the market creator.
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub mint_a: InterfaceAccount<'info, Mint>,
    pub mint_b: InterfaceAccount<'info, Mint>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = mint_a,
        associated_token::authority = pool,
        associated_token::token_program = token_program_a,
    )]
    pub vault_a: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = mint_b,
        associated_token::authority = pool,
        associated_token::token_program = token_program_b,
    )]
    pub vault_b: InterfaceAccount<'info, TokenAccount>,
    pub token_program_a: Interface<'info, TokenInterface>,
    pub token_program_b: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

/// Accounts for `pipeline_seed_liquidity`: the fixed seed accounts, three per mint, then
/// the pool's vaults.
#[derive(Accounts)]
#[instruction(request: psi::SeedRequest)]
pub struct SeedLiquidity<'info> {
    pub seeder: Signer<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + Pool::INIT_SPACE,
        seeds = [
            b"seeded",
            seeder.key().as_ref(),
            mint_a.key().as_ref(),
            mint_b.key().as_ref(),
            fee_seed(&request.data),
        ],
        bump,
    )]
    pub pool: Account<'info, Pool>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: this pool issues no LP shares.
    pub lp_recipient: UncheckedAccount<'info>,
    pub mint_a: InterfaceAccount<'info, Mint>,
    /// CHECK: the token program checks the source and the seeder's authority.
    #[account(mut)]
    pub escrow_a: UncheckedAccount<'info>,
    pub token_program_a: Interface<'info, TokenInterface>,
    pub mint_b: InterfaceAccount<'info, Mint>,
    /// CHECK: the token program checks the source and the seeder's authority.
    #[account(mut)]
    pub escrow_b: UncheckedAccount<'info>,
    pub token_program_b: Interface<'info, TokenInterface>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = mint_a,
        associated_token::authority = pool,
        associated_token::token_program = token_program_a,
    )]
    pub vault_a: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = mint_b,
        associated_token::authority = pool,
        associated_token::token_program = token_program_b,
    )]
    pub vault_b: InterfaceAccount<'info, TokenAccount>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

#[derive(Accounts)]
pub struct MarketInfoAccounts<'info> {
    pub pool: Account<'info, Pool>,
}

/// The fixed trade accounts, read-only, for quotes and account resolution. Only the
/// pool is read; the others may be placeholders.
#[derive(Accounts)]
pub struct ReadTrade<'info> {
    /// CHECK: unused in reads.
    pub market_signer: UncheckedAccount<'info>,
    pub pool: Account<'info, Pool>,
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
    pub pool: Account<'info, Pool>,
    /// CHECK: identity only; never a signer here.
    pub trader: UncheckedAccount<'info>,
    /// CHECK: receives refunds; the token program checks it.
    #[account(mut)]
    pub trader_input: UncheckedAccount<'info>,
    /// CHECK: receives output; the token program checks it.
    #[account(mut)]
    pub trader_output: UncheckedAccount<'info>,
    /// CHECK: compared with the pool's vault for the input mint.
    #[account(mut)]
    pub input_vault: UncheckedAccount<'info>,
    pub input_mint: InterfaceAccount<'info, Mint>,
    pub output_mint: InterfaceAccount<'info, Mint>,
    pub input_token_program: Interface<'info, TokenInterface>,
    pub output_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    pub depositor: Signer<'info>,
    #[account(mut, has_one = mint_a, has_one = mint_b, has_one = vault_a, has_one = vault_b)]
    pub pool: Account<'info, Pool>,
    /// CHECK: the token program checks the source.
    #[account(mut)]
    pub depositor_a: UncheckedAccount<'info>,
    /// CHECK: the token program checks the source.
    #[account(mut)]
    pub depositor_b: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub vault_a: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub vault_b: UncheckedAccount<'info>,
    pub mint_a: InterfaceAccount<'info, Mint>,
    pub mint_b: InterfaceAccount<'info, Mint>,
    pub token_program_a: Interface<'info, TokenInterface>,
    pub token_program_b: Interface<'info, TokenInterface>,
}

#[error_code]
pub enum CpError {
    #[msg("Init data is not valid InitParams")]
    BadInitData,
    #[msg("Caller is not this pool's Pipeline market")]
    NotPipeline,
    #[msg("Unexpected account")]
    WrongAccount,
    #[msg("Not enough input for the requested output")]
    InsufficientInput,
    #[msg("Pool cannot provide that much output")]
    InsufficientLiquidity,
    #[msg("An empty pool needs both tokens in its first deposit")]
    OneSidedDeposit,
}
