//! Shared LiteSVM setup for router integration tests.
//!
//! Programs are loaded from `programs/target/deploy` (override with
//! `PIPELINE_SBF_DIR`), so build them first with `cargo build-sbf` in `programs/`.

#![allow(dead_code, clippy::result_large_err)]

use std::path::PathBuf;

use litesvm::types::{FailedTransactionMetadata, TransactionMetadata};
use litesvm::LiteSVM;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_events::PipelineEvent;
use pipeline_strategy_interface as psi;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

pub const LISTING_FEE: u64 = 10_000_000;
pub const SOL: u64 = 1_000_000_000;
/// Generous cap on the SOL a strategy's market setup may take from the creator.
pub const MAX_SETUP_LAMPORTS: u64 = SOL;

pub const CP_PROGRAM_ID: Pubkey = client::pubkey!("yRptCdde1du1JYR2DcNFtLC1fkp5Asd6rE3XSvZoUFx");

pub type TxResult = Result<TransactionMetadata, FailedTransactionMetadata>;

/// Solana's packet limit for a serialised transaction.
pub const MAX_TRANSACTION_SIZE: usize = 1_232;

pub fn sbf_dir() -> PathBuf {
    std::env::var("PIPELINE_SBF_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../programs/target/deploy")
        })
}

pub struct Env {
    pub svm: LiteSVM,
    /// Router upgrade authority and config admin.
    pub admin: Keypair,
    pub listing_fee_recipient: Pubkey,
    pub treasury_owner: Pubkey,
}

impl Env {
    /// A fresh SVM with the router deployed (upgrade authority = `admin`) and its
    /// config initialised.
    pub fn new() -> Self {
        let mut svm = LiteSVM::new();
        let admin = Keypair::new();
        svm.airdrop(&admin.pubkey(), 1_000 * SOL).unwrap();
        svm.add_program_from_file(client::PROGRAM_ID, sbf_dir().join("pipeline.so"))
            .expect("build programs first: cd programs && cargo build-sbf");
        set_upgrade_authority(&mut svm, &client::PROGRAM_ID, Some(&admin.pubkey()));
        add_native_mint(&mut svm);

        let mut env = Env {
            svm,
            admin,
            listing_fee_recipient: Pubkey::new_unique(),
            treasury_owner: Pubkey::new_unique(),
        };
        let ix = client::initialize_config(
            &env.admin.pubkey(),
            &client::ConfigArgs {
                listing_fee_lamports: LISTING_FEE,
                listing_fee_recipient: env.listing_fee_recipient,
                treasury_owner: env.treasury_owner,
            },
        );
        env.send(&[ix], &[]).expect("initialize_config");
        env
    }

    pub fn funded_keypair(&mut self) -> Keypair {
        let keypair = Keypair::new();
        self.svm.airdrop(&keypair.pubkey(), 100 * SOL).unwrap();
        keypair
    }

    /// Sends a transaction paid for by the admin, with extra signers.
    pub fn send(&mut self, instructions: &[Instruction], signers: &[&Keypair]) -> TxResult {
        self.svm.expire_blockhash();
        let mut all: Vec<&Keypair> = vec![&self.admin];
        all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(
            instructions,
            Some(&self.admin.pubkey()),
            &all,
            self.svm.latest_blockhash(),
        );
        // LiteSVM accepts oversized transactions; real validators don't.
        let size = bincode::serialize(&tx)
            .map(|b| b.len())
            .unwrap_or(usize::MAX);
        assert!(
            size <= MAX_TRANSACTION_SIZE,
            "transaction is {size} bytes, over Solana's {MAX_TRANSACTION_SIZE}-byte limit"
        );
        self.svm.send_transaction(tx)
    }

    /// Deploys a strategy program with `owner` as its upgrade authority.
    pub fn deploy_strategy(&mut self, program_id: &Pubkey, so_name: &str, owner: &Pubkey) {
        self.svm
            .add_program_from_file(*program_id, sbf_dir().join(format!("{so_name}.so")))
            .expect("strategy program");
        set_upgrade_authority(&mut self.svm, program_id, Some(owner));
    }

    /// Deploys and registers a strategy, returning its owner.
    pub fn register(&mut self, program_id: &Pubkey, so_name: &str) -> Keypair {
        let owner = self.funded_keypair();
        self.deploy_strategy(program_id, so_name, &owner.pubkey());
        let ix = client::register_strategy(
            &owner.pubkey(),
            &owner.pubkey(),
            program_id,
            &self.listing_fee_recipient,
            "https://example.com/strategy.json",
            u64::MAX,
        );
        self.send(&[ix], &[&owner]).expect("register_strategy");
        owner
    }

    pub fn account_data(&self, address: &Pubkey) -> Vec<u8> {
        self.svm
            .get_account(address)
            .map(|a| a.data)
            .unwrap_or_default()
    }

    pub fn lamports(&self, address: &Pubkey) -> u64 {
        self.svm
            .get_account(address)
            .map(|a| a.lamports)
            .unwrap_or(0)
    }

    pub fn token_balance(&self, account: &Pubkey) -> u64 {
        let data = self.account_data(account);
        if data.len() < 72 {
            return 0;
        }
        u64::from_le_bytes(data[64..72].try_into().unwrap())
    }

    pub fn market(&self, market: &Pubkey) -> client::Market {
        client::decode_market(&self.account_data(market)).expect("market account")
    }

    // ---------------------------------------------------------------- tokens

    /// Creates a mint with `decimals`, optionally with a Token-2022 transfer fee
    /// (`(bps, max)`) and/or transfer hook program.
    pub fn create_mint(
        &mut self,
        token_program: &Pubkey,
        decimals: u8,
        transfer_fee: Option<(u16, u64)>,
        transfer_hook: Option<Pubkey>,
    ) -> Pubkey {
        use spl_token_2022_interface::extension::ExtensionType;
        let mint = Keypair::new();
        let mut extensions = vec![];
        if transfer_fee.is_some() {
            extensions.push(ExtensionType::TransferFeeConfig);
        }
        if transfer_hook.is_some() {
            extensions.push(ExtensionType::TransferHook);
        }
        let space = if *token_program == client::TOKEN_PROGRAM_ID {
            82
        } else {
            ExtensionType::try_calculate_account_len::<spl_token_2022_interface::state::Mint>(
                &extensions,
            )
            .unwrap()
        };
        let rent = self.svm.minimum_balance_for_rent_exemption(space);
        let authority = self.admin.pubkey();
        let mut ixs = vec![solana_system_interface::instruction::create_account(
            &authority,
            &mint.pubkey(),
            rent,
            space as u64,
            token_program,
        )];
        if let Some((bps, max)) = transfer_fee {
            ixs.push(
                spl_token_2022_interface::extension::transfer_fee::instruction::initialize_transfer_fee_config(
                    token_program,
                    &mint.pubkey(),
                    Some(&authority),
                    Some(&authority),
                    bps,
                    max,
                )
                .unwrap(),
            );
        }
        if let Some(hook) = transfer_hook {
            ixs.push(
                spl_token_2022_interface::extension::transfer_hook::instruction::initialize(
                    token_program,
                    &mint.pubkey(),
                    Some(authority),
                    Some(hook),
                )
                .unwrap(),
            );
        }
        ixs.push(if *token_program == client::TOKEN_PROGRAM_ID {
            spl_token_interface::instruction::initialize_mint2(
                token_program,
                &mint.pubkey(),
                &authority,
                None,
                decimals,
            )
            .unwrap()
        } else {
            spl_token_2022_interface::instruction::initialize_mint2(
                token_program,
                &mint.pubkey(),
                &authority,
                None,
                decimals,
            )
            .unwrap()
        });
        self.send(&ixs, &[&mint]).expect("create mint");
        mint.pubkey()
    }

    /// Creates (idempotently) `owner`'s associated token account.
    pub fn create_ata(&mut self, owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
        let ix = spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(
            &self.admin.pubkey(),
            owner,
            mint,
            token_program,
        );
        self.send(&[ix], &[]).expect("create ata");
        pda::associated_token(owner, mint, token_program)
    }

    /// Mints `amount` into `account`. For wrapped SOL, wraps that many lamports instead.
    pub fn mint_to(
        &mut self,
        mint: &Pubkey,
        token_program: &Pubkey,
        account: &Pubkey,
        amount: u64,
    ) {
        let authority = self.admin.pubkey();
        if client::wallet::is_native(mint, token_program) {
            let ixs = [
                client::wallet::system_transfer(&authority, account, amount),
                client::wallet::sync_native(account),
            ];
            self.send(&ixs, &[]).expect("wrap SOL");
            return;
        }
        let ix = if *token_program == client::TOKEN_PROGRAM_ID {
            spl_token_interface::instruction::mint_to(
                token_program,
                mint,
                account,
                &authority,
                &[],
                amount,
            )
            .unwrap()
        } else {
            spl_token_2022_interface::instruction::mint_to(
                token_program,
                mint,
                account,
                &authority,
                &[],
                amount,
            )
            .unwrap()
        };
        self.send(&[ix], &[]).expect("mint_to");
    }

    /// A new funded trader with `amount` of `mint` in their associated token account.
    pub fn trader_with(
        &mut self,
        mint: &Pubkey,
        token_program: &Pubkey,
        amount: u64,
    ) -> (Keypair, Pubkey) {
        let trader = self.funded_keypair();
        let account = self.create_ata(&trader.pubkey(), mint, token_program);
        self.mint_to(mint, token_program, &account, amount);
        (trader, account)
    }
}

/// Adds the wrapped-SOL mint as it exists on every cluster: SPL Token, 9 decimals, no
/// authorities.
fn add_native_mint(svm: &mut LiteSVM) {
    let mut data = vec![0u8; 82];
    data[44] = 9;
    data[45] = 1;
    let account = solana_account::Account {
        lamports: svm.minimum_balance_for_rent_exemption(82),
        data,
        owner: client::TOKEN_PROGRAM_ID,
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(client::wallet::NATIVE_MINT, account)
        .expect("native mint");
}

/// Rewrites the upgrade authority in a loader-v3 program's ProgramData header.
pub fn set_upgrade_authority(svm: &mut LiteSVM, program: &Pubkey, authority: Option<&Pubkey>) {
    let program_data = pda::program_data(program);
    let mut account = svm.get_account(&program_data).expect("program data");
    match authority {
        Some(key) => {
            account.data[12] = 1;
            account.data[13..45].copy_from_slice(key.as_ref());
        }
        None => {
            account.data[12] = 0;
            account.data[13..45].fill(0);
        }
    }
    svm.set_account(program_data, account).unwrap();
}

/// Router events emitted (via self-CPI) in a transaction, in order.
pub fn events(meta: &TransactionMetadata) -> Vec<PipelineEvent> {
    meta.inner_instructions
        .iter()
        .flatten()
        .filter_map(|inner| pipeline_events::decode(&inner.instruction.data))
        .collect()
}

pub fn swap_events(meta: &TransactionMetadata) -> Vec<pipeline_events::SwapEvent> {
    events(meta)
        .into_iter()
        .filter_map(|event| match event {
            PipelineEvent::Swap(swap) => Some(swap),
            _ => None,
        })
        .collect()
}

/// Asserts a transaction failed and that its logs mention `needle`.
pub fn assert_fails_with(result: TxResult, needle: &str) {
    match result {
        Ok(meta) => panic!(
            "expected failure containing {needle:?}, but succeeded:\n{}",
            meta.logs.join("\n")
        ),
        Err(failed) => {
            let logs = failed.meta.logs.join("\n");
            assert!(
                logs.contains(needle) || format!("{:?}", failed.err).contains(needle),
                "expected failure containing {needle:?}, got {:?}:\n{logs}",
                failed.err
            );
        }
    }
}

// -------------------------------------------------------------------- CP strategy

/// A constant-product market with liquidity, ready to trade.
pub struct CpMarket {
    pub pool: Pubkey,
    pub market: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub token_program_a: Pubkey,
    pub token_program_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
}

impl CpMarket {
    pub fn token_program(&self, mint: &Pubkey) -> Pubkey {
        if *mint == self.mint_a {
            self.token_program_a
        } else {
            self.token_program_b
        }
    }

    pub fn vault(&self, mint: &Pubkey) -> Pubkey {
        if *mint == self.mint_a {
            self.vault_a
        } else {
            self.vault_b
        }
    }
}

pub fn cp_pool(mint_a: &Pubkey, mint_b: &Pubkey, fee_bps: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"pool",
            mint_a.as_ref(),
            mint_b.as_ref(),
            &fee_bps.to_le_bytes(),
        ],
        &CP_PROGRAM_ID,
    )
    .0
}

/// `create_market` for the CP strategy, with `fee_bps` pool fee.
pub fn cp_create_market_ix(
    creator: &Pubkey,
    mint_a: &Pubkey,
    tp_a: &Pubkey,
    mint_b: &Pubkey,
    tp_b: &Pubkey,
    fee_bps: u16,
) -> Instruction {
    let pool = cp_pool(mint_a, mint_b, fee_bps);
    client::create_market(&client::CreateMarket {
        creator: *creator,
        strategy_program: CP_PROGRAM_ID,
        market_state: pool,
        market_state_signs: false,
        init_data: borsh::to_vec(&fee_bps).unwrap(),
        max_creator_lamports: MAX_SETUP_LAMPORTS,
        strategy_accounts: vec![
            AccountMeta::new_readonly(*mint_a, false),
            AccountMeta::new_readonly(*mint_b, false),
            AccountMeta::new(pda::associated_token(&pool, mint_a, tp_a), false),
            AccountMeta::new(pda::associated_token(&pool, mint_b, tp_b), false),
            AccountMeta::new_readonly(*tp_a, false),
            AccountMeta::new_readonly(*tp_b, false),
            AccountMeta::new_readonly(client::ASSOCIATED_TOKEN_PROGRAM_ID, false),
        ],
    })
}

/// The CP strategy's `deposit` custom action, called directly on the strategy.
#[allow(clippy::too_many_arguments)]
pub fn cp_deposit_ix(
    depositor: &Pubkey,
    market: &CpMarket,
    depositor_a: &Pubkey,
    depositor_b: &Pubkey,
    amount_a: u64,
    amount_b: u64,
    hook_accounts: Vec<AccountMeta>,
) -> Instruction {
    let mut data = client::instruction_discriminator("deposit").to_vec();
    data.extend_from_slice(&amount_a.to_le_bytes());
    data.extend_from_slice(&amount_b.to_le_bytes());
    let mut accounts = vec![
        AccountMeta::new_readonly(*depositor, true),
        AccountMeta::new(market.pool, false),
        AccountMeta::new(*depositor_a, false),
        AccountMeta::new(*depositor_b, false),
        AccountMeta::new(market.vault_a, false),
        AccountMeta::new(market.vault_b, false),
        AccountMeta::new_readonly(market.mint_a, false),
        AccountMeta::new_readonly(market.mint_b, false),
        AccountMeta::new_readonly(market.token_program_a, false),
        AccountMeta::new_readonly(market.token_program_b, false),
    ];
    accounts.extend(hook_accounts);
    Instruction::new_with_bytes(CP_PROGRAM_ID, &data, accounts)
}

/// Registers the CP strategy (if needed), creates a market between two new mints, its
/// fee accounts, and deposits `reserve_a`/`reserve_b` of liquidity.
pub fn setup_cp_market(
    env: &mut Env,
    token_program_a: Pubkey,
    token_program_b: Pubkey,
    reserve_a: u64,
    reserve_b: u64,
    fee_bps: u16,
) -> CpMarket {
    let mint_a = env.create_mint(&token_program_a, 6, None, None);
    let mint_b = env.create_mint(&token_program_b, 9, None, None);
    setup_cp_market_with_mints(
        env,
        mint_a,
        token_program_a,
        mint_b,
        token_program_b,
        reserve_a,
        reserve_b,
        fee_bps,
        vec![],
    )
}

#[allow(clippy::too_many_arguments)]
pub fn setup_cp_market_with_mints(
    env: &mut Env,
    mint_a: Pubkey,
    token_program_a: Pubkey,
    mint_b: Pubkey,
    token_program_b: Pubkey,
    reserve_a: u64,
    reserve_b: u64,
    fee_bps: u16,
    deposit_hook_accounts: Vec<AccountMeta>,
) -> CpMarket {
    if env
        .svm
        .get_account(&pda::strategy(&CP_PROGRAM_ID))
        .is_none()
    {
        env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    }
    let creator = env.funded_keypair();
    let ix = cp_create_market_ix(
        &creator.pubkey(),
        &mint_a,
        &token_program_a,
        &mint_b,
        &token_program_b,
        fee_bps,
    );
    env.send(&[ix], &[&creator]).expect("create_market");

    let pool = cp_pool(&mint_a, &mint_b, fee_bps);
    let market_address = pda::market(&CP_PROGRAM_ID, &pool);
    let market = CpMarket {
        pool,
        market: market_address,
        mint_a,
        mint_b,
        token_program_a,
        token_program_b,
        vault_a: pda::associated_token(&pool, &mint_a, &token_program_a),
        vault_b: pda::associated_token(&pool, &mint_b, &token_program_b),
    };
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &market_address, &mint_a, &token_program_a),
            client::init_fee_account(&payer, &market_address, &mint_b, &token_program_b),
        ],
        &[],
    )
    .expect("init fee accounts");

    let lp = env.funded_keypair();
    let lp_a = env.create_ata(&lp.pubkey(), &mint_a, &token_program_a);
    let lp_b = env.create_ata(&lp.pubkey(), &mint_b, &token_program_b);
    env.mint_to(&mint_a, &token_program_a, &lp_a, reserve_a * 2);
    env.mint_to(&mint_b, &token_program_b, &lp_b, reserve_b * 2);
    let deposit = cp_deposit_ix(
        &lp.pubkey(),
        &market,
        &lp_a,
        &lp_b,
        reserve_a,
        reserve_b,
        deposit_hook_accounts,
    );
    env.send(&[deposit], &[&lp]).expect("deposit");
    market
}

/// Swap accounts for the CP strategy: the only strategy account is the output vault.
pub fn cp_swap_accounts(
    market: &CpMarket,
    trader: &Pubkey,
    input_mint: &Pubkey,
    trader_input: &Pubkey,
    output_mint: &Pubkey,
    trader_output: &Pubkey,
) -> client::SwapAccounts {
    client::SwapAccounts {
        trader: *trader,
        strategy_program: CP_PROGRAM_ID,
        market_state: market.pool,
        trader_input: *trader_input,
        trader_output: *trader_output,
        input_vault: market.vault(input_mint),
        input_mint: *input_mint,
        output_mint: *output_mint,
        input_token_program: market.token_program(input_mint),
        output_token_program: market.token_program(output_mint),
        integrator_fee_account: None,
        strategy_accounts: vec![AccountMeta::new(market.vault(output_mint), false)],
        input_hook_accounts: vec![],
    }
}

pub fn exact_in(amount_in: u64, min_amount_out: u64) -> client::SwapArgs {
    client::SwapArgs {
        mode: psi::TradeMode::ExactIn,
        amount_in,
        amount_out: 0,
        min_amount_out,
        integrator_fee_bps: 0,
        input_hook_accounts: 0,
        extra: vec![],
    }
}

/// Mirrors the CP strategy's pricing (with a 256-bit product, as the strategy does).
pub fn cp_out(reserve_in: u64, reserve_out: u64, amount_in: u64, fee_bps: u16) -> u64 {
    let in_after_fee = amount_in as u128 * (10_000 - fee_bps as u128);
    let denominator = reserve_in as u128 * 10_000 + in_after_fee;
    mul_div(reserve_out as u128, in_after_fee, denominator).0 as u64
}

pub fn cp_in(reserve_in: u64, reserve_out: u64, amount_out: u64, fee_bps: u16) -> u64 {
    let denominator = (reserve_out - amount_out) as u128 * (10_000 - fee_bps as u128);
    let (quotient, remainder) =
        mul_div(reserve_in as u128 * 10_000, amount_out as u128, denominator);
    (quotient + (remainder > 0) as u128) as u64
}

/// `a × b / d` as (quotient, remainder) through a 256-bit product. `d` must not be 0 and
/// the quotient must fit in 128 bits.
fn mul_div(a: u128, b: u128, d: u128) -> (u128, u128) {
    let mask = u64::MAX as u128;
    let (a1, a0) = (a >> 64, a & mask);
    let (b1, b0) = (b >> 64, b & mask);
    let (mid, mid_carry) = (a1 * b0).overflowing_add(a0 * b1);
    let (lo, lo_carry) = (a0 * b0).overflowing_add(mid << 64);
    let hi = a1 * b1 + (mid >> 64) + ((mid_carry as u128) << 64) + lo_carry as u128;
    assert!(hi < d, "quotient overflows 128 bits");
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
