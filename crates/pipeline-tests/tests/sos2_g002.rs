//! SOS audit #2, G002: Token-2022 mints that are frozen by default
//! (`DefaultAccountState = Frozen`) or `NonTransferable`.
//!
//! Both are issuer powers over the issuer's own token, so the question is only that
//! nothing else is harmed and nothing half-happens. Asserted against the real router,
//! the CP strategy and Token-2022, with an unrelated SPL Token market trading before
//! and after:
//!
//! Default-frozen mint (paired with an SPL Token mint):
//! - `create_market` works; the pool's vault for the frozen mint is created frozen;
//! - `init_fee_account` works for both mints; the frozen mint's fee account is frozen;
//! - liquidity can't enter the frozen vault until the issuer thaws it;
//! - selling the frozen mint fails while the market's fee account is frozen. The
//!   router's fee transfer is the step that fails, after the strategy has paid out,
//!   and the whole swap reverts: no balance or pool state changes;
//! - buying it into a trader's default-frozen account fails the same way; once the
//!   issuer thaws that account the purchase works and the fee is taken in the other
//!   mint;
//! - `sweep_fees` for the frozen mint is a no-op while its fee account is empty. After
//!   the issuer thaws the fee account and fees arrive, the sweep fails until the
//!   treasury's (default-frozen) account is thawed too, and the fee stays put;
//! - a pool created outside the router (CP's seeding path) can only be funded once the
//!   issuer thaws its vaults; `register_existing_market` then records it.
//!
//! NonTransferable mint:
//! - `create_market` and `init_fee_account` work (Token-2022 gives every account the
//!   non-transferable extension);
//! - every transfer of the mint fails: deposits, the router's input transfer when
//!   selling it, the strategy's payout when buying it (with inventory the issuer
//!   minted straight into the vault), seeding a pool, and sweeping fees the issuer
//!   minted straight into the fee account. Each failure leaves everything unchanged;
//! - `sweep_fees` with an empty fee account is a no-op;
//! - `register_existing_market` records a pool holding the mint: the router doesn't
//!   read the mint's extensions when registering.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_strategy_interface::{self as psi, fees};
use sha2::{Digest, Sha256};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;
use spl_token_2022_interface::extension::ExtensionType;
use spl_token_2022_interface::state::AccountState;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const RESERVE: u64 = 1_000_000_000;
const SWAP: u64 = 1_000_000;
const CP_FEE_BPS: u16 = 30;
/// Token-2022's `AccountFrozen` error.
const FROZEN: &str = "custom program error: 0x11";
/// Token-2022's `NonTransferable` error.
const NON_TRANSFERABLE: &str = "custom program error: 0x25";
/// Offset of the account state in a token account.
const STATE_OFFSET: usize = 108;

#[derive(Clone, Copy)]
enum Extension {
    DefaultFrozen,
    NonTransferable,
}

/// A Token-2022 mint with one extension; the admin is mint and freeze authority.
fn extension_mint(env: &mut Env, extension: Extension) -> Pubkey {
    let mint = Keypair::new();
    let kind = match extension {
        Extension::DefaultFrozen => ExtensionType::DefaultAccountState,
        Extension::NonTransferable => ExtensionType::NonTransferable,
    };
    let space =
        ExtensionType::try_calculate_account_len::<spl_token_2022_interface::state::Mint>(&[kind])
            .unwrap();
    let rent = env.svm.minimum_balance_for_rent_exemption(space);
    let admin = env.admin.pubkey();
    let init_extension = match extension {
        Extension::DefaultFrozen => {
            spl_token_2022_interface::extension::default_account_state::instruction::initialize_default_account_state(
                &TOKEN_2022,
                &mint.pubkey(),
                &AccountState::Frozen,
            )
        }
        Extension::NonTransferable => {
            spl_token_2022_interface::instruction::initialize_non_transferable_mint(
                &TOKEN_2022,
                &mint.pubkey(),
            )
        }
    }
    .unwrap();
    let ixs = [
        solana_system_interface::instruction::create_account(
            &admin,
            &mint.pubkey(),
            rent,
            space as u64,
            &TOKEN_2022,
        ),
        init_extension,
        spl_token_2022_interface::instruction::initialize_mint2(
            &TOKEN_2022,
            &mint.pubkey(),
            &admin,
            Some(&admin),
            6,
        )
        .unwrap(),
    ];
    env.send(&ixs, &[&mint]).expect("create extension mint");
    mint.pubkey()
}

/// The issuer thaws a token account of its default-frozen mint.
fn thaw(env: &mut Env, account: &Pubkey, mint: &Pubkey) {
    let admin = env.admin.pubkey();
    let ix = spl_token_2022_interface::instruction::thaw_account(
        &TOKEN_2022,
        account,
        mint,
        &admin,
        &[],
    )
    .unwrap();
    env.send(&[ix], &[]).expect("thaw");
}

fn is_frozen(env: &Env, account: &Pubkey) -> bool {
    env.account_data(account)[STATE_OFFSET] == AccountState::Frozen as u8
}

/// Account data of every listed account, to show a failed instruction changed nothing.
fn snapshot(env: &Env, accounts: &[Pubkey]) -> Vec<Vec<u8>> {
    accounts.iter().map(|a| env.account_data(a)).collect()
}

/// `create_market` on the CP strategy for (`mint_a`, `mint_b`), with no liquidity.
fn create_cp_market(env: &mut Env, mint_a: Pubkey, tp_a: Pubkey, mint_b: Pubkey) -> CpMarket {
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
        &tp_a,
        &mint_b,
        &TOKEN,
        CP_FEE_BPS,
    );
    env.send(&[ix], &[&creator]).expect("create_market works");
    let pool = cp_pool(&mint_a, &mint_b, CP_FEE_BPS);
    CpMarket {
        pool,
        market: pda::market(&CP_PROGRAM_ID, &pool),
        mint_a,
        mint_b,
        token_program_a: tp_a,
        token_program_b: TOKEN,
        vault_a: pda::associated_token(&pool, &mint_a, &tp_a),
        vault_b: pda::associated_token(&pool, &mint_b, &TOKEN),
    }
}

fn init_fee_accounts(env: &mut Env, m: &CpMarket) {
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &m.market, &m.mint_a, &m.token_program_a),
            client::init_fee_account(&payer, &m.market, &m.mint_b, &m.token_program_b),
        ],
        &[],
    )
    .expect("init_fee_account works");
}

fn fee_account(m: &CpMarket, mint: &Pubkey) -> Pubkey {
    pda::fee_account(&m.market, mint, &m.token_program(mint))
}

fn swap_ix(
    m: &CpMarket,
    trader: &Keypair,
    input_mint: &Pubkey,
    trader_input: &Pubkey,
    output_mint: &Pubkey,
    trader_output: &Pubkey,
) -> Instruction {
    let accounts = cp_swap_accounts(
        m,
        &trader.pubkey(),
        input_mint,
        trader_input,
        output_mint,
        trader_output,
    );
    client::swap(&accounts, &exact_in(SWAP, 1))
}

/// An unrelated SPL Token market and a trader on it, to show it is unaffected.
struct Bystander {
    market: CpMarket,
    trader: Keypair,
    trader_a: Pubkey,
    trader_b: Pubkey,
}

fn bystander(env: &mut Env) -> Bystander {
    let market = setup_cp_market(env, TOKEN, TOKEN, RESERVE, RESERVE, CP_FEE_BPS);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 100 * SWAP);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);
    Bystander {
        market,
        trader,
        trader_a,
        trader_b,
    }
}

fn assert_bystander_trades(env: &mut Env, b: &Bystander) {
    let received_before = env.token_balance(&b.trader_b);
    let fee_before = env.token_balance(&fee_account(&b.market, &b.market.mint_a));
    let ix = swap_ix(
        &b.market,
        &b.trader,
        &b.market.mint_a,
        &b.trader_a,
        &b.market.mint_b,
        &b.trader_b,
    );
    env.send(&[ix], &[&b.trader])
        .expect("an unrelated market keeps trading");
    assert!(env.token_balance(&b.trader_b) > received_before);
    assert_eq!(
        env.token_balance(&fee_account(&b.market, &b.market.mint_a)),
        fee_before + fees::pipeline_fee(SWAP)
    );
}

/// The CP strategy's pool PDA for a pool seeded through `pipeline_seed_liquidity`.
fn seeded_pool(seeder: &Pubkey, mint_a: &Pubkey, mint_b: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"seeded",
            seeder.as_ref(),
            mint_a.as_ref(),
            mint_b.as_ref(),
            &CP_FEE_BPS.to_le_bytes(),
        ],
        &CP_PROGRAM_ID,
    )
    .0
}

/// CP's `pipeline_seed_liquidity`, called directly (as a launchpad would), creating and
/// funding a pool of (`mint_a` on Token-2022, `mint_b` on SPL Token).
#[allow(clippy::too_many_arguments)]
fn seed_ix(
    seeder: &Pubkey,
    payer: &Pubkey,
    mint_a: &Pubkey,
    escrow_a: &Pubkey,
    mint_b: &Pubkey,
    escrow_b: &Pubkey,
    amount: u64,
) -> Instruction {
    let pool = seeded_pool(seeder, mint_a, mint_b);
    let request = psi::SeedRequest {
        amounts: vec![amount, amount],
        data: borsh::to_vec(&CP_FEE_BPS).unwrap(),
    };
    Instruction::new_with_bytes(
        CP_PROGRAM_ID,
        &psi::instruction_data(psi::discriminator::SEED_LIQUIDITY, &request),
        vec![
            AccountMeta::new_readonly(*seeder, true),
            AccountMeta::new(pool, false),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(*seeder, false),
            AccountMeta::new_readonly(*mint_a, false),
            AccountMeta::new(*escrow_a, false),
            AccountMeta::new_readonly(TOKEN_2022, false),
            AccountMeta::new_readonly(*mint_b, false),
            AccountMeta::new(*escrow_b, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new(pda::associated_token(&pool, mint_a, &TOKEN_2022), false),
            AccountMeta::new(pda::associated_token(&pool, mint_b, &TOKEN), false),
            AccountMeta::new_readonly(client::ASSOCIATED_TOKEN_PROGRAM_ID, false),
        ],
    )
}

/// Overwrites the reserves recorded in a CP pool (after the discriminator and four keys).
fn set_pool_reserves(env: &mut Env, pool: &Pubkey, reserve_a: u64, reserve_b: u64) {
    let mut account = env.svm.get_account(pool).unwrap();
    account.data[136..144].copy_from_slice(&reserve_a.to_le_bytes());
    account.data[144..152].copy_from_slice(&reserve_b.to_le_bytes());
    env.svm.set_account(*pool, account).unwrap();
}

/// Writes an empty CP `Pool` account (Anchor discriminator plus Borsh fields) at
/// `m.pool`, as if the strategy had created it outside the router.
fn plant_cp_pool(env: &mut Env, m: &CpMarket) {
    let address = m.pool;
    let mut data = Sha256::digest(b"account:Pool")[..8].to_vec();
    for key in [&m.mint_a, &m.mint_b, &m.vault_a, &m.vault_b] {
        data.extend_from_slice(key.as_ref());
    }
    data.extend_from_slice(&0u64.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());
    data.extend_from_slice(&CP_FEE_BPS.to_le_bytes());
    data.extend_from_slice(pda::market(&CP_PROGRAM_ID, &address).as_ref());
    data.extend_from_slice(Pubkey::default().as_ref());
    data.push(0);
    env.svm
        .set_account(
            address,
            Account {
                lamports: SOL,
                data,
                owner: CP_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
}

/// Default-frozen mint on a market made by `create_market`: market and fee accounts are
/// created, swaps and sweeps fail atomically until the issuer thaws the accounts
/// involved, and an unrelated market is unaffected throughout.
#[test]
fn g002_default_frozen_mint_creates_markets_and_fails_closed_until_thawed() {
    let mut env = Env::new();
    let bystander = bystander(&mut env);
    assert_bystander_trades(&mut env, &bystander);

    let frozen = extension_mint(&mut env, Extension::DefaultFrozen);
    let normal = env.create_mint(&TOKEN, 6, None, None);
    let m = create_cp_market(&mut env, frozen, TOKEN_2022, normal);
    assert!(
        is_frozen(&env, &m.vault_a),
        "the pool's vault starts frozen"
    );
    assert!(!is_frozen(&env, &m.vault_b));
    init_fee_accounts(&mut env, &m);
    let fee_frozen = fee_account(&m, &frozen);
    let fee_normal = fee_account(&m, &normal);
    assert!(
        is_frozen(&env, &fee_frozen),
        "the fee account starts frozen"
    );
    let market = env.market(&m.market);
    assert_eq!(market.mints(), &[frozen, normal]);

    // Liquidity can't enter the frozen vault; the issuer must thaw it.
    let lp = env.funded_keypair();
    let lp_frozen = env.create_ata(&lp.pubkey(), &frozen, &TOKEN_2022);
    thaw(&mut env, &lp_frozen, &frozen);
    env.mint_to(&frozen, &TOKEN_2022, &lp_frozen, RESERVE);
    let lp_normal = env.create_ata(&lp.pubkey(), &normal, &TOKEN);
    env.mint_to(&normal, &TOKEN, &lp_normal, RESERVE);
    let deposit = cp_deposit_ix(
        &lp.pubkey(),
        &m,
        &lp_frozen,
        &lp_normal,
        RESERVE,
        RESERVE,
        vec![],
    );
    let watched = [lp_frozen, lp_normal, m.vault_a, m.vault_b, m.pool];
    let before = snapshot(&env, &watched);
    assert_fails_with(env.send(std::slice::from_ref(&deposit), &[&lp]), FROZEN);
    assert_eq!(snapshot(&env, &watched), before);
    thaw(&mut env, &m.vault_a, &frozen);
    env.send(&[deposit], &[&lp]).expect("deposit after thaw");

    // A seller whose own account the issuer thawed.
    let seller = env.funded_keypair();
    let seller_frozen = env.create_ata(&seller.pubkey(), &frozen, &TOKEN_2022);
    thaw(&mut env, &seller_frozen, &frozen);
    env.mint_to(&frozen, &TOKEN_2022, &seller_frozen, 10 * SWAP);
    let seller_normal = env.create_ata(&seller.pubkey(), &normal, &TOKEN);
    // A buyer whose account in the frozen mint is still frozen.
    let (buyer, buyer_normal) = env.trader_with(&normal, &TOKEN, 10 * SWAP);
    let buyer_frozen = env.create_ata(&buyer.pubkey(), &frozen, &TOKEN_2022);
    assert!(is_frozen(&env, &buyer_frozen));

    let watched = [
        seller_frozen,
        seller_normal,
        buyer_frozen,
        buyer_normal,
        m.vault_a,
        m.vault_b,
        m.pool,
        fee_frozen,
        fee_normal,
    ];

    // Selling fails at the router's fee transfer into the frozen fee account, after the
    // strategy has paid out; the whole swap reverts.
    let sell = swap_ix(
        &m,
        &seller,
        &frozen,
        &seller_frozen,
        &normal,
        &seller_normal,
    );
    let before = snapshot(&env, &watched);
    let failed = env
        .send(std::slice::from_ref(&sell), &[&seller])
        .expect_err("selling into a frozen fee account fails");
    let logs = failed.meta.logs.join("\n");
    assert!(logs.contains(FROZEN), "{logs}");
    assert!(
        logs.contains(&format!("Program {CP_PROGRAM_ID} success")),
        "the strategy had paid out before the fee transfer failed:\n{logs}"
    );
    assert_eq!(snapshot(&env, &watched), before);

    // Buying into the buyer's frozen account fails at the strategy's payout.
    let buy = swap_ix(&m, &buyer, &normal, &buyer_normal, &frozen, &buyer_frozen);
    assert_fails_with(env.send(std::slice::from_ref(&buy), &[&buyer]), FROZEN);
    assert_eq!(snapshot(&env, &watched), before);

    // Once the issuer thaws the buyer's account the purchase works; its fee is in the
    // normal mint, so the frozen fee account is not involved.
    thaw(&mut env, &buyer_frozen, &frozen);
    env.send(&[buy], &[&buyer]).expect("buy after thaw");
    assert!(env.token_balance(&buyer_frozen) > 0);
    assert_eq!(env.token_balance(&fee_normal), fees::pipeline_fee(SWAP));

    // Sweeping the empty, frozen fee account is a no-op; sweeping the other mint works.
    let treasury = env.treasury_owner;
    let sweep_frozen = client::sweep_fees(&m.market, &frozen, &TOKEN_2022, &treasury, vec![]);
    let meta = env
        .send(std::slice::from_ref(&sweep_frozen), &[])
        .expect("sweeping an empty fee account succeeds");
    assert!(events(&meta).is_empty(), "nothing swept, nothing reported");
    assert_eq!(env.token_balance(&fee_frozen), 0);
    let treasury_normal = env.create_ata(&treasury, &normal, &TOKEN);
    env.send(
        &[client::sweep_fees(
            &m.market,
            &normal,
            &TOKEN,
            &treasury,
            vec![],
        )],
        &[],
    )
    .expect("sweep the normal mint");
    assert_eq!(
        env.token_balance(&treasury_normal),
        fees::pipeline_fee(SWAP)
    );

    // The issuer thaws the fee account: selling works and the fee arrives.
    thaw(&mut env, &fee_frozen, &frozen);
    env.send(&[sell], &[&seller]).expect("sell after thaw");
    let fee = fees::pipeline_fee(SWAP);
    assert_eq!(env.token_balance(&fee_frozen), fee);

    // The treasury's account in the frozen mint starts frozen too: the sweep fails and
    // the fee stays in the fee account until the issuer thaws it.
    let treasury_frozen = env.create_ata(&treasury, &frozen, &TOKEN_2022);
    assert!(is_frozen(&env, &treasury_frozen));
    assert_fails_with(env.send(std::slice::from_ref(&sweep_frozen), &[]), FROZEN);
    assert_eq!(env.token_balance(&fee_frozen), fee);
    assert_eq!(env.token_balance(&treasury_frozen), 0);
    thaw(&mut env, &treasury_frozen, &frozen);
    env.send(&[sweep_frozen], &[]).expect("sweep after thaw");
    assert_eq!(env.token_balance(&treasury_frozen), fee);
    assert_eq!(env.token_balance(&fee_frozen), 0);

    assert_bystander_trades(&mut env, &bystander);
}

/// A default-frozen pool created outside the router (CP's seeding path) can only be
/// funded into vaults the issuer has thawed; `register_existing_market` then records it
/// and its fee account can be created.
#[test]
fn g002_default_frozen_seeded_pool_registers_once_the_issuer_thaws_its_vaults() {
    let mut env = Env::new();
    env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    let bystander = bystander(&mut env);
    let frozen = extension_mint(&mut env, Extension::DefaultFrozen);
    let normal = env.create_mint(&TOKEN, 6, None, None);
    let seeder = env.funded_keypair();
    let payer = env.funded_keypair();
    let escrow_frozen = env.create_ata(&seeder.pubkey(), &frozen, &TOKEN_2022);
    thaw(&mut env, &escrow_frozen, &frozen);
    env.mint_to(&frozen, &TOKEN_2022, &escrow_frozen, RESERVE);
    let escrow_normal = env.create_ata(&seeder.pubkey(), &normal, &TOKEN);
    env.mint_to(&normal, &TOKEN, &escrow_normal, RESERVE);
    let pool = seeded_pool(&seeder.pubkey(), &frozen, &normal);
    let vault_frozen = pda::associated_token(&pool, &frozen, &TOKEN_2022);
    let seed = seed_ix(
        &seeder.pubkey(),
        &payer.pubkey(),
        &frozen,
        &escrow_frozen,
        &normal,
        &escrow_normal,
        RESERVE,
    );

    // The vault the seed creates is frozen, so funding it fails and nothing is created.
    assert_fails_with(
        env.send(std::slice::from_ref(&seed), &[&seeder, &payer]),
        FROZEN,
    );
    assert!(env.svm.get_account(&pool).is_none());
    assert_eq!(env.token_balance(&escrow_frozen), RESERVE);

    // The issuer thaws the vault in advance (CP creates vaults idempotently).
    env.create_ata(&pool, &frozen, &TOKEN_2022);
    thaw(&mut env, &vault_frozen, &frozen);
    env.send(&[seed], &[&seeder, &payer])
        .expect("seed into a thawed vault");

    let registrar = env.funded_keypair();
    let ix = client::register_existing_market(&registrar.pubkey(), &CP_PROGRAM_ID, &pool, vec![]);
    env.send(&[ix], &[&registrar])
        .expect("register_existing_market works");
    let market = pda::market(&CP_PROGRAM_ID, &pool);
    let recorded = env.market(&market);
    assert_eq!(recorded.mints(), &[frozen, normal]);
    assert_eq!(recorded.vaults[0], vault_frozen);
    let fee_ix = client::init_fee_account(&registrar.pubkey(), &market, &frozen, &TOKEN_2022);
    env.send(&[fee_ix], &[&registrar])
        .expect("init_fee_account works");
    assert!(is_frozen(
        &env,
        &pda::fee_account(&market, &frozen, &TOKEN_2022)
    ));

    assert_bystander_trades(&mut env, &bystander);
}

/// NonTransferable mint: markets and fee accounts are created, and every movement of
/// the mint (deposit, sell, buy, sweep) fails without changing anything. An empty
/// sweep is a no-op, and an unrelated market keeps trading.
#[test]
fn g002_non_transferable_mint_creates_markets_and_every_transfer_fails_closed() {
    let mut env = Env::new();
    let bystander = bystander(&mut env);
    assert_bystander_trades(&mut env, &bystander);

    let locked = extension_mint(&mut env, Extension::NonTransferable);
    let normal = env.create_mint(&TOKEN, 6, None, None);
    let m = create_cp_market(&mut env, locked, TOKEN_2022, normal);
    init_fee_accounts(&mut env, &m);
    let fee_locked = fee_account(&m, &locked);
    let fee_normal = fee_account(&m, &normal);
    assert_eq!(env.market(&m.market).mints(), &[locked, normal]);

    // Deposits can't move the mint into the vault.
    let lp = env.funded_keypair();
    let lp_locked = env.create_ata(&lp.pubkey(), &locked, &TOKEN_2022);
    env.mint_to(&locked, &TOKEN_2022, &lp_locked, RESERVE);
    let lp_normal = env.create_ata(&lp.pubkey(), &normal, &TOKEN);
    env.mint_to(&normal, &TOKEN, &lp_normal, RESERVE);
    let deposit = cp_deposit_ix(
        &lp.pubkey(),
        &m,
        &lp_locked,
        &lp_normal,
        RESERVE,
        RESERVE,
        vec![],
    );
    let watched = [lp_locked, lp_normal, m.vault_a, m.vault_b, m.pool];
    let before = snapshot(&env, &watched);
    assert_fails_with(env.send(&[deposit], &[&lp]), NON_TRANSFERABLE);
    assert_eq!(snapshot(&env, &watched), before);

    // Stand-in for a strategy holding inventory the issuer minted straight into its
    // vaults: mint into both vaults and record the reserves in the pool.
    env.mint_to(&locked, &TOKEN_2022, &m.vault_a, RESERVE);
    env.mint_to(&normal, &TOKEN, &m.vault_b, RESERVE);
    set_pool_reserves(&mut env, &m.pool, RESERVE, RESERVE);

    let seller = env.funded_keypair();
    let seller_locked = env.create_ata(&seller.pubkey(), &locked, &TOKEN_2022);
    env.mint_to(&locked, &TOKEN_2022, &seller_locked, 10 * SWAP);
    let seller_normal = env.create_ata(&seller.pubkey(), &normal, &TOKEN);
    let (buyer, buyer_normal) = env.trader_with(&normal, &TOKEN, 10 * SWAP);
    let buyer_locked = env.create_ata(&buyer.pubkey(), &locked, &TOKEN_2022);
    let watched = [
        seller_locked,
        seller_normal,
        buyer_locked,
        buyer_normal,
        m.vault_a,
        m.vault_b,
        m.pool,
        fee_locked,
        fee_normal,
    ];
    let before = snapshot(&env, &watched);

    // Selling fails at the router's input transfer.
    let sell = swap_ix(
        &m,
        &seller,
        &locked,
        &seller_locked,
        &normal,
        &seller_normal,
    );
    assert_fails_with(env.send(&[sell], &[&seller]), NON_TRANSFERABLE);
    assert_eq!(snapshot(&env, &watched), before);

    // Buying fails at the strategy's payout, after the router took the buyer's input;
    // that input transfer is undone.
    let buy = swap_ix(&m, &buyer, &normal, &buyer_normal, &locked, &buyer_locked);
    assert_fails_with(env.send(&[buy], &[&buyer]), NON_TRANSFERABLE);
    assert_eq!(snapshot(&env, &watched), before);

    // No fee can ever arrive in the mint's fee account, so the sweep is a no-op.
    let treasury = env.treasury_owner;
    let treasury_locked = env.create_ata(&treasury, &locked, &TOKEN_2022);
    let sweep = client::sweep_fees(&m.market, &locked, &TOKEN_2022, &treasury, vec![]);
    let meta = env
        .send(std::slice::from_ref(&sweep), &[])
        .expect("sweeping an empty fee account succeeds");
    assert!(events(&meta).is_empty());
    // Tokens the issuer mints straight into the fee account can't be swept either.
    env.mint_to(&locked, &TOKEN_2022, &fee_locked, SWAP);
    assert_fails_with(env.send(&[sweep], &[]), NON_TRANSFERABLE);
    assert_eq!(env.token_balance(&fee_locked), SWAP);
    assert_eq!(env.token_balance(&treasury_locked), 0);

    assert_bystander_trades(&mut env, &bystander);
}

/// NonTransferable mint outside `create_market`: seeding a pool fails (it transfers the
/// mint), and the never-created pool is refused by `register_existing_market`
/// (`MarketStateNotOwnedByStrategy`). A pool holding the mint that
/// does exist is recorded by `register_existing_market`, and its fee account can be
/// created: the router doesn't read the mint's extensions to register a market.
#[test]
fn g002_non_transferable_mint_register_existing_market_records_the_pool() {
    let mut env = Env::new();
    env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    let bystander = bystander(&mut env);
    let locked = extension_mint(&mut env, Extension::NonTransferable);
    let normal = env.create_mint(&TOKEN, 6, None, None);
    let seeder = env.funded_keypair();
    let payer = env.funded_keypair();
    let escrow_locked = env.create_ata(&seeder.pubkey(), &locked, &TOKEN_2022);
    env.mint_to(&locked, &TOKEN_2022, &escrow_locked, RESERVE);
    let escrow_normal = env.create_ata(&seeder.pubkey(), &normal, &TOKEN);
    env.mint_to(&normal, &TOKEN, &escrow_normal, RESERVE);
    let seed = seed_ix(
        &seeder.pubkey(),
        &payer.pubkey(),
        &locked,
        &escrow_locked,
        &normal,
        &escrow_normal,
        RESERVE,
    );
    assert_fails_with(env.send(&[seed], &[&seeder, &payer]), NON_TRANSFERABLE);
    let seeded = seeded_pool(&seeder.pubkey(), &locked, &normal);
    assert!(env.svm.get_account(&seeded).is_none());
    assert_eq!(env.token_balance(&escrow_locked), RESERVE);

    let registrar = env.funded_keypair();
    let ix = client::register_existing_market(&registrar.pubkey(), &CP_PROGRAM_ID, &seeded, vec![]);
    assert_fails_with(
        env.send(&[ix], &[&registrar]),
        "MarketStateNotOwnedByStrategy",
    );
    assert!(env
        .svm
        .get_account(&pda::market(&CP_PROGRAM_ID, &seeded))
        .is_none());

    // A CP pool holding the mint, made by the strategy outside the router.
    let pool = Pubkey::new_unique();
    let m = CpMarket {
        pool,
        market: pda::market(&CP_PROGRAM_ID, &pool),
        mint_a: locked,
        mint_b: normal,
        token_program_a: TOKEN_2022,
        token_program_b: TOKEN,
        vault_a: pda::associated_token(&pool, &locked, &TOKEN_2022),
        vault_b: pda::associated_token(&pool, &normal, &TOKEN),
    };
    plant_cp_pool(&mut env, &m);
    env.create_ata(&pool, &locked, &TOKEN_2022);
    env.create_ata(&pool, &normal, &TOKEN);
    let ix = client::register_existing_market(&registrar.pubkey(), &CP_PROGRAM_ID, &pool, vec![]);
    env.send(&[ix], &[&registrar])
        .expect("register_existing_market works");
    let recorded = env.market(&m.market);
    assert_eq!(recorded.mints(), &[locked, normal]);
    assert_eq!(&recorded.vaults[..2], &[m.vault_a, m.vault_b]);
    init_fee_accounts(&mut env, &m);
    assert!(env.svm.get_account(&fee_account(&m, &locked)).is_some());

    assert_bystander_trades(&mut env, &bystander);
}
