//! Addresses, instruction builders and account decoders for the Pipeline router.
//!
//! Builders produce plain `solana_instruction::Instruction`s; callers add them to
//! transactions however they like. Nothing here talks to the network.

use borsh::{BorshDeserialize, BorshSerialize};
pub use pipeline_strategy_interface as interface;
use pipeline_strategy_interface::seeds;
pub use pipeline_strategy_interface::TradeMode;
use sha2::{Digest, Sha256};
pub use solana_instruction::{AccountMeta, Instruction};
pub use solana_pubkey::{pubkey, Pubkey};

pub mod wallet;

/// The Pipeline router program.
pub const PROGRAM_ID: Pubkey = Pubkey::new_from_array(interface::PIPELINE_PROGRAM_ID);
pub const SYSTEM_PROGRAM_ID: Pubkey = pubkey!("11111111111111111111111111111111");
pub const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_PROGRAM_ID: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const BPF_LOADER_UPGRADEABLE_ID: Pubkey =
    pubkey!("BPFLoaderUpgradeab1e11111111111111111111111");

/// Program-derived addresses used by the router.
pub mod pda {
    use super::*;

    pub fn config() -> Pubkey {
        Pubkey::find_program_address(&[seeds::CONFIG], &PROGRAM_ID).0
    }

    pub fn strategy(strategy_program: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[seeds::STRATEGY, strategy_program.as_ref()], &PROGRAM_ID).0
    }

    /// The market account, which also signs every router call into the strategy.
    pub fn market(strategy_program: &Pubkey, market_state: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[
                seeds::MARKET,
                strategy_program.as_ref(),
                market_state.as_ref(),
            ],
            &PROGRAM_ID,
        )
        .0
    }

    /// Temporary payer for a strategy's market setup, funded by the creator.
    pub fn setup_payer(market: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[seeds::SETUP_PAYER, market.as_ref()], &PROGRAM_ID).0
    }

    pub fn fee_authority(market: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[seeds::FEE_AUTHORITY, market.as_ref()], &PROGRAM_ID).0
    }

    /// The market's fee account for one mint.
    pub fn fee_account(market: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
        associated_token(&fee_authority(market), mint, token_program)
    }

    /// The PDA a strategy program signs with to register itself without an upgrade
    /// authority.
    pub fn register(strategy_program: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[seeds::REGISTER], strategy_program).0
    }

    pub fn event_authority() -> Pubkey {
        Pubkey::find_program_address(&[b"__event_authority"], &PROGRAM_ID).0
    }

    pub fn program_data(program: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[program.as_ref()], &BPF_LOADER_UPGRADEABLE_ID).0
    }

    pub fn associated_token(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
            &ASSOCIATED_TOKEN_PROGRAM_ID,
        )
        .0
    }
}

/// Anchor instruction discriminator: `sha256("global:<name>")[..8]`.
pub fn instruction_discriminator(name: &str) -> [u8; 8] {
    Sha256::digest(format!("global:{name}").as_bytes())[..8]
        .try_into()
        .unwrap()
}

/// Anchor account discriminator: `sha256("account:<Name>")[..8]`.
pub fn account_discriminator(name: &str) -> [u8; 8] {
    Sha256::digest(format!("account:{name}").as_bytes())[..8]
        .try_into()
        .unwrap()
}

fn anchor_data(name: &str, args: &impl BorshSerialize) -> Vec<u8> {
    let mut data = instruction_discriminator(name).to_vec();
    args.serialize(&mut data)
        .expect("serialising into a Vec cannot fail");
    data
}

/// The two accounts `#[event_cpi]` appends to an instruction.
fn event_cpi_accounts() -> [AccountMeta; 2] {
    [
        AccountMeta::new_readonly(pda::event_authority(), false),
        AccountMeta::new_readonly(PROGRAM_ID, false),
    ]
}

// ---------------------------------------------------------------------------------------
// Argument types (mirror the router)
// ---------------------------------------------------------------------------------------

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct ConfigArgs {
    pub listing_fee_lamports: u64,
    pub listing_fee_recipient: Pubkey,
    pub treasury_owner: Pubkey,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateConfigArgs {
    pub admin: Option<Pubkey>,
    pub paused: Option<bool>,
    pub listing_fee_lamports: Option<u64>,
    pub listing_fee_recipient: Option<Pubkey>,
    pub treasury_owner: Option<Pubkey>,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct SwapArgs {
    pub mode: TradeMode,
    /// Exact-in: amount sent to the strategy. Exact-out: the most that may be sent.
    pub amount_in: u64,
    /// Exact-out only: output the strategy must send, in strategy vault units.
    pub amount_out: u64,
    pub min_amount_out: u64,
    pub integrator_fee_bps: u16,
    pub input_hook_accounts: u8,
    pub extra: Vec<u8>,
}

/// Return data of `swap`.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct SwapResult {
    pub net_sent: u64,
    pub received: u64,
    pub pipeline_fee: u64,
    pub integrator_fee: u64,
}

// ---------------------------------------------------------------------------------------
// Accounts (mirror the router)
// ---------------------------------------------------------------------------------------

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub admin: Pubkey,
    pub paused: bool,
    pub listing_fee_lamports: u64,
    pub listing_fee_recipient: Pubkey,
    pub treasury_owner: Pubkey,
    pub bump: u8,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct Strategy {
    pub program_id: Pubkey,
    pub metadata_uri: String,
    pub interface_version: u8,
    pub registered_at: i64,
    pub market_count: u64,
    pub bump: u8,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct Market {
    pub strategy_program: Pubkey,
    pub market_state: Pubkey,
    pub mint_count: u8,
    pub mints: [Pubkey; interface::MAX_MARKET_MINTS],
    pub vaults: [Pubkey; interface::MAX_MARKET_MINTS],
    pub supports_exact_out: bool,
    pub created_by: Pubkey,
    pub created_at: i64,
    pub bump: u8,
    pub fee_authority_bump: u8,
}

impl Market {
    pub fn mints(&self) -> &[Pubkey] {
        &self.mints[..self.mint_count as usize]
    }

    pub fn vault_for(&self, mint: &Pubkey) -> Option<Pubkey> {
        self.mints()
            .iter()
            .position(|m| m == mint)
            .map(|i| self.vaults[i])
    }
}

/// Decodes a router account, checking its Anchor discriminator.
pub fn decode_account<T: BorshDeserialize>(name: &str, data: &[u8]) -> Option<T> {
    let body = data.strip_prefix(&account_discriminator(name))?;
    T::deserialize(&mut &body[..]).ok()
}

pub fn decode_config(data: &[u8]) -> Option<Config> {
    decode_account("Config", data)
}

pub fn decode_strategy(data: &[u8]) -> Option<Strategy> {
    decode_account("Strategy", data)
}

pub fn decode_market(data: &[u8]) -> Option<Market> {
    decode_account("Market", data)
}

// ---------------------------------------------------------------------------------------
// Instruction builders
// ---------------------------------------------------------------------------------------

/// `initialize_config`, signed by the router's upgrade authority.
pub fn initialize_config(admin: &Pubkey, args: &ConfigArgs) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &anchor_data("initialize_config", args),
        vec![
            AccountMeta::new(*admin, true),
            AccountMeta::new(pda::config(), false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(pda::program_data(&PROGRAM_ID), false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
    )
}

pub fn update_config(admin: &Pubkey, args: &UpdateConfigArgs) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new_readonly(*admin, true),
        AccountMeta::new(pda::config(), false),
    ];
    accounts.extend(event_cpi_accounts());
    Instruction::new_with_bytes(PROGRAM_ID, &anchor_data("update_config", args), accounts)
}

/// `register_strategy`. `registrar` is the program's upgrade authority or its
/// registration PDA.
pub fn register_strategy(
    payer: &Pubkey,
    registrar: &Pubkey,
    strategy_program: &Pubkey,
    listing_fee_recipient: &Pubkey,
    metadata_uri: &str,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new(*payer, true),
        AccountMeta::new_readonly(*registrar, true),
        AccountMeta::new_readonly(*strategy_program, false),
        AccountMeta::new_readonly(pda::program_data(strategy_program), false),
        AccountMeta::new_readonly(pda::config(), false),
        AccountMeta::new(*listing_fee_recipient, false),
        AccountMeta::new(pda::strategy(strategy_program), false),
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
    ];
    accounts.extend(event_cpi_accounts());
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &anchor_data(
            "register_strategy",
            &(metadata_uri.to_string(), interface::INTERFACE_VERSION),
        ),
        accounts,
    )
}

pub fn update_metadata(
    registrar: &Pubkey,
    strategy_program: &Pubkey,
    metadata_uri: &str,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new_readonly(*registrar, true),
        AccountMeta::new_readonly(*strategy_program, false),
        AccountMeta::new_readonly(pda::program_data(strategy_program), false),
        AccountMeta::new(pda::strategy(strategy_program), false),
    ];
    accounts.extend(event_cpi_accounts());
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &anchor_data("update_metadata", &metadata_uri.to_string()),
        accounts,
    )
}

/// Everything needed to build a `create_market` instruction.
#[derive(Clone, Debug)]
pub struct CreateMarket {
    pub creator: Pubkey,
    pub strategy_program: Pubkey,
    pub market_state: Pubkey,
    /// Set if the market state is a fresh keypair that signs the transaction. Only fresh
    /// (empty, unfunded) accounts keep their signature when passed to the strategy.
    pub market_state_signs: bool,
    /// Strategy-specific setup data, passed to `pipeline_init_market`.
    pub init_data: Vec<u8>,
    /// The most SOL the strategy's setup may spend (normally rent for the accounts it
    /// creates). The router lends exactly this to the setup and returns what is left.
    pub max_creator_lamports: u64,
    /// The strategy's own setup accounts, passed after the router's fixed accounts.
    pub strategy_accounts: Vec<AccountMeta>,
}

/// `create_market`.
pub fn create_market(params: &CreateMarket) -> Instruction {
    let market = pda::market(&params.strategy_program, &params.market_state);
    let mut accounts = vec![
        AccountMeta::new(params.creator, true),
        AccountMeta::new_readonly(pda::config(), false),
        AccountMeta::new(pda::strategy(&params.strategy_program), false),
        AccountMeta::new_readonly(params.strategy_program, false),
        AccountMeta::new(params.market_state, params.market_state_signs),
        AccountMeta::new(market, false),
        AccountMeta::new(pda::setup_payer(&market), false),
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
    ];
    accounts.extend(event_cpi_accounts());
    accounts.extend(params.strategy_accounts.iter().cloned());
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &anchor_data(
            "create_market",
            &(params.init_data.clone(), params.max_creator_lamports),
        ),
        accounts,
    )
}

/// `register_existing_market`. `info_accounts` are passed to the strategy's
/// `pipeline_market_info` after the market state.
pub fn register_existing_market(
    payer: &Pubkey,
    strategy_program: &Pubkey,
    market_state: &Pubkey,
    info_accounts: Vec<AccountMeta>,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new(*payer, true),
        AccountMeta::new_readonly(pda::config(), false),
        AccountMeta::new(pda::strategy(strategy_program), false),
        AccountMeta::new_readonly(*strategy_program, false),
        AccountMeta::new_readonly(*market_state, false),
        AccountMeta::new(pda::market(strategy_program, market_state), false),
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
    ];
    accounts.extend(event_cpi_accounts());
    accounts.extend(info_accounts);
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &instruction_discriminator("register_existing_market"),
        accounts,
    )
}

pub fn init_fee_account(
    payer: &Pubkey,
    market: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &instruction_discriminator("init_fee_account"),
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(*market, false),
            AccountMeta::new_readonly(pda::fee_authority(market), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(pda::fee_account(market, mint, token_program), false),
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
    )
}

/// Everything needed to build a `swap` instruction.
#[derive(Clone, Debug)]
pub struct SwapAccounts {
    pub trader: Pubkey,
    pub strategy_program: Pubkey,
    pub market_state: Pubkey,
    pub trader_input: Pubkey,
    pub trader_output: Pubkey,
    pub input_vault: Pubkey,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub input_token_program: Pubkey,
    pub output_token_program: Pubkey,
    pub integrator_fee_account: Option<Pubkey>,
    /// Strategy-specific accounts, as returned by `pipeline_resolve_accounts`.
    pub strategy_accounts: Vec<AccountMeta>,
    /// Transfer-hook accounts for the input mint (empty if it has no hook).
    pub input_hook_accounts: Vec<AccountMeta>,
}

/// `swap`. `args.input_hook_accounts` is set from `accounts.input_hook_accounts`.
pub fn swap(accounts: &SwapAccounts, args: &SwapArgs) -> Instruction {
    let market = pda::market(&accounts.strategy_program, &accounts.market_state);
    let mut metas = vec![
        AccountMeta::new_readonly(accounts.trader, true),
        AccountMeta::new_readonly(pda::config(), false),
        AccountMeta::new_readonly(accounts.strategy_program, false),
        AccountMeta::new_readonly(market, false),
        AccountMeta::new(accounts.market_state, false),
        AccountMeta::new(accounts.trader_input, false),
        AccountMeta::new(accounts.trader_output, false),
        AccountMeta::new(accounts.input_vault, false),
        AccountMeta::new_readonly(accounts.input_mint, false),
        AccountMeta::new_readonly(accounts.output_mint, false),
        AccountMeta::new_readonly(accounts.input_token_program, false),
        AccountMeta::new_readonly(accounts.output_token_program, false),
        AccountMeta::new_readonly(pda::fee_authority(&market), false),
        AccountMeta::new(
            pda::fee_account(&market, &accounts.input_mint, &accounts.input_token_program),
            false,
        ),
        // Anchor encodes a missing optional account as the program ID.
        match accounts.integrator_fee_account {
            Some(account) => AccountMeta::new(account, false),
            None => AccountMeta::new_readonly(PROGRAM_ID, false),
        },
    ];
    metas.extend(event_cpi_accounts());
    metas.extend(accounts.strategy_accounts.iter().cloned());
    metas.extend(accounts.input_hook_accounts.iter().cloned());

    let mut args = args.clone();
    args.input_hook_accounts = accounts.input_hook_accounts.len() as u8;
    Instruction::new_with_bytes(PROGRAM_ID, &anchor_data("swap", &args), metas)
}

/// `sweep_fees`, moving a market's fees in `mint` to the treasury owner's associated
/// token account (which must already exist).
pub fn sweep_fees(
    market: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
    treasury_owner: &Pubkey,
    hook_accounts: Vec<AccountMeta>,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new_readonly(pda::config(), false),
        AccountMeta::new_readonly(*market, false),
        AccountMeta::new_readonly(pda::fee_authority(market), false),
        AccountMeta::new_readonly(*mint, false),
        AccountMeta::new(pda::fee_account(market, mint, token_program), false),
        AccountMeta::new(
            pda::associated_token(treasury_owner, mint, token_program),
            false,
        ),
        AccountMeta::new_readonly(*token_program, false),
    ];
    accounts.extend(event_cpi_accounts());
    accounts.extend(hook_accounts);
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &instruction_discriminator("sweep_fees"),
        accounts,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_id_is_the_vanity_address() {
        assert_eq!(
            PROGRAM_ID,
            pubkey!("PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34")
        );
    }

    #[test]
    fn known_discriminators() {
        assert_eq!(
            instruction_discriminator("swap"),
            [248, 198, 158, 145, 225, 117, 135, 200]
        );
        assert_eq!(
            account_discriminator("Market"),
            [219, 190, 213, 55, 0, 227, 198, 154]
        );
    }
}
