//! Regression tests for the findings of the first security review (2026-10-13).

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_quoter::{MarketView, QuoteRequest, Quoter};
use pipeline_strategy_interface::{fees, TradeMode};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

fn transfer_checked_ix(
    token_program: &Pubkey,
    from: &Pubkey,
    mint: &Pubkey,
    to: &Pubkey,
    authority: &Pubkey,
    amount: u64,
    decimals: u8,
) -> Instruction {
    spl_token_2022_interface::instruction::transfer_checked(
        token_program,
        from,
        mint,
        to,
        authority,
        &[],
        amount,
        decimals,
    )
    .unwrap()
}

/// H1: tokens deposited straight into a pool's vault in the same transaction are a
/// donation; the strategy credits only what the router sent, so the fee cannot be
/// dodged and the swap event reflects the real trade.
#[test]
fn pre_deposited_input_is_a_donation_not_fee_free_input() {
    let mut env = Env::new();
    let (ra, rb) = (1_000_000_000u64, 1_000_000_000u64);
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, ra, rb, 30);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 20_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);

    let pre_deposit = transfer_checked_ix(
        &TOKEN,
        &trader_a,
        &market.mint_a,
        &market.vault_a,
        &trader.pubkey(),
        5_000_000,
        6,
    );
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    let swap = client::swap(&accounts, &exact_in(1, 0));
    let meta = env.send(&[pre_deposit.clone(), swap], &[&trader]).unwrap();

    // The trader got the output for 1 unit only, and paid the fee on 1 unit.
    let event = &swap_events(&meta)[0];
    assert_eq!(event.net_sent, 1);
    assert_eq!(event.received, cp_out(ra, rb, 1, 30));
    assert_eq!(event.pipeline_fee, 1);
    // The 5,000,000 stayed in the pool as a donation.
    assert_eq!(env.token_balance(&market.vault_a), ra + 5_000_000 + 1);

    // Exact-out with a pre-deposit cannot get the deposit refunded either: the strategy
    // needs real input from the router.
    let mut args = exact_in(1, 1_000_000);
    args.mode = TradeMode::ExactOut;
    args.amount_out = 1_000_000;
    let swap = client::swap(&accounts, &args);
    assert_fails_with(
        env.send(&[pre_deposit, swap], &[&trader]),
        "InsufficientInput",
    );
}

/// M1: a Token-2022 mint whose permanent delegate is the market PDA lets the strategy
/// pull extra input; the router caps what the trader parts with at `amount_in`.
#[test]
fn router_caps_trader_spend_even_with_a_permanent_delegate() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let mint_b = env.create_mint(&TOKEN, 6, None, None);

    // The input mint's permanent delegate is the market PDA, which can be computed
    // before the market exists.
    let mint_a = Keypair::new();
    let state = Pubkey::find_program_address(
        &[b"evil", mint_a.pubkey().as_ref(), mint_b.as_ref()],
        &EVIL_ID,
    )
    .0;
    let market = pda::market(&EVIL_ID, &state);
    let space = spl_token_2022_interface::extension::ExtensionType::try_calculate_account_len::<
        spl_token_2022_interface::state::Mint,
    >(&[spl_token_2022_interface::extension::ExtensionType::PermanentDelegate])
    .unwrap();
    let admin = env.admin.pubkey();
    let rent = env.svm.minimum_balance_for_rent_exemption(space);
    env.send(
        &[
            solana_system_interface::instruction::create_account(
                &admin,
                &mint_a.pubkey(),
                rent,
                space as u64,
                &TOKEN_2022,
            ),
            spl_token_2022_interface::instruction::initialize_permanent_delegate(
                &TOKEN_2022,
                &mint_a.pubkey(),
                &market,
            )
            .unwrap(),
            spl_token_2022_interface::instruction::initialize_mint2(
                &TOKEN_2022,
                &mint_a.pubkey(),
                &admin,
                None,
                6,
            )
            .unwrap(),
        ],
        &[&mint_a],
    )
    .unwrap();
    let mint_a = mint_a.pubkey();

    let vault_a = env.create_ata(&state, &mint_a, &TOKEN_2022);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    let creator = env.funded_keypair();
    let create = client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
        strategy_program: EVIL_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![0],
        max_creator_lamports: MAX_SETUP_LAMPORTS,
        strategy_accounts: vec![
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
        ],
    });
    env.send(
        &[
            create,
            client::init_fee_account(&admin, &market, &mint_a, &TOKEN_2022),
        ],
        &[&creator],
    )
    .unwrap();
    env.mint_to(&mint_b, &TOKEN, &vault_b, 1_000_000_000);

    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN_2022, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let accounts = client::SwapAccounts {
        trader: trader.pubkey(),
        strategy_program: EVIL_ID,
        market_state: state,
        trader_input: trader_a,
        trader_output: trader_b,
        input_vault: vault_a,
        input_mint: mint_a,
        output_mint: mint_b,
        input_token_program: TOKEN_2022,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts: vec![AccountMeta::new(vault_b, false)],
        input_hook_accounts: vec![],
    };
    let mut args = exact_in(1_000, 0);
    args.extra = vec![10]; // PullAsDelegate
    assert_fails_with(
        env.send(&[client::swap(&accounts, &args)], &[&trader]),
        "SpendLimitExceeded",
    );
    assert_eq!(env.token_balance(&trader_a), 10_000_000);
}

/// M2: creating a pool's vault accounts in advance cannot block the pool, and pools
/// with different fees are different markets.
#[test]
fn pools_cannot_be_blocked_and_fee_tiers_are_separate() {
    let mut env = Env::new();
    env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);

    // An attacker pre-creates the 30 bps pool's vault.
    let pool_30 = cp_pool(&mint_a, &mint_b, 30);
    env.create_ata(&pool_30, &mint_a, &TOKEN);

    let creator = env.funded_keypair();
    for fee in [30u16, 100] {
        let ix = cp_create_market_ix(&creator.pubkey(), &mint_a, &TOKEN, &mint_b, &TOKEN, fee);
        env.send(&[ix], &[&creator]).unwrap();
    }
    assert_ne!(
        cp_pool(&mint_a, &mint_b, 30),
        cp_pool(&mint_a, &mint_b, 100)
    );
    assert!(env
        .svm
        .get_account(&pda::market(&CP_PROGRAM_ID, &pool_30))
        .is_some());
}

/// An evil market setup with the given `info_mode`, plus extra strategy accounts.
fn evil_setup(
    env: &mut Env,
    creator: &Keypair,
    init_mode: u8,
    max_creator_lamports: u64,
    extra: Vec<AccountMeta>,
) -> Instruction {
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    let mut strategy_accounts = vec![
        AccountMeta::new_readonly(mint_a, false),
        AccountMeta::new_readonly(mint_b, false),
        AccountMeta::new(vault_a, false),
        AccountMeta::new(vault_b, false),
    ];
    strategy_accounts.extend(extra);
    client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
        strategy_program: EVIL_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![init_mode],
        max_creator_lamports,
        strategy_accounts,
    })
}

/// L1: the creator's signature never reaches the strategy. Setup spends from a router
/// payer funded with `max_creator_lamports`, and the leftover comes back.
#[test]
fn market_setup_can_spend_at_most_the_creators_allowance() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let creator = env.funded_keypair();

    // Taking 1 SOL fails when only 0.01 SOL was allowed.
    let ix = evil_setup(&mut env, &creator, 4, 10_000_000, vec![]);
    let before = env.lamports(&creator.pubkey());
    assert_fails_with(env.send(&[ix], &[&creator]), "insufficient lamports");
    assert_eq!(env.lamports(&creator.pubkey()), before);

    // With a 2 SOL allowance it succeeds, and the unused part is returned.
    let ix = evil_setup(&mut env, &creator, 4, 2 * SOL, vec![]);
    let before = env.lamports(&creator.pubkey());
    env.send(&[ix], &[&creator]).unwrap();
    let spent = before - env.lamports(&creator.pubkey());
    assert!(spent > SOL && spent < SOL + SOL / 10, "spent {spent}");
}

/// L1 (second review): repurposing the setup payer, which would let a strategy keep
/// control of it, fails the whole transaction.
#[test]
fn setup_payer_cannot_be_reassigned_or_allocated() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let creator = env.funded_keypair();
    for mode in [5u8, 6] {
        let ix = evil_setup(&mut env, &creator, mode, SOL, vec![]);
        let before = env.lamports(&creator.pubkey());
        assert_fails_with(env.send(&[ix], &[&creator]), "SetupPayerMisused");
        assert_eq!(env.lamports(&creator.pubkey()), before);
        let owner = env.svm.get_account(&creator.pubkey()).unwrap().owner;
        assert_eq!(owner, client::SYSTEM_PROGRAM_ID);
    }
}

/// L1 (second review): token accounts and wallets passed to setup lose their signer
/// flag unless they are fresh, so a strategy cannot approve the creator's tokens.
#[test]
fn setup_cannot_use_the_creators_token_accounts() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let creator = env.funded_keypair();
    let mint = env.create_mint(&TOKEN, 6, None, None);
    let creator_tokens = env.create_ata(&creator.pubkey(), &mint, &TOKEN);
    env.mint_to(&mint, &TOKEN, &creator_tokens, 1_000_000);
    let ix = evil_setup(
        &mut env,
        &creator,
        7,
        SOL,
        vec![
            AccountMeta::new(creator_tokens, false),
            AccountMeta::new_readonly(creator.pubkey(), true),
            AccountMeta::new_readonly(TOKEN, false),
        ],
    );
    assert_fails_with(env.send(&[ix], &[&creator]), "privilege escalated");
    let data = env.account_data(&creator_tokens);
    assert_eq!(data[72], 0, "no delegate was set");
}

/// L2: a constant-product pool cannot be funded on one side only, and an empty pool
/// refuses to trade instead of giving tokens away.
#[test]
fn one_sided_pools_cannot_be_funded_or_traded() {
    let mut env = Env::new();
    env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let creator = env.funded_keypair();
    env.send(
        &[cp_create_market_ix(
            &creator.pubkey(),
            &mint_a,
            &TOKEN,
            &mint_b,
            &TOKEN,
            30,
        )],
        &[&creator],
    )
    .unwrap();
    let pool = cp_pool(&mint_a, &mint_b, 30);
    let market = CpMarket {
        pool,
        market: pda::market(&CP_PROGRAM_ID, &pool),
        mint_a,
        mint_b,
        token_program_a: TOKEN,
        token_program_b: TOKEN,
        vault_a: pda::associated_token(&pool, &mint_a, &TOKEN),
        vault_b: pda::associated_token(&pool, &mint_b, &TOKEN),
    };
    let (lp, lp_a) = env.trader_with(&mint_a, &TOKEN, 1_000_000);
    let lp_b = env.create_ata(&lp.pubkey(), &mint_b, &TOKEN);
    env.mint_to(&mint_b, &TOKEN, &lp_b, 1);
    for (a, b) in [(1_000_000, 0), (0, 1)] {
        let deposit = cp_deposit_ix(&lp.pubkey(), &market, &lp_a, &lp_b, a, b, vec![]);
        assert_fails_with(env.send(&[deposit], &[&lp]), "OneSidedDeposit");
    }

    let payer = env.admin.pubkey();
    env.send(
        &[client::init_fee_account(
            &payer,
            &market.market,
            &mint_a,
            &TOKEN,
        )],
        &[],
    )
    .unwrap();
    let accounts = cp_swap_accounts(&market, &lp.pubkey(), &mint_a, &lp_a, &mint_b, &lp_b);
    assert_fails_with(
        env.send(&[client::swap(&accounts, &exact_in(1, 0))], &[&lp]),
        "InsufficientLiquidity",
    );
}

/// M3: the quoter applies the Token-2022 transfer fee for the epoch it is told, so a fee
/// change takes effect in quotes exactly when it does on chain.
#[test]
fn quoter_follows_transfer_fee_changes_by_epoch() {
    let mut env = Env::new();
    let mint_a = env.create_mint(&TOKEN_2022, 6, Some((100, u64::MAX)), None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let market = setup_cp_market_with_mints(
        &mut env,
        mint_a,
        TOKEN_2022,
        mint_b,
        TOKEN,
        1_000_000_000,
        1_000_000_000,
        30,
        vec![],
    );

    // Raise the fee to 5%; Token-2022 applies it two epochs later. Move the chain there.
    let admin = env.admin.pubkey();
    let set_fee = spl_token_2022_interface::extension::transfer_fee::instruction::set_transfer_fee(
        &TOKEN_2022,
        &mint_a,
        &admin,
        &[],
        500,
        u64::MAX,
    )
    .unwrap();
    env.send(&[set_fee], &[]).unwrap();
    let mut clock: solana_clock::Clock = env.svm.get_sysvar();
    clock.epoch += 2;
    clock.slot += 2 * 432_000;
    env.svm.set_sysvar(&clock);

    let mut quoter = Quoter::new();
    for (id, name) in [
        (client::PROGRAM_ID, "pipeline"),
        (CP_PROGRAM_ID, "test_strategy_cp"),
    ] {
        quoter
            .load_program(
                id,
                &std::fs::read(sbf_dir().join(format!("{name}.so"))).unwrap(),
            )
            .unwrap();
    }
    for key in [
        market.market,
        market.pool,
        mint_a,
        mint_b,
        market.vault_a,
        market.vault_b,
    ] {
        quoter
            .set_account(key, env.svm.get_account(&key).unwrap())
            .unwrap();
    }
    quoter.set_clock(clock.slot, clock.epoch, clock.unix_timestamp);
    let view = MarketView::from_market(
        market.market,
        &env.market(&market.market),
        vec![TOKEN_2022, TOKEN],
    )
    .unwrap();
    let request = QuoteRequest {
        input_mint: mint_a,
        output_mint: mint_b,
        mode: TradeMode::ExactIn,
        amount: 1_000_000,
        trader: None,
        integrator_fee_bps: 0,
        extra: vec![],
    };
    let quote = quoter
        .quote(&view, &request, &[AccountMeta::new(market.vault_b, false)])
        .unwrap();

    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN_2022, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &mint_a,
        &trader_a,
        &mint_b,
        &trader_b,
    );
    env.send(
        &[client::swap(
            &accounts,
            &exact_in(1_000_000, quote.amount_out),
        )],
        &[&trader],
    )
    .unwrap();
    assert_eq!(env.token_balance(&trader_b), quote.amount_out);
    assert_eq!(quote.pipeline_fee, fees::pipeline_fee(1_000_000));
}

/// L3: the quoter refuses to cache program and sysvar accounts instead of panicking.
#[test]
fn quoter_refuses_program_and_sysvar_accounts() {
    let mut quoter = Quoter::new();
    let program = Account {
        lamports: 1,
        data: vec![1, 2, 3],
        owner: client::BPF_LOADER_UPGRADEABLE_ID,
        executable: true,
        rent_epoch: 0,
    };
    assert!(quoter.set_account(Pubkey::new_unique(), program).is_err());
    let sysvar = Account {
        lamports: 1,
        data: vec![0; 8],
        owner: client::pubkey!("Sysvar1111111111111111111111111111111111111"),
        executable: false,
        rent_epoch: 0,
    };
    assert!(quoter.set_account(Pubkey::new_unique(), sysvar).is_err());
}
