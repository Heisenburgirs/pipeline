//! Quotes any Pipeline strategy by running its deployed bytecode in an embedded,
//! sandboxed Solana VM, from account data the caller supplies.
//!
//! The quoter never talks to the network. Callers keep it fed with account data
//! (from RPC, WebSocket or Geyser streams) via [`Quoter::set_account`], and with the
//! expected execution time via [`Quoter::set_clock`].
//!
//! Two ways to quote:
//! - [`Quoter::quote`] runs only the strategy's `pipeline_quote` and applies Pipeline's
//!   fee, the integrator fee and Token-2022 transfer fees on top. Fast; suitable for
//!   route searching.
//! - [`Quoter::simulate_swap`] simulates the full router `swap` with a synthetic trader
//!   holding enough tokens, and reads the router's own result. Slower, but exact for
//!   any token behaviour.
//!
//! Strategy code runs with a compute cap ([`Quoter::compute_limit`], default 200,000
//! units), so a slow or looping strategy produces an error rather than hanging.

use std::collections::HashSet;

use borsh::BorshDeserialize;
use litesvm::LiteSVM;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_strategy_interface::{self as psi, discriminator, fees, TradeMode};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;
use spl_token_2022_interface::extension::transfer_fee::TransferFeeConfig;
use spl_token_2022_interface::extension::{BaseStateWithExtensions, StateWithExtensions};

const COMPUTE_BUDGET_PROGRAM_ID: Pubkey =
    client::pubkey!("ComputeBudget111111111111111111111111111111");
pub const DEFAULT_COMPUTE_LIMIT: u32 = 200_000;
const SYSVAR_OWNER: Pubkey = client::pubkey!("Sysvar1111111111111111111111111111111111111");

#[derive(Debug, thiserror::Error)]
pub enum QuoteError {
    #[error("strategy program {0} has not been loaded")]
    ProgramNotLoaded(Pubkey),
    #[error("account {0} is missing from the quoter's cache")]
    MissingAccount(Pubkey),
    #[error("strategy failed: {0}")]
    StrategyFailed(String),
    #[error("strategy returned no or malformed return data")]
    BadReturnData,
    #[error("account resolution did not finish within {0} rounds")]
    ResolveIncomplete(u8),
    #[error("arithmetic overflow")]
    Overflow,
    #[error("fetching accounts failed: {0}")]
    Fetch(String),
    #[error("{0}")]
    Invalid(String),
}

/// The parts of a Pipeline market a quote needs. Build it from the router's `Market`
/// account with [`MarketView::from_market`].
#[derive(Clone, Debug)]
pub struct MarketView {
    pub market: Pubkey,
    pub strategy_program: Pubkey,
    pub market_state: Pubkey,
    pub mints: Vec<Pubkey>,
    pub vaults: Vec<Pubkey>,
    pub token_programs: Vec<Pubkey>,
    pub supports_exact_out: bool,
}

impl MarketView {
    /// `token_programs` must give each mint's owning token program, in mint order.
    pub fn from_market(
        market_address: Pubkey,
        market: &client::Market,
        token_programs: Vec<Pubkey>,
    ) -> Result<Self, QuoteError> {
        if token_programs.len() != market.mint_count as usize {
            return Err(QuoteError::Invalid(format!(
                "expected {} token programs, got {}",
                market.mint_count,
                token_programs.len()
            )));
        }
        Ok(MarketView {
            market: market_address,
            strategy_program: market.strategy_program,
            market_state: market.market_state,
            mints: market.mints().to_vec(),
            vaults: market.vaults[..market.mint_count as usize].to_vec(),
            token_programs,
            supports_exact_out: market.supports_exact_out,
        })
    }

    fn index(&self, mint: &Pubkey) -> Result<usize, QuoteError> {
        self.mints
            .iter()
            .position(|m| m == mint)
            .ok_or_else(|| QuoteError::Invalid(format!("mint {mint} is not in this market")))
    }
}

/// A trade as the trader sees it.
#[derive(Clone, Debug)]
pub struct QuoteRequest {
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub mode: TradeMode,
    /// Exact-in: the amount the trader sends to the strategy (fees come on top).
    /// Exact-out: the amount the trader wants to receive.
    pub amount: u64,
    pub trader: Option<Pubkey>,
    pub integrator_fee_bps: u16,
    pub extra: Vec<u8>,
}

/// A quote in the trader's terms. All amounts are in smallest token units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraderQuote {
    /// What the router transfers to the strategy (`SwapArgs::amount_in`).
    pub amount_in: u64,
    /// What the trader parts with before fees (the router's `net_sent`).
    pub net_sent: u64,
    pub pipeline_fee: u64,
    pub integrator_fee: u64,
    /// `net_sent` plus both fees.
    pub total_spend: u64,
    /// What arrives in the trader's output account.
    pub amount_out: u64,
    /// Exact-out only: what the strategy sends (`SwapArgs::amount_out`).
    pub strategy_amount_out: u64,
    /// Compute units the strategy's quote used.
    pub compute_units: u64,
}

pub struct Quoter {
    svm: LiteSVM,
    payer: Keypair,
    programs: HashSet<Pubkey>,
    compute_limit: u32,
}

impl Default for Quoter {
    fn default() -> Self {
        Self::new()
    }
}

impl Quoter {
    pub fn new() -> Self {
        let mut svm = LiteSVM::new()
            .with_sigverify(false)
            .with_blockhash_check(false);
        let payer = Keypair::new();
        svm.airdrop(&payer.pubkey(), 1_000_000_000_000)
            .expect("airdrop");
        Quoter {
            svm,
            payer,
            programs: HashSet::new(),
            compute_limit: DEFAULT_COMPUTE_LIMIT,
        }
    }

    pub fn compute_limit(&self) -> u32 {
        self.compute_limit
    }

    pub fn set_compute_limit(&mut self, limit: u32) {
        self.compute_limit = limit;
    }

    /// Loads (or reloads, after an upgrade) a program's ELF.
    pub fn load_program(&mut self, program_id: Pubkey, elf: &[u8]) -> Result<(), QuoteError> {
        self.svm
            .add_program(program_id, elf)
            .map_err(|e| QuoteError::Invalid(format!("program load failed: {e}")))?;
        self.programs.insert(program_id);
        Ok(())
    }

    /// Loads a program from a loader-v3 ProgramData account's data (as fetched from
    /// chain), skipping its 45-byte header.
    pub fn load_program_from_program_data(
        &mut self,
        program_id: Pubkey,
        program_data: &[u8],
    ) -> Result<(), QuoteError> {
        let elf = program_data
            .get(45..)
            .ok_or_else(|| QuoteError::Invalid("program data too short".into()))?;
        self.load_program(program_id, elf)
    }

    /// Caches an account's data. Programs must be loaded with [`Quoter::load_program`]
    /// and sysvars are managed by the quoter, so executable and sysvar accounts are
    /// refused rather than risk corrupting the quoter's state.
    pub fn set_account(&mut self, address: Pubkey, account: Account) -> Result<(), QuoteError> {
        if account.executable || account.owner == SYSVAR_OWNER {
            return Err(QuoteError::Invalid(format!(
                "refusing to cache program or sysvar account {address}"
            )));
        }
        self.svm
            .set_account(address, account)
            .map_err(|e| QuoteError::Invalid(format!("cannot cache {address}: {e}")))
    }

    /// Removes an account cached with [`Quoter::set_account`], so long-running callers can
    /// drop data they no longer need.
    pub fn remove_account(&mut self, address: &Pubkey) {
        // A zero-lamport account is deleted from the VM's store.
        self.svm
            .set_account(*address, Account::default())
            .expect("removing an account cannot fail");
    }

    pub fn account(&self, address: &Pubkey) -> Option<Account> {
        self.svm.get_account(address)
    }

    /// Sets the slot, epoch and timestamp the strategy will see. The epoch also selects
    /// which Token-2022 transfer fee applies. A new quoter starts at epoch 0; call this
    /// with the cluster's current clock before quoting.
    pub fn set_clock(&mut self, slot: u64, epoch: u64, unix_timestamp: i64) {
        let mut clock: solana_clock::Clock = self.svm.get_sysvar();
        clock.slot = slot;
        clock.epoch = epoch;
        clock.unix_timestamp = unix_timestamp;
        self.svm.set_sysvar(&clock);
    }

    /// The fixed trade accounts for a read-only strategy call. Trader accounts are the
    /// trader's associated token accounts (they need not exist).
    fn read_accounts(
        &self,
        market: &MarketView,
        input: usize,
        output: usize,
        trader: &Pubkey,
    ) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new_readonly(market.market, false),
            AccountMeta::new_readonly(market.market_state, false),
            AccountMeta::new_readonly(*trader, false),
            AccountMeta::new_readonly(
                pda::associated_token(trader, &market.mints[input], &market.token_programs[input]),
                false,
            ),
            AccountMeta::new_readonly(
                pda::associated_token(
                    trader,
                    &market.mints[output],
                    &market.token_programs[output],
                ),
                false,
            ),
            AccountMeta::new_readonly(market.vaults[input], false),
            AccountMeta::new_readonly(market.mints[input], false),
            AccountMeta::new_readonly(market.mints[output], false),
            AccountMeta::new_readonly(market.token_programs[input], false),
            AccountMeta::new_readonly(market.token_programs[output], false),
        ]
    }

    fn compute_budget_instruction(&self) -> Instruction {
        let mut limit = vec![2u8];
        limit.extend_from_slice(&self.compute_limit.to_le_bytes());
        Instruction::new_with_bytes(COMPUTE_BUDGET_PROGRAM_ID, &limit, vec![])
    }

    /// Runs one instruction in a simulated transaction and returns (return data, CU).
    fn run(
        &mut self,
        instruction: Instruction,
        expect_program: &Pubkey,
    ) -> Result<(Vec<u8>, u64), QuoteError> {
        let budget = self.compute_budget_instruction();
        let tx = Transaction::new_signed_with_payer(
            &[budget, instruction],
            Some(&self.payer.pubkey()),
            &[&self.payer],
            self.svm.latest_blockhash(),
        );
        match self.svm.simulate_transaction(tx) {
            Ok(info) => {
                let data = info.meta.return_data;
                if data.program_id != *expect_program {
                    return Err(QuoteError::BadReturnData);
                }
                Ok((data.data, info.meta.compute_units_consumed))
            }
            Err(failed) => Err(QuoteError::StrategyFailed(format!(
                "{:?}: {}",
                failed.err,
                failed.meta.logs.last().cloned().unwrap_or_default()
            ))),
        }
    }

    fn trade_request(
        market: &MarketView,
        request: &QuoteRequest,
        amount: u64,
    ) -> Result<psi::TradeRequest, QuoteError> {
        Ok(psi::TradeRequest {
            mode: request.mode,
            input_index: market.index(&request.input_mint)? as u8,
            output_index: market.index(&request.output_mint)? as u8,
            amount,
            trader: request.trader.map(|t| t.to_bytes()),
            extra: request.extra.clone(),
        })
    }

    /// Asks the strategy which accounts the trade needs, up to
    /// [`psi::MAX_RESOLVE_ROUNDS`] rounds. Before each later round, `fetch` is called
    /// with the accounts returned so far so the caller can load them into the quoter.
    pub fn resolve_accounts(
        &mut self,
        market: &MarketView,
        request: &QuoteRequest,
        mut fetch: impl FnMut(&mut Quoter, &[Pubkey]),
    ) -> Result<Vec<AccountMeta>, QuoteError> {
        self.try_resolve_accounts(market, request, |quoter, keys| {
            fetch(quoter, keys);
            Ok(())
        })
    }

    /// [`Quoter::resolve_accounts`] with a `fetch` that can fail, for example when the
    /// strategy asks for more account data than the caller will load. The first error
    /// stops resolution and is returned.
    pub fn try_resolve_accounts(
        &mut self,
        market: &MarketView,
        request: &QuoteRequest,
        mut fetch: impl FnMut(&mut Quoter, &[Pubkey]) -> Result<(), QuoteError>,
    ) -> Result<Vec<AccountMeta>, QuoteError> {
        self.ensure_loaded(&market.strategy_program)?;
        let trade = Self::trade_request(market, request, request.amount)?;
        let trader = request.trader.unwrap_or_default();
        let mut known: Vec<AccountMeta> = vec![];
        for round in 0..psi::MAX_RESOLVE_ROUNDS {
            let mut accounts = self.read_accounts(
                market,
                trade.input_index as usize,
                trade.output_index as usize,
                &trader,
            );
            accounts.extend(
                known
                    .iter()
                    .map(|m| AccountMeta::new_readonly(m.pubkey, false)),
            );
            let data = psi::instruction_data(
                discriminator::RESOLVE_ACCOUNTS,
                &psi::ResolveRequest {
                    trade: trade.clone(),
                    round,
                },
            );
            let (output, _) = self.run(
                Instruction::new_with_bytes(market.strategy_program, &data, accounts),
                &market.strategy_program,
            )?;
            let result = psi::ResolveResult::try_from_slice(&output)
                .map_err(|_| QuoteError::BadReturnData)?;
            known = result
                .accounts
                .iter()
                .map(|a| {
                    let key = Pubkey::new_from_array(a.address);
                    if a.writable {
                        AccountMeta::new(key, false)
                    } else {
                        AccountMeta::new_readonly(key, false)
                    }
                })
                .collect();
            if result.complete {
                return Ok(known);
            }
            let keys: Vec<Pubkey> = known.iter().map(|m| m.pubkey).collect();
            fetch(self, &keys)?;
        }
        Err(QuoteError::ResolveIncomplete(psi::MAX_RESOLVE_ROUNDS))
    }

    /// Runs the strategy's `pipeline_quote` with vault-level amounts.
    pub fn quote_strategy(
        &mut self,
        market: &MarketView,
        trade: &psi::TradeRequest,
        strategy_accounts: &[AccountMeta],
    ) -> Result<(psi::QuoteResult, u64), QuoteError> {
        self.ensure_loaded(&market.strategy_program)?;
        let trader = trade.trader.map(Pubkey::new_from_array).unwrap_or_default();
        let mut accounts = self.read_accounts(
            market,
            trade.input_index as usize,
            trade.output_index as usize,
            &trader,
        );
        accounts.extend(
            strategy_accounts
                .iter()
                .map(|m| AccountMeta::new_readonly(m.pubkey, false)),
        );
        let data = psi::instruction_data(discriminator::QUOTE, trade);
        let (output, units) = self.run(
            Instruction::new_with_bytes(market.strategy_program, &data, accounts),
            &market.strategy_program,
        )?;
        let quote =
            psi::QuoteResult::try_from_slice(&output).map_err(|_| QuoteError::BadReturnData)?;
        Ok((quote, units))
    }

    /// Quotes a trade in the trader's terms: runs the strategy's quote and applies
    /// Token-2022 transfer fees, Pipeline's fee and the integrator fee.
    pub fn quote(
        &mut self,
        market: &MarketView,
        request: &QuoteRequest,
        strategy_accounts: &[AccountMeta],
    ) -> Result<TraderQuote, QuoteError> {
        let input = market.index(&request.input_mint)?;
        let output = market.index(&request.output_mint)?;
        let epoch = self.svm.get_sysvar::<solana_clock::Clock>().epoch;
        let input_fee = self.transfer_fee_config(&market.mints[input])?;
        let output_fee = self.transfer_fee_config(&market.mints[output])?;

        let (amount_in, strategy_amount_out, quote, units) = match request.mode {
            TradeMode::ExactIn => {
                let arrives = request.amount - transfer_fee(&input_fee, epoch, request.amount)?;
                let trade = Self::trade_request(market, request, arrives)?;
                let (quote, units) = self.quote_strategy(market, &trade, strategy_accounts)?;
                (request.amount, quote.amount_out, quote, units)
            }
            TradeMode::ExactOut => {
                if !market.supports_exact_out {
                    return Err(QuoteError::Invalid(
                        "market does not support exact-out".into(),
                    ));
                }
                let send = gross_for_net(&output_fee, epoch, request.amount)?;
                let trade = Self::trade_request(market, request, send)?;
                let (quote, units) = self.quote_strategy(market, &trade, strategy_accounts)?;
                let amount_in = gross_for_net(&input_fee, epoch, quote.amount_in)?;
                (amount_in, send, quote, units)
            }
        };

        // Any input the strategy does not keep is refunded, losing the transfer fee again.
        let arrives = amount_in - transfer_fee(&input_fee, epoch, amount_in)?;
        let refund = arrives.saturating_sub(quote.amount_in);
        let refund_arrives = refund - transfer_fee(&input_fee, epoch, refund)?;
        let net_sent = amount_in - refund_arrives;

        let pipeline_fee = fees::pipeline_fee(net_sent);
        let integrator_fee = fees::ceil_bps(net_sent, request.integrator_fee_bps as u64)
            .ok_or(QuoteError::Overflow)?;
        let amount_out = quote.amount_out - transfer_fee(&output_fee, epoch, quote.amount_out)?;
        Ok(TraderQuote {
            amount_in,
            net_sent,
            pipeline_fee,
            integrator_fee,
            total_spend: net_sent
                .checked_add(pipeline_fee)
                .and_then(|t| t.checked_add(integrator_fee))
                .ok_or(QuoteError::Overflow)?,
            amount_out,
            strategy_amount_out,
            compute_units: units,
        })
    }

    /// Simulates the full router swap for a synthetic trader funded with `amount_in`
    /// plus fees, and returns the router's own result. Needs the router program, its
    /// config, the market, its fee account and every account the swap touches.
    pub fn simulate_swap(
        &mut self,
        accounts: &client::SwapAccounts,
        args: &client::SwapArgs,
    ) -> Result<client::SwapResult, QuoteError> {
        let budget = fees::max_total_spend(args.amount_in, args.integrator_fee_bps)
            .ok_or(QuoteError::Overflow)?;
        let trader = Keypair::new();
        let input = self.synthetic_token_account(
            &accounts.input_mint,
            &accounts.input_token_program,
            &trader.pubkey(),
            budget,
        )?;
        let output = match self.synthetic_token_account(
            &accounts.output_mint,
            &accounts.output_token_program,
            &trader.pubkey(),
            0,
        ) {
            Ok(output) => output,
            Err(error) => {
                self.remove_account(&input);
                return Err(error);
            }
        };
        let mut accounts = accounts.clone();
        accounts.trader = trader.pubkey();
        accounts.trader_input = input;
        accounts.trader_output = output;

        let tx = Transaction::new_signed_with_payer(
            &[
                self.compute_budget_instruction(),
                client::swap(&accounts, args),
            ],
            Some(&self.payer.pubkey()),
            &[&self.payer, &trader],
            self.svm.latest_blockhash(),
        );
        let outcome = self.svm.simulate_transaction(tx);

        // Remove the synthetic accounts so a long-running quoter does not grow.
        for address in [input, output] {
            self.remove_account(&address);
        }

        match outcome {
            Ok(info) => {
                if info.meta.return_data.program_id != client::PROGRAM_ID {
                    return Err(QuoteError::BadReturnData);
                }
                client::SwapResult::try_from_slice(&info.meta.return_data.data)
                    .map_err(|_| QuoteError::BadReturnData)
            }
            Err(failed) => Err(QuoteError::StrategyFailed(format!(
                "{:?}: {}",
                failed.err,
                failed.meta.logs.last().cloned().unwrap_or_default()
            ))),
        }
    }

    /// Creates a token account in the quoter's state only, by cloning the layout of an
    /// account the token program would create. Supports SPL Token and Token-2022.
    fn synthetic_token_account(
        &mut self,
        mint: &Pubkey,
        token_program: &Pubkey,
        owner: &Pubkey,
        amount: u64,
    ) -> Result<Pubkey, QuoteError> {
        let address = pda::associated_token(owner, mint, token_program);
        let mint_account = self
            .svm
            .get_account(mint)
            .ok_or(QuoteError::MissingAccount(*mint))?;
        let mut data = if *token_program == client::TOKEN_2022_PROGRAM_ID {
            token_2022_account_data(&mint_account.data)?
        } else {
            vec![0u8; 165]
        };
        data[..32].copy_from_slice(mint.as_ref());
        data[32..64].copy_from_slice(owner.as_ref());
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        data[108] = 1; // AccountState::Initialized
        self.set_account(
            address,
            Account {
                lamports: self.svm.minimum_balance_for_rent_exemption(data.len()),
                data,
                owner: *token_program,
                executable: false,
                rent_epoch: 0,
            },
        )?;
        Ok(address)
    }

    fn ensure_loaded(&self, program: &Pubkey) -> Result<(), QuoteError> {
        if self.programs.contains(program) {
            Ok(())
        } else {
            Err(QuoteError::ProgramNotLoaded(*program))
        }
    }

    fn transfer_fee_config(&self, mint: &Pubkey) -> Result<Option<TransferFeeConfig>, QuoteError> {
        let account = self
            .svm
            .get_account(mint)
            .ok_or(QuoteError::MissingAccount(*mint))?;
        if account.owner != client::TOKEN_2022_PROGRAM_ID {
            return Ok(None);
        }
        let state =
            StateWithExtensions::<spl_token_2022_interface::state::Mint>::unpack(&account.data)
                .map_err(|e| QuoteError::Invalid(format!("bad mint {mint}: {e}")))?;
        Ok(state.get_extension::<TransferFeeConfig>().ok().copied())
    }
}

fn transfer_fee(
    config: &Option<TransferFeeConfig>,
    epoch: u64,
    amount: u64,
) -> Result<u64, QuoteError> {
    match config {
        Some(config) => config
            .calculate_epoch_fee(epoch, amount)
            .ok_or(QuoteError::Overflow),
        None => Ok(0),
    }
}

/// The smallest gross amount whose transfer delivers at least `net`.
fn gross_for_net(
    config: &Option<TransferFeeConfig>,
    epoch: u64,
    net: u64,
) -> Result<u64, QuoteError> {
    let Some(config) = config else {
        return Ok(net);
    };
    let fee = config
        .get_epoch_fee(epoch)
        .calculate_inverse_fee(net)
        .ok_or(QuoteError::Overflow)?;
    let mut gross = net.checked_add(fee).ok_or(QuoteError::Overflow)?;
    // Step down while a smaller gross still delivers `net`.
    while gross > net && gross - 1 - transfer_fee(&Some(*config), epoch, gross - 1)? >= net {
        gross -= 1;
    }
    Ok(gross)
}

/// Token-2022 account data with the extensions a holder of `mint` must carry.
#[allow(deprecated)] // the replacement helper is not yet public in this interface version
fn token_2022_account_data(mint_data: &[u8]) -> Result<Vec<u8>, QuoteError> {
    use spl_token_2022_interface::extension::{BaseStateWithExtensionsMut, ExtensionType};
    use spl_token_2022_interface::state::{Account as TokenAccount, Mint};
    let mint = StateWithExtensions::<Mint>::unpack(mint_data)
        .map_err(|e| QuoteError::Invalid(format!("bad mint: {e}")))?;
    let mint_extensions = mint
        .get_extension_types()
        .map_err(|e| QuoteError::Invalid(e.to_string()))?;
    let account_extensions = ExtensionType::get_required_init_account_extensions(&mint_extensions);
    let len = ExtensionType::try_calculate_account_len::<TokenAccount>(&account_extensions)
        .map_err(|e| QuoteError::Invalid(e.to_string()))?;
    let mut data = vec![0u8; len];
    let mut state = spl_token_2022_interface::extension::StateWithExtensionsMut::<TokenAccount>::unpack_uninitialized(&mut data)
        .map_err(|e| QuoteError::Invalid(e.to_string()))?;
    for extension in account_extensions {
        state
            .init_account_extension_from_type(extension)
            .map_err(|e| QuoteError::Invalid(e.to_string()))?;
    }
    state
        .init_account_type()
        .map_err(|e| QuoteError::Invalid(e.to_string()))?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_accounts_are_gone() {
        let mut quoter = Quoter::new();
        let address = Pubkey::new_unique();
        let account = Account {
            lamports: 1_000_000,
            data: vec![7; 1024],
            owner: Pubkey::new_unique(),
            executable: false,
            rent_epoch: 0,
        };
        quoter.set_account(address, account.clone()).unwrap();
        assert_eq!(quoter.account(&address).unwrap().data, account.data);
        quoter.remove_account(&address);
        assert_eq!(quoter.account(&address), None);
    }
}
