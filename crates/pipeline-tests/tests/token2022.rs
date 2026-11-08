//! Swaps involving Token-2022 transfer fees and transfer hooks.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_strategy_interface::fees;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const HOOK_PROGRAM_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ3");

/// Token-2022 transfer fee: `ceil(amount × bps / 10,000)`, capped at `max`.
fn transfer_fee(amount: u64, bps: u16, max: u64) -> u64 {
    fees::ceil_bps(amount, bps as u64).unwrap().min(max)
}

fn validation(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], &HOOK_PROGRAM_ID).0
}

fn whitelist(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"whitelist", mint.as_ref()], &HOOK_PROGRAM_ID).0
}

/// Creates a Token-2022 mint whose hook allows everything (`allowed` empty) or only
/// transfers to or from the `allowed` token accounts.
fn hooked_mint(env: &mut Env, allowed: &[Pubkey]) -> Pubkey {
    if env.svm.get_account(&HOOK_PROGRAM_ID).is_none() {
        env.svm
            .add_program_from_file(HOOK_PROGRAM_ID, sbf_dir().join("test_hook.so"))
            .unwrap();
    }
    let mint = env.create_mint(&TOKEN_2022, 6, None, Some(HOOK_PROGRAM_ID));
    let mut data = b"tsthkini".to_vec();
    for key in allowed {
        data.extend_from_slice(key.as_ref());
    }
    let ix = Instruction::new_with_bytes(
        HOOK_PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new(env.admin.pubkey(), true),
            AccountMeta::new(validation(&mint), false),
            AccountMeta::new(whitelist(&mint), false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    );
    env.send(&[ix], &[]).expect("hook initialize");
    mint
}

/// The accounts Token-2022 needs to run `mint`'s hook.
fn hook_accounts(mint: &Pubkey, restricted: bool) -> Vec<AccountMeta> {
    let mut accounts = vec![
        AccountMeta::new_readonly(HOOK_PROGRAM_ID, false),
        AccountMeta::new_readonly(validation(mint), false),
    ];
    if restricted {
        accounts.push(AccountMeta::new_readonly(whitelist(mint), false));
    }
    accounts
}

#[test]
fn transfer_fee_on_input_token_charges_trader_on_what_they_sent() {
    let mut env = Env::new();
    let (bps, max) = (100u16, u64::MAX);
    let mint_a = env.create_mint(&TOKEN_2022, 6, Some((bps, max)), None);
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
    let reserve_a = env.token_balance(&market.vault_a);
    let reserve_b = env.token_balance(&market.vault_b);
    assert_eq!(
        reserve_a,
        1_000_000_000 - transfer_fee(1_000_000_000, bps, max)
    );

    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN_2022, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let amount = 1_000_000;
    let arrived = amount - transfer_fee(amount, bps, max);
    let expected_out = cp_out(reserve_a, reserve_b, arrived, 30);
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &mint_a,
        &trader_a,
        &mint_b,
        &trader_b,
    );
    let meta = env
        .send(
            &[client::swap(&accounts, &exact_in(amount, expected_out))],
            &[&trader],
        )
        .unwrap();

    // The trader pays 0.25% of what they sent; Pipeline receives that minus the token's cut.
    let pipeline_fee = fees::pipeline_fee(amount);
    assert_eq!(
        env.token_balance(&trader_a),
        10_000_000 - amount - pipeline_fee
    );
    assert_eq!(env.token_balance(&trader_b), expected_out);
    let fee_account = pda::fee_account(&market.market, &mint_a, &TOKEN_2022);
    assert_eq!(
        env.token_balance(&fee_account),
        pipeline_fee - transfer_fee(pipeline_fee, bps, max)
    );
    let event = &swap_events(&meta)[0];
    assert_eq!((event.net_sent, event.pipeline_fee), (amount, pipeline_fee));
}

#[test]
fn transfer_fee_on_output_token_is_reflected_in_received() {
    let mut env = Env::new();
    let (bps, max) = (250u16, u64::MAX);
    let mint_a = env.create_mint(&TOKEN_2022, 6, Some((bps, max)), None);
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
    let reserve_a = env.token_balance(&market.vault_a);
    let reserve_b = env.token_balance(&market.vault_b);

    let (trader, trader_b) = env.trader_with(&mint_b, &TOKEN, 10_000_000);
    let trader_a = env.create_ata(&trader.pubkey(), &mint_a, &TOKEN_2022);
    let sent_by_pool = cp_out(reserve_b, reserve_a, 1_000_000, 30);
    let arrives = sent_by_pool - transfer_fee(sent_by_pool, bps, max);
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &mint_b,
        &trader_b,
        &mint_a,
        &trader_a,
    );

    // A minimum above what actually arrives fails even though the pool sent enough.
    let too_strict = client::swap(&accounts, &exact_in(1_000_000, sent_by_pool));
    assert_fails_with(env.send(&[too_strict], &[&trader]), "InsufficientOutput");

    let meta = env
        .send(
            &[client::swap(&accounts, &exact_in(1_000_000, arrives))],
            &[&trader],
        )
        .unwrap();
    assert_eq!(env.token_balance(&trader_a), arrives);
    assert_eq!(swap_events(&meta)[0].received, arrives);
}

#[test]
fn allow_all_hook_works_in_both_directions() {
    let mut env = Env::new();
    let mint_h = hooked_mint(&mut env, &[]);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let market = setup_cp_market_with_mints(
        &mut env,
        mint_h,
        TOKEN_2022,
        mint_b,
        TOKEN,
        1_000_000_000,
        1_000_000_000,
        30,
        hook_accounts(&mint_h, false),
    );

    // Selling the hooked token: the router's transfers need the hook accounts.
    let (trader, trader_h) = env.trader_with(&mint_h, &TOKEN_2022, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let mut accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &mint_h,
        &trader_h,
        &mint_b,
        &trader_b,
    );
    accounts.input_hook_accounts = hook_accounts(&mint_h, false);
    env.send(
        &[client::swap(&accounts, &exact_in(1_000_000, 1))],
        &[&trader],
    )
    .unwrap();
    let fee_account = pda::fee_account(&market.market, &mint_h, &TOKEN_2022);
    assert_eq!(
        env.token_balance(&fee_account),
        fees::pipeline_fee(1_000_000)
    );

    // Buying it: the strategy's output transfer needs them, passed after the vault.
    let mut accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &mint_b,
        &trader_b,
        &mint_h,
        &trader_h,
    );
    accounts
        .strategy_accounts
        .extend(hook_accounts(&mint_h, false));
    let before = env.token_balance(&trader_h);
    env.send(
        &[client::swap(&accounts, &exact_in(500_000, 1))],
        &[&trader],
    )
    .unwrap();
    assert!(env.token_balance(&trader_h) > before);
}

#[test]
fn restrictive_hook_blocks_selling_unless_the_fee_account_is_whitelisted() {
    let mut env = Env::new();
    let mint_b = env.create_mint(&TOKEN, 6, None, None);

    // Whitelist only the pool's vault, as a token with a whitelist hook might.
    let make = |env: &mut Env, include_fee_account: bool| {
        // Create the mint first, then initialise its hook with a whitelist that names
        // addresses derived from it.
        if env.svm.get_account(&HOOK_PROGRAM_ID).is_none() {
            env.svm
                .add_program_from_file(HOOK_PROGRAM_ID, sbf_dir().join("test_hook.so"))
                .unwrap();
        }
        let mint_h = env.create_mint(&TOKEN_2022, 6, None, Some(HOOK_PROGRAM_ID));
        let pool = cp_pool(&mint_h, &mint_b, 30);
        let market = pda::market(&CP_PROGRAM_ID, &pool);
        let mut allowed = vec![pda::associated_token(&pool, &mint_h, &TOKEN_2022)];
        if include_fee_account {
            allowed.push(pda::fee_account(&market, &mint_h, &TOKEN_2022));
        }
        let mut data = b"tsthkini".to_vec();
        for key in &allowed {
            data.extend_from_slice(key.as_ref());
        }
        let ix = Instruction::new_with_bytes(
            HOOK_PROGRAM_ID,
            &data,
            vec![
                AccountMeta::new(env.admin.pubkey(), true),
                AccountMeta::new(validation(&mint_h), false),
                AccountMeta::new(whitelist(&mint_h), false),
                AccountMeta::new_readonly(mint_h, false),
                AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
            ],
        );
        env.send(&[ix], &[]).unwrap();
        let market = setup_cp_market_with_mints(
            env,
            mint_h,
            TOKEN_2022,
            mint_b,
            TOKEN,
            1_000_000_000,
            1_000_000_000,
            30,
            hook_accounts(&mint_h, true),
        );
        (mint_h, market)
    };

    let (mint_h, market) = make(&mut env, false);
    let (trader, trader_h) = env.trader_with(&mint_h, &TOKEN_2022, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);

    // Buying works: the vault is whitelisted as the source.
    let mut buy = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &mint_b,
        &trader_b,
        &mint_h,
        &trader_h,
    );
    env.mint_to(&mint_b, &TOKEN, &trader_b, 2_000_000);
    buy.strategy_accounts.extend(hook_accounts(&mint_h, true));
    env.send(&[client::swap(&buy, &exact_in(1_000_000, 1))], &[&trader])
        .unwrap();

    // Selling fails: the fee transfer goes from the trader to a non-whitelisted account.
    let mut sell = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &mint_h,
        &trader_h,
        &mint_b,
        &trader_b,
    );
    sell.input_hook_accounts = hook_accounts(&mint_h, true);
    assert_fails_with(
        env.send(&[client::swap(&sell, &exact_in(1_000_000, 1))], &[&trader]),
        "neither source nor destination is whitelisted",
    );

    // With the market's fee account on the whitelist, selling works.
    let (mint_h2, market2) = make(&mut env, true);
    let (trader2, trader2_h) = env.trader_with(&mint_h2, &TOKEN_2022, 10_000_000);
    let trader2_b = env.create_ata(&trader2.pubkey(), &mint_b, &TOKEN);
    let mut sell = cp_swap_accounts(
        &market2,
        &trader2.pubkey(),
        &mint_h2,
        &trader2_h,
        &mint_b,
        &trader2_b,
    );
    sell.input_hook_accounts = hook_accounts(&mint_h2, true);
    env.send(&[client::swap(&sell, &exact_in(1_000_000, 1))], &[&trader2])
        .unwrap();
}
