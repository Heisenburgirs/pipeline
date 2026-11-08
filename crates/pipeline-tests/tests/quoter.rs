//! The embedded-VM quoter must agree exactly with real execution through the router.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Pubkey};
use pipeline_quoter::{MarketView, QuoteError, QuoteRequest, Quoter, TraderQuote};
use pipeline_strategy_interface::TradeMode;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

/// A quoter loaded with the router and CP strategy, mirroring `keys` from the chain.
fn quoter_for(env: &Env, keys: &[Pubkey]) -> Quoter {
    let mut quoter = Quoter::new();
    for (id, name) in [
        (client::PROGRAM_ID, "pipeline"),
        (CP_PROGRAM_ID, "test_strategy_cp"),
        (EVIL_ID, "test_strategy_evil"),
    ] {
        let elf = std::fs::read(sbf_dir().join(format!("{name}.so"))).unwrap();
        quoter.load_program(id, &elf).unwrap();
    }
    sync(&mut quoter, env, keys);
    quoter
}

/// Copies the current state of `keys` from the chain into the quoter, as a stream would.
fn sync(quoter: &mut Quoter, env: &Env, keys: &[Pubkey]) {
    for key in keys {
        if let Some(account) = env.svm.get_account(key) {
            quoter.set_account(*key, account).unwrap();
        }
    }
}

fn market_keys(market: &CpMarket) -> Vec<Pubkey> {
    vec![
        pda::config(),
        market.market,
        market.pool,
        market.mint_a,
        market.mint_b,
        market.vault_a,
        market.vault_b,
        pda::fee_account(&market.market, &market.mint_a, &market.token_program_a),
        pda::fee_account(&market.market, &market.mint_b, &market.token_program_b),
    ]
}

fn view(env: &Env, market: &CpMarket) -> MarketView {
    MarketView::from_market(
        market.market,
        &env.market(&market.market),
        vec![market.token_program_a, market.token_program_b],
    )
    .unwrap()
}

fn request(input: Pubkey, output: Pubkey, mode: TradeMode, amount: u64) -> QuoteRequest {
    QuoteRequest {
        input_mint: input,
        output_mint: output,
        mode,
        amount,
        trader: None,
        integrator_fee_bps: 0,
        extra: vec![],
    }
}

/// Executes the quoted trade for a fresh trader and returns (spent, received).
fn execute(
    env: &mut Env,
    market: &CpMarket,
    input: Pubkey,
    output: Pubkey,
    mode: TradeMode,
    quote: &TraderQuote,
    min_out: u64,
) -> (u64, u64) {
    let input_program = market.token_program(&input);
    let output_program = market.token_program(&output);
    let (trader, trader_in) = env.trader_with(&input, &input_program, quote.total_spend * 2 + 10);
    let trader_out = env.create_ata(&trader.pubkey(), &output, &output_program);
    let before = env.token_balance(&trader_in);
    let accounts = cp_swap_accounts(
        market,
        &trader.pubkey(),
        &input,
        &trader_in,
        &output,
        &trader_out,
    );
    let args = client::SwapArgs {
        mode,
        amount_in: quote.amount_in,
        amount_out: quote.strategy_amount_out,
        min_amount_out: min_out,
        integrator_fee_bps: 0,
        input_hook_accounts: 0,
        extra: vec![],
    };
    env.send(&[client::swap(&accounts, &args)], &[&trader])
        .expect("swap");
    (
        before - env.token_balance(&trader_in),
        env.token_balance(&trader_out),
    )
}

fn check_market(mut env: Env, market: CpMarket) {
    let keys = market_keys(&market);
    let view = view(&env, &market);
    let strategy_accounts_for =
        |output: &Pubkey| vec![AccountMeta::new(market.vault(output), false)];

    for (input, output) in [
        (market.mint_a, market.mint_b),
        (market.mint_b, market.mint_a),
    ] {
        for amount in [1u64, 999, 1_000_000, 37_000_003] {
            let mut quoter = quoter_for(&env, &keys);
            let resolved = quoter
                .resolve_accounts(
                    &view,
                    &request(input, output, TradeMode::ExactIn, amount),
                    |_, _| {},
                )
                .unwrap();
            assert_eq!(resolved, strategy_accounts_for(&output));

            let quote = quoter
                .quote(
                    &view,
                    &request(input, output, TradeMode::ExactIn, amount),
                    &resolved,
                )
                .unwrap();
            let simulated = quoter
                .simulate_swap(
                    &cp_swap_accounts(
                        &market,
                        &Pubkey::new_unique(),
                        &input,
                        &Pubkey::default(),
                        &output,
                        &Pubkey::default(),
                    ),
                    &exact_in(quote.amount_in, 0),
                )
                .unwrap();
            assert_eq!(
                (simulated.net_sent, simulated.received),
                (quote.net_sent, quote.amount_out)
            );
            assert_eq!(simulated.pipeline_fee, quote.pipeline_fee);

            let (spent, received) = execute(
                &mut env,
                &market,
                input,
                output,
                TradeMode::ExactIn,
                &quote,
                quote.amount_out,
            );
            assert_eq!(
                (spent, received),
                (quote.total_spend, quote.amount_out),
                "exact-in {amount}"
            );
        }

        // Exact-out: the trader receives exactly what they asked for and spends the quote.
        for want in [1u64, 12_345, 2_000_000] {
            let mut quoter = quoter_for(&env, &keys);
            let quote = quoter
                .quote(
                    &view,
                    &request(input, output, TradeMode::ExactOut, want),
                    &strategy_accounts_for(&output),
                )
                .unwrap();
            assert_eq!(quote.amount_out, want);
            let (spent, received) = execute(
                &mut env,
                &market,
                input,
                output,
                TradeMode::ExactOut,
                &quote,
                want,
            );
            assert_eq!(
                (spent, received),
                (quote.total_spend, want),
                "exact-out {want}"
            );
        }
    }
}

#[test]
fn quotes_match_execution_for_spl_tokens() {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, 5_000_000_000, 7_000_000_000, 30);
    check_market(env, market);
}

#[test]
fn quotes_match_execution_with_token_2022_transfer_fees() {
    let mut env = Env::new();
    let mint_a = env.create_mint(&TOKEN_2022, 6, Some((150, u64::MAX)), None);
    let mint_b = env.create_mint(&TOKEN_2022, 9, Some((37, 5_000)), None);
    let market = setup_cp_market_with_mints(
        &mut env,
        mint_a,
        TOKEN_2022,
        mint_b,
        TOKEN_2022,
        5_000_000_000,
        7_000_000_000,
        30,
        vec![],
    );
    check_market(env, market);
}

#[test]
fn integrator_fee_is_included_in_quoted_spend() {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, 5_000_000_000, 7_000_000_000, 30);
    let mut quoter = quoter_for(&env, &market_keys(&market));
    let mut req = request(market.mint_a, market.mint_b, TradeMode::ExactIn, 1_000_000);
    req.integrator_fee_bps = 100;
    let quote = quoter
        .quote(
            &view(&env, &market),
            &req,
            &[AccountMeta::new(market.vault_b, false)],
        )
        .unwrap();
    assert_eq!(quote.integrator_fee, 10_000);
    assert_eq!(quote.total_spend, 1_000_000 + 2_500 + 10_000);
}

#[test]
fn quotes_follow_state_updates() {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, 5_000_000_000, 7_000_000_000, 30);
    let keys = market_keys(&market);
    let view = view(&env, &market);
    let accounts = [AccountMeta::new(market.vault_b, false)];
    let req = request(market.mint_a, market.mint_b, TradeMode::ExactIn, 1_000_000);

    let mut quoter = quoter_for(&env, &keys);
    let first = quoter.quote(&view, &req, &accounts).unwrap();
    execute(
        &mut env,
        &market,
        market.mint_a,
        market.mint_b,
        TradeMode::ExactIn,
        &first,
        0,
    );

    // Stale state gives the old price; syncing gives the new one, which matches execution.
    assert_eq!(quoter.quote(&view, &req, &accounts).unwrap(), first);
    sync(&mut quoter, &env, &keys);
    let second = quoter.quote(&view, &req, &accounts).unwrap();
    assert!(second.amount_out < first.amount_out);
    let (_, received) = execute(
        &mut env,
        &market,
        market.mint_a,
        market.mint_b,
        TradeMode::ExactIn,
        &second,
        0,
    );
    assert_eq!(received, second.amount_out);
}

#[test]
fn hostile_quotes_are_contained() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    let creator = env.funded_keypair();
    let ix = client::create_market(&client::CreateMarket {
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
    env.send(&[ix], &[&creator]).unwrap();
    let market_address = pda::market(&EVIL_ID, &state);
    let view = MarketView::from_market(
        market_address,
        &env.market(&market_address),
        vec![TOKEN, TOKEN],
    )
    .unwrap();
    let mut quoter = quoter_for(
        &env,
        &[market_address, state, mint_a, mint_b, vault_a, vault_b],
    );

    // A strategy that quotes forever is stopped by the compute cap.
    let mut req = request(mint_a, mint_b, TradeMode::ExactIn, 1_000);
    req.extra = vec![8];
    let started = std::time::Instant::now();
    let err = quoter
        .quote(&view, &req, &[AccountMeta::new(vault_b, false)])
        .unwrap_err();
    assert!(matches!(err, QuoteError::StrategyFailed(_)), "{err}");
    assert!(started.elapsed().as_secs() < 5);

    // A strategy that over-quotes is caught by the trader's minimum at execution.
    req.extra = vec![7];
    let quote = quoter
        .quote(&view, &req, &[AccountMeta::new(vault_b, false)])
        .unwrap();
    assert_eq!(quote.amount_out, 2_000);
}
