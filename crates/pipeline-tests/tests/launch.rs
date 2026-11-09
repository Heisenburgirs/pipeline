//! The launch-curve strategy through the router: curve pricing with pump.fun's ratios
//! scaled to a $1B graduation, the buy that sells out the curve and graduates the
//! market into its pool, and pool trades afterwards.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Pubkey};
use pipeline_quoter::{MarketView, QuoteRequest, Quoter};
use pipeline_strategy_interface::{fees, TradeMode};
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const LAUNCH_PROGRAM_ID: Pubkey = client::pubkey!("4scUVyL9JbLjvkppBk8PYNGRPMCQfp7ppdBFkthDYPLA");

/// The quote token stands in for USDC: 6 decimals, so 1 unit is $1.
const USD: u64 = 1_000_000;
/// One billion tokens with 6 decimals.
const SUPPLY: u64 = 1_000_000_000 * USD;
/// pump.fun's shape: 793.1 million tokens sell on the curve and 206.9 million are held
/// back for the pool. pump.fun prices against 1.073 billion virtual tokens that include
/// the real ones; this strategy adds the virtual part to the real one, so the virtual
/// part is the 279.9 million left when the curve sells out.
const CURVE_SUPPLY: u64 = 793_100_000 * USD;
const RESERVED: u64 = SUPPLY - CURVE_SUPPLY;
const VIRTUAL_TOKEN: u64 = 279_900_000 * USD;
/// The virtual quote reserve that puts the curve's final price at $1.00, so the token
/// graduates at a $1B market cap: `virtual_token² / (virtual_token + curve_supply)`.
const VIRTUAL_QUOTE: u64 = 73_013_988_816_403;
const POOL_FEE_BPS: u16 = 30;

/// Mirrors the strategy's `InitParams`.
#[derive(borsh::BorshSerialize)]
struct InitParams {
    supply: u64,
    curve_supply: u64,
    virtual_quote: u64,
    virtual_token: u64,
    pool_fee_bps: u16,
}

fn params() -> InitParams {
    InitParams {
        supply: SUPPLY,
        curve_supply: CURVE_SUPPLY,
        virtual_quote: VIRTUAL_QUOTE,
        virtual_token: VIRTUAL_TOKEN,
        pool_fee_bps: POOL_FEE_BPS,
    }
}

struct LaunchMarket {
    market: Pubkey,
    launch: Pubkey,
    quote_mint: Pubkey,
    token_mint: Pubkey,
    quote_vault: Pubkey,
    token_vault: Pubkey,
}

fn launch_pda(token_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"launch", token_mint.as_ref()], &LAUNCH_PROGRAM_ID).0
}

fn launch_child(seed: &[u8], launch: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[seed, launch.as_ref()], &LAUNCH_PROGRAM_ID).0
}

fn register(env: &mut Env) {
    if env
        .svm
        .get_account(&pda::strategy(&LAUNCH_PROGRAM_ID))
        .is_none()
    {
        env.register(&LAUNCH_PROGRAM_ID, "launch_curve");
    }
}

/// Creates a market with `params` against a fresh 6-decimal quote mint. The token mint
/// is a new keypair signing `create_market`.
fn try_setup(env: &mut Env, params: &InitParams) -> Result<LaunchMarket, Box<TxResult>> {
    register(env);
    let quote_mint = env.create_mint(&TOKEN, 6, None, None);
    let token_mint = Keypair::new();
    let launch = launch_pda(&token_mint.pubkey());
    let quote_vault = launch_child(b"quote_vault", &launch);
    let token_vault = launch_child(b"token_vault", &launch);

    let creator = env.funded_keypair();
    let ix = client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
        strategy_program: LAUNCH_PROGRAM_ID,
        market_state: launch,
        market_state_signs: false,
        init_data: borsh::to_vec(params).unwrap(),
        max_creator_lamports: MAX_SETUP_LAMPORTS,
        strategy_accounts: vec![
            AccountMeta::new_readonly(quote_mint, false),
            AccountMeta::new(token_mint.pubkey(), true),
            AccountMeta::new(quote_vault, false),
            AccountMeta::new(token_vault, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(TOKEN, false),
        ],
    });
    let result = env.send(&[ix], &[&creator, &token_mint]);
    if result.is_err() {
        return Err(Box::new(result));
    }

    let market = pda::market(&LAUNCH_PROGRAM_ID, &launch);
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &market, &quote_mint, &TOKEN),
            client::init_fee_account(&payer, &market, &token_mint.pubkey(), &TOKEN),
        ],
        &[],
    )
    .expect("init fee accounts");

    Ok(LaunchMarket {
        market,
        launch,
        quote_mint,
        token_mint: token_mint.pubkey(),
        quote_vault,
        token_vault,
    })
}

fn setup(env: &mut Env) -> LaunchMarket {
    try_setup(env, &params()).unwrap_or_else(|_| panic!("create_market"))
}

/// The launch account's `(graduated, quote_reserve, token_reserve, graduation_slot)`.
fn state(env: &Env, m: &LaunchMarket) -> (bool, u64, u64, u64) {
    let data = env.account_data(&m.launch);
    let at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
    // 8 discriminator + 4 pubkeys + 4 u64 + u16 = 170.
    (data[170] == 1, at(171), at(179), at(187))
}

fn reserves(env: &Env, m: &LaunchMarket) -> (u64, u64) {
    let (_, quote, token, _) = state(env, m);
    (quote, token)
}

/// `floor(reserve_out × in / (reserve_in + in))`.
fn out_given_in(reserve_in: u128, reserve_out: u128, amount_in: u64) -> u64 {
    (reserve_out * amount_in as u128 / (reserve_in + amount_in as u128)) as u64
}

/// `ceil(x × out / (y − out))`: the quote that buys exactly `out` from the curve.
fn cost_for(x: u128, y: u128, out: u64) -> u64 {
    let numerator = x * out as u128;
    let denominator = y - out as u128;
    numerator.div_ceil(denominator) as u64
}

fn curve_xy(quote_reserve: u64, token_reserve: u64) -> (u128, u128) {
    (
        VIRTUAL_QUOTE as u128 + quote_reserve as u128,
        VIRTUAL_TOKEN as u128 + token_reserve as u128,
    )
}

/// A pool trade: the fee rounds up and comes off the input.
fn pool_out(reserve_in: u64, reserve_out: u64, amount_in: u64) -> u64 {
    let net = amount_in - fees::ceil_bps(amount_in, POOL_FEE_BPS as u64).unwrap();
    out_given_in(reserve_in as u128, reserve_out as u128, net).min(reserve_out - 1)
}

/// What a buy of `amount` gives, mirroring the strategy across graduation.
fn price_buy(env: &Env, m: &LaunchMarket, amount: u64) -> u64 {
    let (graduated, quote_reserve, token_reserve, _) = state(env, m);
    if graduated {
        return pool_out(quote_reserve, token_reserve, amount);
    }
    let (x, y) = curve_xy(quote_reserve, token_reserve);
    let out = out_given_in(x, y, amount);
    if out < token_reserve {
        return out;
    }
    let cost = cost_for(x, y, token_reserve);
    let rest = amount - cost;
    let pool = if rest > 0 {
        pool_out(quote_reserve + cost, RESERVED, rest)
    } else {
        0
    };
    token_reserve + pool
}

fn price_sell(env: &Env, m: &LaunchMarket, amount: u64) -> u64 {
    let (graduated, quote_reserve, token_reserve, _) = state(env, m);
    if graduated {
        return pool_out(token_reserve, quote_reserve, amount);
    }
    let (x, y) = curve_xy(quote_reserve, token_reserve);
    out_given_in(y, x, amount).min(quote_reserve)
}

fn swap_accounts(
    m: &LaunchMarket,
    trader: &Pubkey,
    buy: bool,
    trader_quote: &Pubkey,
    trader_token: &Pubkey,
) -> client::SwapAccounts {
    let (input_mint, output_mint, trader_input, trader_output, input_vault, output_vault) = if buy {
        (
            m.quote_mint,
            m.token_mint,
            *trader_quote,
            *trader_token,
            m.quote_vault,
            m.token_vault,
        )
    } else {
        (
            m.token_mint,
            m.quote_mint,
            *trader_token,
            *trader_quote,
            m.token_vault,
            m.quote_vault,
        )
    };
    client::SwapAccounts {
        trader: *trader,
        strategy_program: LAUNCH_PROGRAM_ID,
        market_state: m.launch,
        trader_input,
        trader_output,
        input_vault,
        input_mint,
        output_mint,
        input_token_program: TOKEN,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts: vec![AccountMeta::new(output_vault, false)],
        input_hook_accounts: vec![],
    }
}

/// A funded trader holding `quote` of the quote token, with an empty token account.
fn trader(env: &mut Env, m: &LaunchMarket, quote: u64) -> (Keypair, Pubkey, Pubkey) {
    let (trader, trader_quote) = env.trader_with(&m.quote_mint, &TOKEN, quote);
    let trader_token = env.create_ata(&trader.pubkey(), &m.token_mint, &TOKEN);
    (trader, trader_quote, trader_token)
}

/// Buys `amount` through the router, checking the strategy against the mirror and the
/// quoter. Returns the tokens received.
fn buy(env: &mut Env, m: &LaunchMarket, t: &(Keypair, Pubkey, Pubkey), amount: u64) -> u64 {
    let expected = price_buy(env, m, amount);
    assert_eq!(quoted(env, m, true, amount), expected, "quoter on a buy");
    let before = env.token_balance(&t.2);
    let accounts = swap_accounts(m, &t.0.pubkey(), true, &t.1, &t.2);
    env.send(
        &[client::swap(&accounts, &exact_in(amount, expected))],
        &[&t.0],
    )
    .expect("buy");
    let got = env.token_balance(&t.2) - before;
    assert_eq!(got, expected, "buy output");
    got
}

fn sell(env: &mut Env, m: &LaunchMarket, t: &(Keypair, Pubkey, Pubkey), amount: u64) -> u64 {
    let expected = price_sell(env, m, amount);
    assert_eq!(quoted(env, m, false, amount), expected, "quoter on a sell");
    let before = env.token_balance(&t.1);
    let accounts = swap_accounts(m, &t.0.pubkey(), false, &t.1, &t.2);
    env.send(
        &[client::swap(&accounts, &exact_in(amount, expected))],
        &[&t.0],
    )
    .expect("sell");
    let got = env.token_balance(&t.1) - before;
    assert_eq!(got, expected, "sell output");
    got
}

// ------------------------------------------------------------------- the quoter

fn quoter_for(env: &Env, m: &LaunchMarket) -> Quoter {
    let mut quoter = Quoter::new();
    for (id, name) in [
        (client::PROGRAM_ID, "pipeline"),
        (LAUNCH_PROGRAM_ID, "launch_curve"),
    ] {
        let elf = std::fs::read(sbf_dir().join(format!("{name}.so"))).unwrap();
        quoter.load_program(id, &elf).unwrap();
    }
    for key in [
        pda::config(),
        m.market,
        m.launch,
        m.quote_mint,
        m.token_mint,
        m.quote_vault,
        m.token_vault,
        pda::fee_account(&m.market, &m.quote_mint, &TOKEN),
        pda::fee_account(&m.market, &m.token_mint, &TOKEN),
    ] {
        if let Some(account) = env.svm.get_account(&key) {
            quoter.set_account(key, account).unwrap();
        }
    }
    quoter
}

fn view(env: &Env, m: &LaunchMarket) -> MarketView {
    MarketView::from_market(m.market, &env.market(&m.market), vec![TOKEN, TOKEN]).unwrap()
}

/// The quoter's answer for an exact-in trade, after checking it resolves only the
/// output vault.
fn quoted(env: &Env, m: &LaunchMarket, buy: bool, amount: u64) -> u64 {
    let (input_mint, output_mint, output_vault) = if buy {
        (m.quote_mint, m.token_mint, m.token_vault)
    } else {
        (m.token_mint, m.quote_mint, m.quote_vault)
    };
    let request = QuoteRequest {
        input_mint,
        output_mint,
        mode: TradeMode::ExactIn,
        amount,
        trader: None,
        integrator_fee_bps: 0,
        extra: vec![],
    };
    let mut quoter = quoter_for(env, m);
    let resolved = quoter
        .resolve_accounts(&view(env, m), &request, |_, _| {})
        .unwrap();
    assert_eq!(resolved, vec![AccountMeta::new(output_vault, false)]);
    quoter
        .quote(&view(env, m), &request, &resolved)
        .unwrap()
        .amount_out
}

// ------------------------------------------------------------------------ tests

#[test]
fn market_creation_fixes_the_supply_and_holds_back_the_pool_share() {
    let mut env = Env::new();
    let m = setup(&mut env);

    assert_eq!(env.token_balance(&m.token_vault), SUPPLY);
    assert_eq!(env.token_balance(&m.quote_vault), 0);
    assert_eq!(state(&env, &m), (false, 0, CURVE_SUPPLY, 0));

    // The mint authority is gone and the supply is exactly what the launch holds.
    let mint = env.account_data(&m.token_mint);
    assert_eq!(
        u32::from_le_bytes(mint[..4].try_into().unwrap()),
        0,
        "mint authority"
    );
    assert_eq!(u64::from_le_bytes(mint[36..44].try_into().unwrap()), SUPPLY);
    assert_eq!(mint[44], 6);

    let market = env.market(&m.market);
    assert_eq!(market.mints(), &[m.quote_mint, m.token_mint]);
    assert_eq!(&market.vaults[..2], &[m.quote_vault, m.token_vault]);
    assert!(!market.supports_exact_out);
}

#[test]
fn bad_parameters_are_refused() {
    let mut env = Env::new();
    let refused = |env: &mut Env, p: InitParams| {
        let failed = try_setup(env, &p).err().expect("create_market should fail");
        assert_fails_with(*failed, "BadInitData");
    };
    // Nothing held back for the pool.
    refused(
        &mut env,
        InitParams {
            curve_supply: SUPPLY,
            ..params()
        },
    );
    // The curve would hit an infinite price as it sells out.
    refused(
        &mut env,
        InitParams {
            virtual_token: 0,
            ..params()
        },
    );
    refused(
        &mut env,
        InitParams {
            pool_fee_bps: 10_000,
            ..params()
        },
    );
}

#[test]
fn curve_trades_follow_the_curve_and_match_quotes() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 1_000_000 * USD);

    // The opening price is virtual_quote / (virtual_token + curve_supply) ≈ $0.068, a
    // $68M market cap.
    let amount = 1_000 * USD;
    let got = buy(&mut env, &m, &t, amount);
    let opening_price = VIRTUAL_QUOTE as f64 / (VIRTUAL_TOKEN + CURVE_SUPPLY) as f64;
    let paid_per_token = amount as f64 / got as f64;
    assert!((paid_per_token / opening_price - 1.0).abs() < 1e-3);
    assert_eq!(reserves(&env, &m), (amount, CURVE_SUPPLY - got));
    assert_eq!(
        env.token_balance(&t.1),
        1_000_000 * USD - amount - fees::pipeline_fee(amount)
    );

    // Sell half back; Pipeline's fee on a sell is taken in the token.
    let half = got / 2;
    let back = sell(&mut env, &m, &t, half);
    assert!(back > 0 && back < amount);
    assert_eq!(
        env.token_balance(&t.2),
        got - half - fees::pipeline_fee(half)
    );
    assert_eq!(
        reserves(&env, &m),
        (amount - back, CURVE_SUPPLY - got + half)
    );
    assert!(!state(&env, &m).0);

    // Exact-out is refused by the router because the market does not support it.
    let accounts = swap_accounts(&m, &t.0.pubkey(), true, &t.1, &t.2);
    let args = client::SwapArgs {
        mode: TradeMode::ExactOut,
        amount_in: amount,
        amount_out: 1_000,
        ..exact_in(amount, 0)
    };
    assert_fails_with(
        env.send(&[client::swap(&accounts, &args)], &[&t.0]),
        "ExactOutUnsupported",
    );
}

#[test]
fn the_buy_that_sells_out_the_curve_graduates_it_into_the_pool() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 300_000_000 * USD);

    // Most of the curve goes in one buy; it does not graduate.
    buy(&mut env, &m, &t, 150_000_000 * USD);
    let (graduated, quote_reserve, left, _) = state(&env, &m);
    assert!(!graduated && left > 0);

    // The buy that takes the rest: it pays exactly for the remaining curve tokens and
    // spends the remaining $1M on the new pool.
    let (x, y) = curve_xy(quote_reserve, left);
    let cost = cost_for(x, y, left);
    let extra = 1_000_000 * USD;
    let raise = quote_reserve + cost;
    let pool_leg = pool_out(raise, RESERVED, extra);
    assert!(pool_leg > 0);
    assert_eq!(price_buy(&env, &m, cost + extra), left + pool_leg);

    let before = env.token_balance(&t.2);
    let accounts = swap_accounts(&m, &t.0.pubkey(), true, &t.1, &t.2);
    let meta = env
        .send(
            &[client::swap(
                &accounts,
                &exact_in(cost + extra, left + pool_leg),
            )],
            &[&t.0],
        )
        .expect("graduating buy");
    assert_eq!(env.token_balance(&t.2) - before, left + pool_leg);
    let slot = env.svm.get_sysvar::<solana_clock::Clock>().slot;
    assert_eq!(
        state(&env, &m),
        (true, raise + extra, RESERVED - pool_leg, slot)
    );
    assert_eq!(env.token_balance(&m.token_vault), RESERVED - pool_leg);
    assert_eq!(env.token_balance(&m.quote_vault), raise + extra);
    // The strategy logs its Graduated event.
    assert!(meta.logs.iter().any(|l| l.starts_with("Program data:")));

    // The raise is pump.fun's share of the graduation market cap: about 20.7% of $1B.
    let raise_usd = raise as f64 / USD as f64;
    assert!(
        (raise_usd / 206_886_011.0 - 1.0).abs() < 1e-4,
        "{raise_usd}"
    );
    // The curve's final price is $1.00 (a $1B market cap on 1B tokens), and the pool
    // opens at that same price because the held-back tokens are worth the raise.
    let final_price = (x + cost as u128) as f64 / (y - left as u128) as f64;
    let pool_price = raise as f64 / RESERVED as f64;
    assert!((final_price - 1.0).abs() < 1e-3, "{final_price}");
    assert!(
        (pool_price / final_price - 1.0).abs() < 1e-3,
        "{pool_price}"
    );
}

#[test]
fn a_buy_for_exactly_the_remaining_cost_graduates_with_no_pool_leg() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 300_000_000 * USD);

    let (x, y) = curve_xy(0, CURVE_SUPPLY);
    let cost = cost_for(x, y, CURVE_SUPPLY);

    // One unit short leaves at least one curve token and does not graduate.
    let short = price_buy(&env, &m, cost - 1);
    assert!(short < CURVE_SUPPLY);
    assert_eq!(quoted(&env, &m, true, cost - 1), short);

    let got = buy(&mut env, &m, &t, cost);
    assert_eq!(got, CURVE_SUPPLY);
    let (graduated, quote_reserve, token_reserve, _) = state(&env, &m);
    assert!(graduated);
    assert_eq!((quote_reserve, token_reserve), (cost, RESERVED));
    assert_eq!(env.token_balance(&m.token_vault), RESERVED);
}

#[test]
fn after_graduation_trades_follow_the_pool_and_never_return_to_the_curve() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let whale = trader(&mut env, &m, 300_000_000 * USD);
    let (x, y) = curve_xy(0, CURVE_SUPPLY);
    buy(&mut env, &m, &whale, cost_for(x, y, CURVE_SUPPLY));
    let (_, pool_quote, pool_token, _) = state(&env, &m);
    assert!(state(&env, &m).0);

    // A pool buy: constant product on real reserves, 0.3% fee kept by the pool.
    let t = trader(&mut env, &m, 10_000_000 * USD);
    let amount = 1_000_000 * USD;
    let got = buy(&mut env, &m, &t, amount);
    assert_eq!(got, pool_out(pool_quote, pool_token, amount));
    // Just over $1 per token, since the pool opened at parity.
    let paid = amount as f64 / got as f64;
    assert!(paid > 1.0 && paid < 1.02, "{paid}");
    assert_eq!(reserves(&env, &m), (pool_quote + amount, pool_token - got));

    // Selling nearly everything back (Pipeline takes its fee on a sell in the token, so
    // the trader keeps a little) leaves the pool richer and still graduated.
    let sold = got * 9_900 / 10_000;
    let back = sell(&mut env, &m, &t, sold);
    assert!(back < amount);
    let (graduated, quote_after, token_after, _) = state(&env, &m);
    assert!(graduated);
    assert_eq!(
        (quote_after, token_after),
        (pool_quote + amount - back, pool_token - got + sold)
    );
    assert_eq!(env.token_balance(&m.quote_vault), quote_after);

    // The whale dumping most of the supply moves the pool, never the curve: the sell is
    // priced on real reserves alone.
    let dump = CURVE_SUPPLY / 2;
    let out = sell(&mut env, &m, &whale, dump);
    assert_eq!(out, pool_out(token_after, quote_after, dump));
    assert!(state(&env, &m).0);
}
