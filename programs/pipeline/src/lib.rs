//! Pipeline router.
//!
//! Keeps a registry of strategy programs and their markets, routes swaps into those
//! strategies, enforces the trader's minimum output, charges Pipeline's fixed fee on
//! the input the trader actually parted with, and emits one standard event per trade.
//!
//! Strategies hold their own liquidity. During a swap a strategy receives:
//! - a signature from the market's PDA, so it can tell the call came from Pipeline
//!   for that specific market;
//! - the trader's token accounts as writable but never the trader's signature, so it
//!   can pay the trader but cannot take more from them.
//!
//! Fee accounts belong to a separate per-market PDA that never signs during a swap or
//! market setup; it signs only sweeps to the treasury.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::{
    get_return_data, invoke, invoke_signed, set_return_data,
};
use anchor_lang::solana_program::{bpf_loader_upgradeable, system_program};
use pipeline_strategy_interface::{self as psi, seeds, TradeMode};
use token::associated_token_address;

mod token;

declare_id!("PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34");

/// Hard ceiling on the admin-adjustable listing fee. Placeholder until the mainnet
/// price is chosen.
pub const MAX_LISTING_FEE_LAMPORTS: u64 = 1_000_000_000_000;
pub const MAX_METADATA_URI_LEN: usize = 200;

#[program]
pub mod pipeline {
    use super::*;

    /// Creates the global config. Only the router's upgrade authority can call this,
    /// so nobody can claim the config first after deployment.
    pub fn initialize_config(ctx: Context<InitializeConfig>, args: ConfigArgs) -> Result<()> {
        check_listing_fee(args.listing_fee_lamports)?;
        let config = &mut ctx.accounts.config;
        config.admin = ctx.accounts.admin.key();
        config.paused = false;
        config.listing_fee_lamports = args.listing_fee_lamports;
        config.listing_fee_recipient = args.listing_fee_recipient;
        config.treasury_owner = args.treasury_owner;
        config.bump = ctx.bumps.config;
        Ok(())
    }

    /// Changes admin settings. Every field is optional.
    ///
    /// Pausing needs the router to still be upgradeable: once it is frozen, the pause
    /// power ends, and `lift_pause` lets anyone clear a pause left in place.
    pub fn update_config(ctx: Context<UpdateConfig>, args: UpdateConfigArgs) -> Result<()> {
        if args.paused == Some(true) {
            require!(
                ctx.accounts.router_data.upgrade_authority_address.is_some(),
                PipelineError::FrozenRouterCannotPause
            );
        }
        let config = &mut ctx.accounts.config;
        if let Some(admin) = args.admin {
            config.admin = admin;
        }
        if let Some(paused) = args.paused {
            config.paused = paused;
        }
        if let Some(fee) = args.listing_fee_lamports {
            check_listing_fee(fee)?;
            config.listing_fee_lamports = fee;
        }
        if let Some(recipient) = args.listing_fee_recipient {
            config.listing_fee_recipient = recipient;
        }
        if let Some(owner) = args.treasury_owner {
            config.treasury_owner = owner;
        }
        emit_cpi!(ConfigChanged {
            admin: config.admin,
            paused: config.paused,
            listing_fee_lamports: config.listing_fee_lamports,
            listing_fee_recipient: config.listing_fee_recipient,
            treasury_owner: config.treasury_owner,
        });
        Ok(())
    }

    /// Clears the pause once the router is frozen. Anyone can call it, so a pause can't
    /// outlive the router's upgradeability even if the admin key is lost.
    pub fn lift_pause(ctx: Context<LiftPause>) -> Result<()> {
        require!(
            ctx.accounts.router_data.upgrade_authority_address.is_none(),
            PipelineError::RouterNotFrozen
        );
        let config = &mut ctx.accounts.config;
        config.paused = false;
        emit_cpi!(ConfigChanged {
            admin: config.admin,
            paused: config.paused,
            listing_fee_lamports: config.listing_fee_lamports,
            listing_fee_recipient: config.listing_fee_recipient,
            treasury_owner: config.treasury_owner,
        });
        Ok(())
    }

    /// Registers a strategy program for the current listing fee, which must be at most
    /// `max_listing_fee_lamports` (so a fee change can't surprise a pending registration).
    ///
    /// The registrar must be the program's upgrade authority, or the program's own
    /// `["pipeline-register"]` PDA (signing through a CPI) for programs that are or
    /// will be immutable.
    pub fn register_strategy(
        ctx: Context<RegisterStrategy>,
        metadata_uri: String,
        interface_version: u8,
        max_listing_fee_lamports: u64,
    ) -> Result<()> {
        let config = &ctx.accounts.config;
        require!(!config.paused, PipelineError::Paused);
        require!(
            config.listing_fee_lamports <= max_listing_fee_lamports,
            PipelineError::ListingFeeAboveMaximum
        );
        require!(
            interface_version == psi::INTERFACE_VERSION,
            PipelineError::UnsupportedInterfaceVersion
        );
        require!(
            metadata_uri.len() <= MAX_METADATA_URI_LEN,
            PipelineError::MetadataUriTooLong
        );
        check_registrar(
            &ctx.accounts.registrar,
            &ctx.accounts.strategy_program,
            &ctx.accounts.strategy_program_data,
        )?;

        let fee = config.listing_fee_lamports;
        if fee > 0 {
            invoke(
                &anchor_lang::solana_program::system_instruction::transfer(
                    ctx.accounts.payer.key,
                    ctx.accounts.listing_fee_recipient.key,
                    fee,
                ),
                &[
                    ctx.accounts.payer.to_account_info(),
                    ctx.accounts.listing_fee_recipient.to_account_info(),
                    ctx.accounts.system_program.to_account_info(),
                ],
            )?;
        }

        let strategy = &mut ctx.accounts.strategy;
        strategy.program_id = ctx.accounts.strategy_program.key();
        strategy.metadata_uri = metadata_uri;
        strategy.interface_version = interface_version;
        strategy.registered_at = Clock::get()?.unix_timestamp;
        strategy.market_count = 0;
        strategy.bump = ctx.bumps.strategy;

        emit_cpi!(StrategyRegistered {
            strategy: strategy.program_id,
            fee_paid: fee,
            metadata_uri: strategy.metadata_uri.clone(),
        });
        Ok(())
    }

    /// Points a strategy at new metadata. Same authority rules as registration.
    pub fn update_metadata(ctx: Context<UpdateMetadata>, metadata_uri: String) -> Result<()> {
        require!(
            metadata_uri.len() <= MAX_METADATA_URI_LEN,
            PipelineError::MetadataUriTooLong
        );
        check_registrar(
            &ctx.accounts.registrar,
            &ctx.accounts.strategy_program,
            &ctx.accounts.strategy_program_data,
        )?;
        let strategy = &mut ctx.accounts.strategy;
        strategy.metadata_uri = metadata_uri;
        emit_cpi!(MetadataUpdated {
            strategy: strategy.program_id,
            metadata_uri: strategy.metadata_uri.clone(),
        });
        Ok(())
    }

    /// Creates a market: calls the strategy's `pipeline_init_market`, then
    /// `pipeline_market_info`, and records the result.
    ///
    /// The creator's signature is never passed to the strategy. Instead the router moves
    /// `max_creator_lamports` into a fresh, router-controlled setup payer, lets the
    /// strategy pay rent from it, checks it is still a plain system account, and returns
    /// whatever is left to the creator. So the most a strategy's setup can take from the
    /// creator is `max_creator_lamports`.
    ///
    /// Remaining accounts are passed after the fixed accounts with their writable flags.
    /// They keep their signer flag only if they are fresh (empty, system-owned, no
    /// lamports), as a new keypair account the strategy is meant to create would be.
    /// The router can't tell such an account from a wallet that holds no SOL but owns
    /// token accounts or holds authorities (including one emptied earlier in the same
    /// transaction), or from a program's PDA signing through a CPI. So callers must only
    /// sign with new keypairs here: the strategy can use a fresh signer's signature
    /// however it likes.
    pub fn create_market<'info>(
        ctx: Context<'info, CreateMarket<'info>>,
        init_data: Vec<u8>,
        max_creator_lamports: u64,
    ) -> Result<()> {
        require!(!ctx.accounts.config.paused, PipelineError::Paused);
        let strategy_program = ctx.accounts.strategy_program.key();
        let market_state = ctx.accounts.market_state.key();
        let market_key = ctx.accounts.market.key();
        let market_bump = ctx.bumps.market;
        let setup_bump = ctx.bumps.setup_payer;
        let market_seeds: &[&[u8]] = &[
            seeds::MARKET,
            strategy_program.as_ref(),
            market_state.as_ref(),
            &[market_bump],
        ];
        let setup_seeds: &[&[u8]] = &[seeds::SETUP_PAYER, market_key.as_ref(), &[setup_bump]];
        let setup_payer = ctx.accounts.setup_payer.to_account_info();
        let system_program = ctx.accounts.system_program.to_account_info();

        // Decide which accounts keep their signature before the creator pays anything: a
        // creator emptied by the payment below would otherwise look fresh. The creator's
        // own signature is never passed on.
        let creator = ctx.accounts.creator.key();
        let keeps_signature = |account: &AccountInfo| *account.key != creator && is_fresh(account);
        let market_state_signs = keeps_signature(&ctx.accounts.market_state.to_account_info());
        let remaining_sign: Vec<bool> =
            ctx.remaining_accounts.iter().map(keeps_signature).collect();

        if max_creator_lamports > 0 {
            invoke(
                &anchor_lang::solana_program::system_instruction::transfer(
                    ctx.accounts.creator.key,
                    setup_payer.key,
                    max_creator_lamports,
                ),
                &[
                    ctx.accounts.creator.to_account_info(),
                    setup_payer.clone(),
                    system_program.clone(),
                ],
            )?;
        }

        let mut metas = vec![
            AccountMeta::new_readonly(market_key, true),
            meta_like(
                &ctx.accounts.market_state.to_account_info(),
                market_state_signs,
            ),
            AccountMeta::new(setup_payer.key(), true),
            AccountMeta::new_readonly(system_program::ID, false),
        ];
        let mut infos = vec![
            ctx.accounts.market.to_account_info(),
            ctx.accounts.market_state.to_account_info(),
            setup_payer.clone(),
            system_program.clone(),
        ];
        for (account, signs) in ctx.remaining_accounts.iter().zip(remaining_sign) {
            metas.push(meta_like(account, signs));
            infos.push(account.clone());
        }
        infos.push(ctx.accounts.strategy_program.to_account_info());
        invoke_signed(
            &Instruction {
                program_id: strategy_program,
                accounts: metas,
                data: psi::instruction_data(psi::discriminator::INIT_MARKET, &init_data),
            },
            &infos,
            &[market_seeds, setup_seeds],
        )?;

        // The strategy may only have spent the setup payer's SOL, not repurposed it.
        require!(
            *setup_payer.owner == system_program::ID && setup_payer.data_is_empty(),
            PipelineError::SetupPayerMisused
        );
        let leftover = setup_payer.lamports();
        if leftover > 0 {
            invoke_signed(
                &anchor_lang::solana_program::system_instruction::transfer(
                    setup_payer.key,
                    ctx.accounts.creator.key,
                    leftover,
                ),
                &[
                    setup_payer.clone(),
                    ctx.accounts.creator.to_account_info(),
                    system_program.clone(),
                ],
                &[setup_seeds],
            )?;
        }

        let info = fetch_market_info(
            &ctx.accounts.strategy_program,
            &ctx.accounts.market_state,
            ctx.remaining_accounts,
        )?;
        record_market(
            &mut ctx.accounts.market,
            &mut ctx.accounts.strategy,
            &ctx.accounts.market_state,
            info,
            ctx.accounts.creator.key(),
            market_bump,
        )?;
        let market = &ctx.accounts.market;
        emit_cpi!(MarketCreated {
            market: market.key(),
            strategy: strategy_program,
            market_state,
            mints: market.mints[..market.mint_count as usize].to_vec(),
            created_by: market.created_by,
        });
        Ok(())
    }

    /// Records a market that the strategy created by itself. Anyone can call this.
    pub fn register_existing_market<'info>(
        ctx: Context<'info, RegisterExistingMarket<'info>>,
    ) -> Result<()> {
        require!(!ctx.accounts.config.paused, PipelineError::Paused);
        let info = fetch_market_info(
            &ctx.accounts.strategy_program,
            &ctx.accounts.market_state,
            ctx.remaining_accounts,
        )?;
        record_market(
            &mut ctx.accounts.market,
            &mut ctx.accounts.strategy,
            &ctx.accounts.market_state,
            info,
            ctx.accounts.payer.key(),
            ctx.bumps.market,
        )?;
        let market = &ctx.accounts.market;
        emit_cpi!(MarketCreated {
            market: market.key(),
            strategy: market.strategy_program,
            market_state: market.market_state,
            mints: market.mints[..market.mint_count as usize].to_vec(),
            created_by: market.created_by,
        });
        Ok(())
    }

    /// Creates a market's fee account for one of its mints. Swaps selling that mint
    /// need it to exist. Anyone can pay for it; it is idempotent.
    pub fn init_fee_account(ctx: Context<InitFeeAccount>) -> Result<()> {
        let market = &ctx.accounts.market;
        mint_index(market, &ctx.accounts.mint.key())?;
        require_keys_eq!(
            *ctx.accounts.mint.owner,
            ctx.accounts.token_program.key(),
            PipelineError::WrongTokenProgram
        );
        token::create_associated_token_account_idempotent(
            &ctx.accounts.associated_token_program,
            &ctx.accounts.payer,
            &ctx.accounts.fee_account,
            &ctx.accounts.fee_authority,
            &ctx.accounts.mint,
            &ctx.accounts.system_program,
            &ctx.accounts.token_program,
        )
    }

    /// Swaps through a market's strategy: sends the input, calls the strategy, measures
    /// what was spent and received, and charges the fee.
    ///
    /// Remaining accounts are the strategy's own accounts, followed by
    /// `input_hook_accounts` accounts that the input mint's transfer hook needs. The
    /// strategy receives all of them, without signer privileges.
    pub fn swap<'info>(ctx: Context<'info, Swap<'info>>, args: SwapArgs) -> Result<()> {
        require!(!ctx.accounts.config.paused, PipelineError::Paused);
        let accounts = &ctx.accounts;
        let market = &accounts.market;

        let input_mint = accounts.input_mint.key();
        let output_mint = accounts.output_mint.key();
        require_keys_neq!(input_mint, output_mint, PipelineError::SameMint);
        let input_index = mint_index(market, &input_mint)?;
        let output_index = mint_index(market, &output_mint)?;
        require_keys_eq!(
            accounts.input_vault.key(),
            market.vaults[input_index],
            PipelineError::WrongVault
        );
        require_keys_eq!(
            *accounts.input_mint.owner,
            accounts.input_token_program.key(),
            PipelineError::WrongTokenProgram
        );
        require_keys_eq!(
            *accounts.output_mint.owner,
            accounts.output_token_program.key(),
            PipelineError::WrongTokenProgram
        );
        let expected_fee_account = associated_token_address(
            &accounts.fee_authority.key(),
            &input_mint,
            &accounts.input_token_program.key(),
        );
        require_keys_eq!(
            accounts.fee_account.key(),
            expected_fee_account,
            PipelineError::WrongFeeAccount
        );
        if args.mode == TradeMode::ExactOut {
            require!(
                market.supports_exact_out,
                PipelineError::ExactOutUnsupported
            );
        }
        require!(
            args.integrator_fee_bps == 0 || accounts.integrator_fee_account.is_some(),
            PipelineError::MissingIntegratorFeeAccount
        );
        // Fees must leave the trader's input account. Paying a fee "to" that same
        // account would record a fee in the event that was never collected.
        require_keys_neq!(
            accounts.fee_account.key(),
            accounts.trader_input.key(),
            PipelineError::FeeToTraderInput
        );
        if let Some(integrator_account) = &accounts.integrator_fee_account {
            require_keys_neq!(
                integrator_account.key(),
                accounts.trader_input.key(),
                PipelineError::FeeToTraderInput
            );
        }

        let remaining = ctx.remaining_accounts;
        let hook_start = psi::settlement::hook_split(remaining.len(), args.input_hook_accounts)
            .ok_or(PipelineError::BadHookAccounts)?;
        let hook_accounts = &remaining[hook_start..];

        let trader_input = accounts.trader_input.to_account_info();
        let trader_output = accounts.trader_output.to_account_info();
        let input_before = token::checked_balance(
            &trader_input,
            &input_mint,
            &accounts.input_token_program.key(),
        )?;
        let output_before = token::checked_balance(
            &trader_output,
            &output_mint,
            &accounts.output_token_program.key(),
        )?;
        let input_vault = accounts.input_vault.to_account_info();
        let vault_before = token::checked_balance(
            &input_vault,
            &input_mint,
            &accounts.input_token_program.key(),
        )?;
        let input_decimals = token::decimals(&accounts.input_mint.to_account_info())?;

        token::transfer_checked(
            &accounts.input_token_program.to_account_info(),
            &trader_input,
            &accounts.input_mint.to_account_info(),
            &accounts.input_vault.to_account_info(),
            &accounts.trader.to_account_info(),
            hook_accounts,
            args.amount_in,
            input_decimals,
            &[],
        )?;

        let request = psi::SwapRequest {
            trade: psi::TradeRequest {
                mode: args.mode,
                input_index: input_index as u8,
                output_index: output_index as u8,
                amount: match args.mode {
                    TradeMode::ExactIn => args.amount_in,
                    TradeMode::ExactOut => args.amount_out,
                },
                trader: Some(accounts.trader.key().to_bytes()),
                extra: args.extra.clone(),
            },
            amount_sent: args.amount_in,
            min_amount_out: args.min_amount_out,
        };
        call_strategy_swap(accounts, remaining, &request)?;

        // Re-check the accounts after the call: the strategy can't have swapped them for
        // something else, but a closed or reassigned account must not be read as a
        // token account.
        let measured = psi::settlement::Measured {
            input_before,
            input_after: token::checked_balance(
                &trader_input,
                &input_mint,
                &accounts.input_token_program.key(),
            )?,
            output_before,
            output_after: token::checked_balance(
                &trader_output,
                &output_mint,
                &accounts.output_token_program.key(),
            )?,
            vault_before,
            vault_after: token::checked_balance(
                &input_vault,
                &input_mint,
                &accounts.input_token_program.key(),
            )?,
        };
        // The fee base is the trader's net spend, or what the market's input vault kept
        // if that is more (other code, such as the output mint's transfer hook, could top
        // up the trader's input account during the call), at most `amount_in`. See
        // `pipeline_strategy_interface::settlement`.
        let settlement = psi::settlement::settle(
            &measured,
            args.amount_in,
            args.min_amount_out,
            args.integrator_fee_bps,
        )
        .map_err(settle_error)?;
        let net_sent = settlement.net_sent;
        let received = settlement.received;
        let pipeline_fee = settlement.pipeline_fee;
        let integrator_fee = settlement.integrator_fee;

        if pipeline_fee > 0 {
            token::transfer_checked(
                &accounts.input_token_program.to_account_info(),
                &trader_input,
                &accounts.input_mint.to_account_info(),
                &accounts.fee_account.to_account_info(),
                &accounts.trader.to_account_info(),
                hook_accounts,
                pipeline_fee,
                input_decimals,
                &[],
            )?;
        }

        let integrator = accounts.integrator_fee_account.as_ref().map(|a| a.key());
        if integrator_fee > 0 {
            let integrator_account = accounts
                .integrator_fee_account
                .as_ref()
                .ok_or(PipelineError::MissingIntegratorFeeAccount)?;
            token::transfer_checked(
                &accounts.input_token_program.to_account_info(),
                &trader_input,
                &accounts.input_mint.to_account_info(),
                &integrator_account.to_account_info(),
                &accounts.trader.to_account_info(),
                hook_accounts,
                integrator_fee,
                input_decimals,
                &[],
            )?;
        }

        let clock = Clock::get()?;
        emit_cpi!(SwapEvent {
            market: market.key(),
            strategy: market.strategy_program,
            trader: accounts.trader.key(),
            input_mint,
            output_mint,
            mode: args.mode as u8,
            net_sent,
            received,
            pipeline_fee,
            integrator,
            integrator_fee,
            slot: clock.slot,
            unix_timestamp: clock.unix_timestamp,
        });
        let result = SwapResult {
            net_sent,
            received,
            pipeline_fee,
            integrator_fee,
        };
        let mut encoded = Vec::with_capacity(32);
        result.serialize(&mut encoded)?;
        set_return_data(&encoded);
        Ok(())
    }

    /// Moves a market's collected fees in one mint to the treasury. Anyone can call it.
    pub fn sweep_fees<'info>(ctx: Context<'info, SweepFees<'info>>) -> Result<()> {
        let accounts = &ctx.accounts;
        let market = &accounts.market;
        let mint = accounts.mint.key();
        mint_index(market, &mint)?;
        let token_program = accounts.token_program.key();
        require_keys_eq!(
            *accounts.mint.owner,
            token_program,
            PipelineError::WrongTokenProgram
        );
        require_keys_eq!(
            accounts.fee_account.key(),
            associated_token_address(&accounts.fee_authority.key(), &mint, &token_program),
            PipelineError::WrongFeeAccount
        );
        require_keys_eq!(
            accounts.treasury_account.key(),
            associated_token_address(&accounts.config.treasury_owner, &mint, &token_program),
            PipelineError::WrongTreasuryAccount
        );

        let fee_account = accounts.fee_account.to_account_info();
        let amount = token::balance(&fee_account)?;
        if amount == 0 {
            return Ok(());
        }
        let market_key = market.key();
        let fee_seeds: &[&[u8]] = &[
            seeds::FEE_AUTHORITY,
            market_key.as_ref(),
            &[market.fee_authority_bump],
        ];
        token::transfer_checked(
            &accounts.token_program.to_account_info(),
            &fee_account,
            &accounts.mint.to_account_info(),
            &accounts.treasury_account.to_account_info(),
            &accounts.fee_authority.to_account_info(),
            ctx.remaining_accounts,
            amount,
            token::decimals(&accounts.mint.to_account_info())?,
            &[fee_seeds],
        )?;
        emit_cpi!(FeesSwept {
            market: market_key,
            mint,
            amount,
        });
        Ok(())
    }
}

/// Builds the `pipeline_swap` call into the strategy and invokes it, signed by the
/// market PDA. The trader is passed without signer privileges.
fn call_strategy_swap<'info>(
    accounts: &Swap<'info>,
    remaining: &[AccountInfo<'info>],
    request: &psi::SwapRequest,
) -> Result<()> {
    let market = &accounts.market;
    let mut metas = vec![
        AccountMeta::new_readonly(market.key(), true),
        AccountMeta::new(accounts.market_state.key(), false),
        AccountMeta::new_readonly(accounts.trader.key(), false),
        AccountMeta::new(accounts.trader_input.key(), false),
        AccountMeta::new(accounts.trader_output.key(), false),
        AccountMeta::new(accounts.input_vault.key(), false),
        AccountMeta::new_readonly(accounts.input_mint.key(), false),
        AccountMeta::new_readonly(accounts.output_mint.key(), false),
        AccountMeta::new_readonly(accounts.input_token_program.key(), false),
        AccountMeta::new_readonly(accounts.output_token_program.key(), false),
    ];
    let mut infos = vec![
        market.to_account_info(),
        accounts.market_state.to_account_info(),
        accounts.trader.to_account_info(),
        accounts.trader_input.to_account_info(),
        accounts.trader_output.to_account_info(),
        accounts.input_vault.to_account_info(),
        accounts.input_mint.to_account_info(),
        accounts.output_mint.to_account_info(),
        accounts.input_token_program.to_account_info(),
        accounts.output_token_program.to_account_info(),
    ];
    for account in remaining {
        metas.push(meta_like(account, false));
        infos.push(account.clone());
    }
    infos.push(accounts.strategy_program.to_account_info());

    let strategy_program = market.strategy_program;
    let market_state = market.market_state;
    let signer_seeds: &[&[u8]] = &[
        seeds::MARKET,
        strategy_program.as_ref(),
        market_state.as_ref(),
        &[market.bump],
    ];
    invoke_signed(
        &Instruction {
            program_id: strategy_program,
            accounts: metas,
            data: psi::instruction_data(psi::discriminator::SWAP, request),
        },
        &infos,
        &[signer_seeds],
    )?;
    Ok(())
}

/// A listing fee is at most the compiled cap, and either 0 or at least the rent-exempt
/// minimum for an empty account: a smaller payment to an unfunded recipient would fail,
/// and with it every registration.
fn check_listing_fee(fee: u64) -> Result<()> {
    require!(
        fee <= MAX_LISTING_FEE_LAMPORTS,
        PipelineError::ListingFeeTooHigh
    );
    require!(
        fee == 0 || fee >= Rent::get()?.minimum_balance(0),
        PipelineError::ListingFeeBelowRent
    );
    Ok(())
}

/// The router's error for each reason [`psi::settlement::settle`] refuses a swap.
fn settle_error(error: psi::settlement::SettleError) -> Error {
    use psi::settlement::SettleError;
    match error {
        SettleError::InsufficientOutput => PipelineError::InsufficientOutput.into(),
        SettleError::SpendLimitExceeded => PipelineError::SpendLimitExceeded.into(),
        SettleError::OutputWithoutInput => PipelineError::OutputWithoutInput.into(),
        SettleError::NothingSwapped => PipelineError::NothingSwapped.into(),
        SettleError::MathOverflow => PipelineError::MathOverflow.into(),
    }
}

/// Calls the strategy's read-only `pipeline_market_info` and decodes its return data.
fn fetch_market_info<'info>(
    strategy_program: &UncheckedAccount<'info>,
    market_state: &UncheckedAccount<'info>,
    remaining: &[AccountInfo<'info>],
) -> Result<psi::MarketInfo> {
    require_keys_eq!(
        *market_state.owner,
        strategy_program.key(),
        PipelineError::MarketStateNotOwnedByStrategy
    );
    let mut metas = vec![AccountMeta::new_readonly(market_state.key(), false)];
    let mut infos = vec![market_state.to_account_info()];
    for account in remaining {
        metas.push(AccountMeta::new_readonly(account.key(), false));
        infos.push(account.clone());
    }
    infos.push(strategy_program.to_account_info());
    invoke(
        &Instruction {
            program_id: strategy_program.key(),
            accounts: metas,
            data: psi::discriminator::MARKET_INFO.to_vec(),
        },
        &infos,
    )?;
    let (program, data) = get_return_data().ok_or(PipelineError::BadMarketInfo)?;
    require_keys_eq!(
        program,
        strategy_program.key(),
        PipelineError::BadMarketInfo
    );
    let info = psi::MarketInfo::try_from_slice(&data).map_err(|_| PipelineError::BadMarketInfo)?;

    let count = info.mints.len();
    require!(
        (psi::MIN_MARKET_MINTS..=psi::MAX_MARKET_MINTS).contains(&count)
            && info.vaults.len() == count,
        PipelineError::BadMarketInfo
    );
    for (i, mint) in info.mints.iter().enumerate() {
        require!(
            !info.mints[..i].contains(mint),
            PipelineError::BadMarketInfo
        );
    }
    Ok(info)
}

fn record_market(
    market: &mut Account<Market>,
    strategy: &mut Account<Strategy>,
    market_state: &UncheckedAccount,
    info: psi::MarketInfo,
    created_by: Pubkey,
    bump: u8,
) -> Result<()> {
    market.strategy_program = strategy.program_id;
    market.market_state = market_state.key();
    market.mint_count = info.mints.len() as u8;
    for (i, (mint, vault)) in info.mints.iter().zip(&info.vaults).enumerate() {
        market.mints[i] = Pubkey::new_from_array(*mint);
        market.vaults[i] = Pubkey::new_from_array(*vault);
    }
    market.supports_exact_out = info.supports_exact_out;
    market.created_by = created_by;
    market.created_at = Clock::get()?.unix_timestamp;
    market.bump = bump;
    let (_, fee_bump) =
        Pubkey::find_program_address(&[seeds::FEE_AUTHORITY, market.key().as_ref()], &crate::ID);
    market.fee_authority_bump = fee_bump;
    strategy.market_count = strategy
        .market_count
        .checked_add(1)
        .ok_or(PipelineError::MathOverflow)?;
    Ok(())
}

fn mint_index(market: &Market, mint: &Pubkey) -> Result<usize> {
    market.mints[..market.mint_count as usize]
        .iter()
        .position(|m| m == mint)
        .ok_or_else(|| error!(PipelineError::MintNotInMarket))
}

/// An empty, system-owned account with no lamports, as a new keypair account that a
/// strategy may create is. A wallet with no SOL looks the same, which is why callers
/// must sign `create_market` only with new keypairs.
fn is_fresh(account: &AccountInfo) -> bool {
    account.lamports() == 0 && account.data_is_empty() && *account.owner == system_program::ID
}

/// Copies an account's writable flag (and optionally its signer flag) into a CPI meta.
fn meta_like(account: &AccountInfo, keep_signer: bool) -> AccountMeta {
    let signer = keep_signer && account.is_signer;
    if account.is_writable {
        AccountMeta::new(account.key(), signer)
    } else {
        AccountMeta::new_readonly(account.key(), signer)
    }
}

/// Accepts the program's upgrade authority, or the program's own registration PDA.
fn check_registrar(
    registrar: &Signer,
    strategy_program: &UncheckedAccount,
    strategy_program_data: &UncheckedAccount,
) -> Result<()> {
    require_keys_eq!(
        *strategy_program.owner,
        bpf_loader_upgradeable::ID,
        PipelineError::UnsupportedLoader
    );
    let program_data_address =
        upgradeable_programdata_address(&strategy_program.try_borrow_data()?)
            .ok_or(PipelineError::UnsupportedLoader)?;
    require_keys_eq!(
        *strategy_program_data.owner,
        bpf_loader_upgradeable::ID,
        PipelineError::WrongProgramData
    );
    require_keys_eq!(
        strategy_program_data.key(),
        program_data_address,
        PipelineError::WrongProgramData
    );
    let upgrade_authority = upgrade_authority(&strategy_program_data.try_borrow_data()?)
        .ok_or(PipelineError::WrongProgramData)?;

    if upgrade_authority == Some(registrar.key()) {
        return Ok(());
    }
    let (register_pda, _) =
        Pubkey::find_program_address(&[seeds::REGISTER], &strategy_program.key());
    require_keys_eq!(
        registrar.key(),
        register_pda,
        PipelineError::NotStrategyOwner
    );
    Ok(())
}

/// Reads `UpgradeableLoaderState::Program { programdata_address }` (bincode: u32 tag 2).
fn upgradeable_programdata_address(data: &[u8]) -> Option<Pubkey> {
    if data.len() < 36 || data[..4] != 2u32.to_le_bytes() {
        return None;
    }
    Some(Pubkey::new_from_array(data[4..36].try_into().ok()?))
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

// ---------------------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    pub paused: bool,
    pub listing_fee_lamports: u64,
    /// Receives listing fees.
    pub listing_fee_recipient: Pubkey,
    /// Owner of the treasury token accounts that swept trading fees go to.
    pub treasury_owner: Pubkey,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Strategy {
    pub program_id: Pubkey,
    #[max_len(MAX_METADATA_URI_LEN)]
    pub metadata_uri: String,
    pub interface_version: u8,
    pub registered_at: i64,
    pub market_count: u64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Market {
    pub strategy_program: Pubkey,
    /// The strategy-owned account that holds this market's state.
    pub market_state: Pubkey,
    pub mint_count: u8,
    pub mints: [Pubkey; psi::MAX_MARKET_MINTS],
    /// Where input for each mint is sent, as declared by the strategy at creation.
    pub vaults: [Pubkey; psi::MAX_MARKET_MINTS],
    pub supports_exact_out: bool,
    pub created_by: Pubkey,
    pub created_at: i64,
    pub bump: u8,
    pub fee_authority_bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct ConfigArgs {
    pub listing_fee_lamports: u64,
    pub listing_fee_recipient: Pubkey,
    pub treasury_owner: Pubkey,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct UpdateConfigArgs {
    pub admin: Option<Pubkey>,
    pub paused: Option<bool>,
    pub listing_fee_lamports: Option<u64>,
    pub listing_fee_recipient: Option<Pubkey>,
    pub treasury_owner: Option<Pubkey>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct SwapArgs {
    pub mode: TradeMode,
    /// Exact-in: the amount sent to the strategy. Exact-out: the most that may be sent;
    /// the strategy refunds what it does not need.
    pub amount_in: u64,
    /// Exact-out only: the output the strategy must send (strategy vault units).
    pub amount_out: u64,
    /// The least the trader must receive, measured on their output account.
    pub min_amount_out: u64,
    pub integrator_fee_bps: u16,
    /// How many trailing remaining accounts are the input mint's transfer-hook accounts.
    pub input_hook_accounts: u8,
    pub extra: Vec<u8>,
}

/// Return data of `swap`, so a simulation doubles as a quote.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct SwapResult {
    pub net_sent: u64,
    pub received: u64,
    pub pipeline_fee: u64,
    pub integrator_fee: u64,
}

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        init,
        payer = admin,
        space = 8 + Config::INIT_SPACE,
        seeds = [seeds::CONFIG],
        bump,
    )]
    pub config: Account<'info, Config>,
    #[account(constraint = program.programdata_address()? == Some(program_data.key()))]
    pub program: Program<'info, crate::program::Pipeline>,
    #[account(constraint = program_data.upgrade_authority_address == Some(admin.key()) @ PipelineError::NotAdmin)]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [seeds::CONFIG], bump = config.bump, has_one = admin @ PipelineError::NotAdmin)]
    pub config: Account<'info, Config>,
    /// The router itself and its ProgramData, to tell whether it is still upgradeable.
    /// (`#[event_cpi]` already adds a `program` field.)
    #[account(constraint = router.programdata_address()? == Some(router_data.key()))]
    pub router: Program<'info, crate::program::Pipeline>,
    pub router_data: Account<'info, ProgramData>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct LiftPause<'info> {
    #[account(mut, seeds = [seeds::CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,
    /// The router itself and its ProgramData, to tell whether it is still upgradeable.
    /// (`#[event_cpi]` already adds a `program` field.)
    #[account(constraint = router.programdata_address()? == Some(router_data.key()))]
    pub router: Program<'info, crate::program::Pipeline>,
    pub router_data: Account<'info, ProgramData>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct RegisterStrategy<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub registrar: Signer<'info>,
    /// CHECK: a loader-v3 program; checked in `check_registrar`.
    pub strategy_program: UncheckedAccount<'info>,
    /// CHECK: the program's ProgramData; checked in `check_registrar`.
    pub strategy_program_data: UncheckedAccount<'info>,
    #[account(seeds = [seeds::CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,
    /// CHECK: receives SOL only; must match the config.
    #[account(mut, address = config.listing_fee_recipient @ PipelineError::WrongFeeRecipient)]
    pub listing_fee_recipient: UncheckedAccount<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + Strategy::INIT_SPACE,
        seeds = [seeds::STRATEGY, strategy_program.key().as_ref()],
        bump,
    )]
    pub strategy: Account<'info, Strategy>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct UpdateMetadata<'info> {
    pub registrar: Signer<'info>,
    /// CHECK: checked in `check_registrar`.
    pub strategy_program: UncheckedAccount<'info>,
    /// CHECK: checked in `check_registrar`.
    pub strategy_program_data: UncheckedAccount<'info>,
    #[account(mut, seeds = [seeds::STRATEGY, strategy_program.key().as_ref()], bump = strategy.bump)]
    pub strategy: Account<'info, Strategy>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct CreateMarket<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(seeds = [seeds::CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [seeds::STRATEGY, strategy_program.key().as_ref()], bump = strategy.bump)]
    pub strategy: Account<'info, Strategy>,
    /// CHECK: the registered strategy program (enforced by the `strategy` seeds).
    #[account(executable)]
    pub strategy_program: UncheckedAccount<'info>,
    /// CHECK: created by the strategy during this call; ownership checked afterwards.
    #[account(mut)]
    pub market_state: UncheckedAccount<'info>,
    #[account(
        init,
        payer = creator,
        space = 8 + Market::INIT_SPACE,
        seeds = [seeds::MARKET, strategy_program.key().as_ref(), market_state.key().as_ref()],
        bump,
    )]
    pub market: Account<'info, Market>,
    /// CHECK: router PDA that pays for the strategy's setup; funded and emptied here.
    #[account(mut, seeds = [seeds::SETUP_PAYER, market.key().as_ref()], bump)]
    pub setup_payer: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct RegisterExistingMarket<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [seeds::CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [seeds::STRATEGY, strategy_program.key().as_ref()], bump = strategy.bump)]
    pub strategy: Account<'info, Strategy>,
    /// CHECK: the registered strategy program (enforced by the `strategy` seeds).
    #[account(executable)]
    pub strategy_program: UncheckedAccount<'info>,
    /// CHECK: must be owned by the strategy program; checked in `fetch_market_info`.
    pub market_state: UncheckedAccount<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + Market::INIT_SPACE,
        seeds = [seeds::MARKET, strategy_program.key().as_ref(), market_state.key().as_ref()],
        bump,
    )]
    pub market: Account<'info, Market>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct InitFeeAccount<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub market: Account<'info, Market>,
    /// CHECK: PDA that owns the market's fee accounts.
    #[account(seeds = [seeds::FEE_AUTHORITY, market.key().as_ref()], bump = market.fee_authority_bump)]
    pub fee_authority: UncheckedAccount<'info>,
    /// CHECK: one of the market's mints; checked in the handler.
    pub mint: UncheckedAccount<'info>,
    /// CHECK: created by the associated token program, which checks its address.
    #[account(mut)]
    pub fee_account: UncheckedAccount<'info>,
    /// CHECK: SPL Token or Token-2022.
    #[account(constraint = token::is_token_program(token_program.key) @ PipelineError::WrongTokenProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the associated token account program.
    #[account(address = token::ASSOCIATED_TOKEN_PROGRAM_ID)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct Swap<'info> {
    pub trader: Signer<'info>,
    #[account(seeds = [seeds::CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,
    /// CHECK: the market's strategy program (enforced by the `market` seeds).
    #[account(executable)]
    pub strategy_program: UncheckedAccount<'info>,
    #[account(
        seeds = [seeds::MARKET, strategy_program.key().as_ref(), market_state.key().as_ref()],
        bump = market.bump,
    )]
    pub market: Account<'info, Market>,
    /// CHECK: the market's state, owned by the strategy (enforced by the `market` seeds).
    #[account(mut)]
    pub market_state: UncheckedAccount<'info>,
    /// CHECK: token account in the input mint; parsed and checked in the handler.
    #[account(mut)]
    pub trader_input: UncheckedAccount<'info>,
    /// CHECK: token account in the output mint; parsed and checked in the handler.
    #[account(mut)]
    pub trader_output: UncheckedAccount<'info>,
    /// CHECK: must equal the market's vault for the input mint.
    #[account(mut)]
    pub input_vault: UncheckedAccount<'info>,
    /// CHECK: one of the market's mints; owner checked against its token program.
    pub input_mint: UncheckedAccount<'info>,
    /// CHECK: one of the market's mints; owner checked against its token program.
    pub output_mint: UncheckedAccount<'info>,
    /// CHECK: SPL Token or Token-2022.
    #[account(constraint = token::is_token_program(input_token_program.key) @ PipelineError::WrongTokenProgram)]
    pub input_token_program: UncheckedAccount<'info>,
    /// CHECK: SPL Token or Token-2022.
    #[account(constraint = token::is_token_program(output_token_program.key) @ PipelineError::WrongTokenProgram)]
    pub output_token_program: UncheckedAccount<'info>,
    /// CHECK: PDA that owns the market's fee accounts.
    #[account(seeds = [seeds::FEE_AUTHORITY, market.key().as_ref()], bump = market.fee_authority_bump)]
    pub fee_authority: UncheckedAccount<'info>,
    /// CHECK: the fee authority's associated token account for the input mint.
    #[account(mut)]
    pub fee_account: UncheckedAccount<'info>,
    /// CHECK: any token account in the input mint; the token program checks the mint.
    #[account(mut)]
    pub integrator_fee_account: Option<UncheckedAccount<'info>>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct SweepFees<'info> {
    #[account(seeds = [seeds::CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,
    pub market: Account<'info, Market>,
    /// CHECK: PDA that owns the market's fee accounts.
    #[account(seeds = [seeds::FEE_AUTHORITY, market.key().as_ref()], bump = market.fee_authority_bump)]
    pub fee_authority: UncheckedAccount<'info>,
    /// CHECK: one of the market's mints; checked in the handler.
    pub mint: UncheckedAccount<'info>,
    /// CHECK: checked against the derived fee account address.
    #[account(mut)]
    pub fee_account: UncheckedAccount<'info>,
    /// CHECK: checked against the treasury owner's associated token account.
    #[account(mut)]
    pub treasury_account: UncheckedAccount<'info>,
    /// CHECK: SPL Token or Token-2022.
    #[account(constraint = token::is_token_program(token_program.key) @ PipelineError::WrongTokenProgram)]
    pub token_program: UncheckedAccount<'info>,
}

// ---------------------------------------------------------------------------------------
// Events and errors
// ---------------------------------------------------------------------------------------

#[event]
pub struct SwapEvent {
    pub market: Pubkey,
    pub strategy: Pubkey,
    pub trader: Pubkey,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    /// 0 = exact-in, 1 = exact-out.
    pub mode: u8,
    /// Input the swap is charged for, excluding fees: the trader's net spend, or what
    /// the market's input vault kept if that is more, at most `amount_in`.
    pub net_sent: u64,
    /// Output that arrived in the trader's output account.
    pub received: u64,
    pub pipeline_fee: u64,
    pub integrator: Option<Pubkey>,
    pub integrator_fee: u64,
    pub slot: u64,
    pub unix_timestamp: i64,
}

#[event]
pub struct StrategyRegistered {
    pub strategy: Pubkey,
    pub fee_paid: u64,
    pub metadata_uri: String,
}

#[event]
pub struct MetadataUpdated {
    pub strategy: Pubkey,
    pub metadata_uri: String,
}

#[event]
pub struct MarketCreated {
    pub market: Pubkey,
    pub strategy: Pubkey,
    pub market_state: Pubkey,
    pub mints: Vec<Pubkey>,
    pub created_by: Pubkey,
}

#[event]
pub struct FeesSwept {
    pub market: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
}

#[event]
pub struct ConfigChanged {
    pub admin: Pubkey,
    pub paused: bool,
    pub listing_fee_lamports: u64,
    pub listing_fee_recipient: Pubkey,
    pub treasury_owner: Pubkey,
}

#[error_code]
pub enum PipelineError {
    #[msg("Pipeline is paused")]
    Paused,
    #[msg("Signer is not the admin")]
    NotAdmin,
    #[msg("Listing fee exceeds the hard maximum")]
    ListingFeeTooHigh,
    #[msg("Unsupported strategy interface version")]
    UnsupportedInterfaceVersion,
    #[msg("Metadata URI is too long")]
    MetadataUriTooLong,
    #[msg("Strategy program must use the upgradeable loader")]
    UnsupportedLoader,
    #[msg("ProgramData account does not match the strategy program")]
    WrongProgramData,
    #[msg("Signer is neither the program's upgrade authority nor its registration PDA")]
    NotStrategyOwner,
    #[msg("Listing fee recipient does not match the config")]
    WrongFeeRecipient,
    #[msg("Market state is not owned by the strategy program")]
    MarketStateNotOwnedByStrategy,
    #[msg("Strategy returned invalid market info")]
    BadMarketInfo,
    #[msg("Mint is not part of this market")]
    MintNotInMarket,
    #[msg("Input and output mints are the same")]
    SameMint,
    #[msg("Input vault does not match the market")]
    WrongVault,
    #[msg("Mint is not owned by the given token program")]
    WrongTokenProgram,
    #[msg("Fee account is not the market's fee account for this mint")]
    WrongFeeAccount,
    #[msg("Treasury account is not the treasury owner's account for this mint")]
    WrongTreasuryAccount,
    #[msg("This market does not support exact-output swaps")]
    ExactOutUnsupported,
    #[msg("An integrator fee needs an integrator fee account")]
    MissingIntegratorFeeAccount,
    #[msg("More hook accounts declared than remaining accounts passed")]
    BadHookAccounts,
    #[msg("Not a valid token account for this mint and program")]
    BadTokenAccount,
    #[msg("Trader received less than the minimum")]
    InsufficientOutput,
    #[msg("Trader parted with more input than the swap's amount_in")]
    SpendLimitExceeded,
    #[msg("Strategy delivered output without keeping any input")]
    OutputWithoutInput,
    #[msg("Strategy setup changed the setup payer's owner or data")]
    SetupPayerMisused,
    #[msg("Arithmetic overflow")]
    MathOverflow,
    #[msg("The router is frozen, so it can no longer be paused")]
    FrozenRouterCannotPause,
    #[msg("Only a frozen router's pause can be lifted by anyone")]
    RouterNotFrozen,
    #[msg("The listing fee is above the maximum the registration accepts")]
    ListingFeeAboveMaximum,
    #[msg("A fee account can't be the trader's input account")]
    FeeToTraderInput,
    #[msg("Nothing was sent and nothing received")]
    NothingSwapped,
    #[msg("A non-zero listing fee must be at least the rent-exempt minimum")]
    ListingFeeBelowRent,
}
