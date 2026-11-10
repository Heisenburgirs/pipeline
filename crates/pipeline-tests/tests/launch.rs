//! The launch-curve strategy through the router: curve pricing with pump.fun's ratios
//! scaled to a $1B graduation, the buy that sells out the curve and graduates the
//! market into its pool, and pool trades afterwards.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
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
/// The default preset from docs/LAUNCH-CURVE.md: a $100k start, a $1B graduation
/// (10,000x, so `r = 0.01`), and the held-back tokens worth exactly the raise at the
/// final $1.00 price. `curve_supply = supply / 1.01`, `virtual_token = curve_supply / 99`,
/// `virtual_quote = virtual_token / 100`.
const CURVE_SUPPLY: u64 = 990_099_009_900_990;
const RESERVED: u64 = SUPPLY - CURVE_SUPPLY;
const VIRTUAL_TOKEN: u64 = 10_001_000_100_010;
const VIRTUAL_QUOTE: u64 = 100_010_001_000;
const POOL_FEE_BPS: u16 = 30;
/// 0.3% of every trade to the market's creator, 0.2% to the program's upgrade
/// authority (compiled into the program).
const CREATOR_FEE_BPS: u16 = 30;
const PROTOCOL_FEE_BPS: u16 = 20;

/// Mirrors the strategy's `InitParams`.
#[derive(borsh::BorshSerialize)]
struct InitParams {
    supply: u64,
    curve_supply: u64,
    virtual_quote: u64,
    virtual_token: u64,
    pool_fee_bps: u16,
    creator_fee_bps: u16,
    creator: [u8; 32],
}

fn params(creator: &Pubkey) -> InitParams {
    InitParams {
        supply: SUPPLY,
        curve_supply: CURVE_SUPPLY,
        virtual_quote: VIRTUAL_QUOTE,
        virtual_token: VIRTUAL_TOKEN,
        pool_fee_bps: POOL_FEE_BPS,
        creator_fee_bps: CREATOR_FEE_BPS,
        creator: creator.to_bytes(),
    }
}

struct LaunchMarket {
    market: Pubkey,
    launch: Pubkey,
    /// Claims the creator fee.
    creator: Keypair,
    /// The program's upgrade authority, who claims the protocol fee.
    owner: Keypair,
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

/// Registers the program once, returning its upgrade authority, and leaves the
/// authority in place for later markets.
fn register(env: &mut Env) -> Keypair {
    if env
        .svm
        .get_account(&pda::strategy(&LAUNCH_PROGRAM_ID))
        .is_none()
    {
        return env.register(&LAUNCH_PROGRAM_ID, "launch_curve");
    }
    let owner = env.funded_keypair();
    set_upgrade_authority(&mut env.svm, &LAUNCH_PROGRAM_ID, Some(&owner.pubkey()));
    owner
}

/// Creates a market with `params(creator)` against a fresh 6-decimal quote mint. The
/// token mint is a new keypair signing `create_market`.
fn try_setup(
    env: &mut Env,
    make_params: impl FnOnce(&Pubkey) -> InitParams,
) -> Result<LaunchMarket, Box<TxResult>> {
    let owner = register(env);
    let creator = env.funded_keypair();
    let params = make_params(&creator.pubkey());
    let quote_mint = env.create_mint(&TOKEN, 6, None, None);
    let token_mint = Keypair::new();
    let launch = launch_pda(&token_mint.pubkey());
    let quote_vault = launch_child(b"quote_vault", &launch);
    let token_vault = launch_child(b"token_vault", &launch);

    let ix = client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
        strategy_program: LAUNCH_PROGRAM_ID,
        market_state: launch,
        market_state_signs: false,
        init_data: borsh::to_vec(&params).unwrap(),
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
        creator,
        owner,
        quote_mint,
        token_mint: token_mint.pubkey(),
        quote_vault,
        token_vault,
    })
}

fn setup(env: &mut Env) -> LaunchMarket {
    try_setup(env, params).unwrap_or_else(|_| panic!("create_market"))
}

/// The launch account's `(graduated, quote_reserve, token_reserve, graduation_slot)`.
fn state(env: &Env, m: &LaunchMarket) -> (bool, u64, u64, u64) {
    let data = env.account_data(&m.launch);
    let at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
    // 8 discriminator + 5 pubkeys + 4 u64 + 2 u16 = 204.
    (data[204] == 1, at(205), at(213), at(221))
}

/// Unclaimed fees: `(creator quote, creator token, protocol quote, protocol token)`.
fn owed(env: &Env, m: &LaunchMarket) -> (u64, u64, u64, u64) {
    let data = env.account_data(&m.launch);
    let at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
    (at(229), at(237), at(245), at(253))
}

/// The creator's and protocol's fees on an input of `amount`.
fn trade_fees(amount: u64) -> (u64, u64) {
    (
        fees::ceil_bps(amount, CREATOR_FEE_BPS as u64).unwrap(),
        fees::ceil_bps(amount, PROTOCOL_FEE_BPS as u64).unwrap(),
    )
}

/// What is left of an input after both fees.
fn net_in(amount: u64) -> u64 {
    let (creator, protocol) = trade_fees(amount);
    amount - creator - protocol
}

/// The smallest input whose net after fees is exactly `target`.
fn gross_up(target: u64) -> u64 {
    let guess = (target as u128 * 10_000
        / (10_000 - CREATOR_FEE_BPS as u128 - PROTOCOL_FEE_BPS as u128)) as u64;
    (guess.saturating_sub(8)..guess + 8)
        .find(|a| net_in(*a) == target)
        .expect("a gross amount nets to the target")
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

/// What a buy of `amount` gives, mirroring the strategy across graduation: the fees
/// come off first, the rest trades.
fn price_buy(env: &Env, m: &LaunchMarket, amount: u64) -> u64 {
    let (graduated, quote_reserve, token_reserve, _) = state(env, m);
    let amount = net_in(amount);
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
    let amount = net_in(amount);
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

/// The associated token account, created if it does not exist yet.
fn ata_for(env: &mut Env, owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    let address = pda::associated_token(owner, mint, &TOKEN);
    if env.svm.get_account(&address).is_none() {
        env.create_ata(owner, mint, &TOKEN);
    }
    address
}

/// `claim_fees` by `claimant`, paid to their associated token accounts.
fn claim_fees_ix(env: &mut Env, m: &LaunchMarket, claimant: &Pubkey) -> Instruction {
    let claimant_quote = ata_for(env, claimant, &m.quote_mint);
    let claimant_token = ata_for(env, claimant, &m.token_mint);
    Instruction::new_with_bytes(
        LAUNCH_PROGRAM_ID,
        &client::instruction_discriminator("claim_fees"),
        vec![
            AccountMeta::new_readonly(*claimant, true),
            AccountMeta::new(m.launch, false),
            AccountMeta::new(m.quote_vault, false),
            AccountMeta::new(m.token_vault, false),
            AccountMeta::new(claimant_quote, false),
            AccountMeta::new(claimant_token, false),
            AccountMeta::new_readonly(m.quote_mint, false),
            AccountMeta::new_readonly(m.token_mint, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(pda::program_data(&LAUNCH_PROGRAM_ID), false),
        ],
    )
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
    let refused = |env: &mut Env, p: fn(&Pubkey) -> InitParams| {
        let failed = try_setup(env, p).err().expect("create_market should fail");
        assert_fails_with(*failed, "BadInitData");
    };
    // Nothing held back for the pool.
    refused(&mut env, |c| InitParams {
        curve_supply: SUPPLY,
        ..params(c)
    });
    // The curve would hit an infinite price as it sells out.
    refused(&mut env, |c| InitParams {
        virtual_token: 0,
        ..params(c)
    });
    refused(&mut env, |c| InitParams {
        pool_fee_bps: 10_000,
        ..params(c)
    });
    // The creator fee is capped at 5%.
    refused(&mut env, |c| InitParams {
        creator_fee_bps: 501,
        ..params(c)
    });
}

#[test]
fn curve_trades_follow_the_curve_and_match_quotes() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 1_000_000 * USD);

    // The opening price is virtual_quote / (virtual_token + curve_supply) = $0.0001, a
    // $100k market cap. A $10 buy barely moves it.
    let amount = 10 * USD;
    let got = buy(&mut env, &m, &t, amount);
    let opening_price = VIRTUAL_QUOTE as f64 / (VIRTUAL_TOKEN + CURVE_SUPPLY) as f64;
    // 0.5% dearer than the curve itself: the creator's and protocol's fees.
    let paid_per_token = amount as f64 / got as f64;
    assert!((paid_per_token / opening_price - 1.005).abs() < 1e-3);
    let (creator_fee, protocol_fee) = trade_fees(amount);
    assert_eq!((creator_fee, protocol_fee), (30_000, 20_000));
    assert_eq!(reserves(&env, &m), (net_in(amount), CURVE_SUPPLY - got));
    assert_eq!(owed(&env, &m), (creator_fee, 0, protocol_fee, 0));
    assert_eq!(env.token_balance(&m.quote_vault), amount);
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
    let (creator_token, protocol_token) = trade_fees(half);
    assert_eq!(
        reserves(&env, &m),
        (net_in(amount) - back, CURVE_SUPPLY - got + net_in(half))
    );
    assert_eq!(
        owed(&env, &m),
        (creator_fee, creator_token, protocol_fee, protocol_token)
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
    let t = trader(&mut env, &m, 20_000_000 * USD);

    // Half the raise goes in one buy; it does not graduate.
    buy(&mut env, &m, &t, 5_000_000 * USD);
    let (graduated, quote_reserve, left, _) = state(&env, &m);
    assert!(!graduated && left > 0);

    // The buy that takes the rest: it pays exactly for the remaining curve tokens and
    // spends the remaining $100k on the new pool.
    let (x, y) = curve_xy(quote_reserve, left);
    let cost = cost_for(x, y, left);
    let extra = 100_000 * USD;
    let raise = quote_reserve + cost;
    let pool_leg = pool_out(raise, RESERVED, extra);
    assert!(pool_leg > 0);
    // Gross the input up so that, after the 0.5% of fees, cost + extra reaches the curve.
    let amount = gross_up(cost + extra);
    assert_eq!(price_buy(&env, &m, amount), left + pool_leg);

    let before = env.token_balance(&t.2);
    let (_, _, owed_quote_before, _) = owed(&env, &m);
    let accounts = swap_accounts(&m, &t.0.pubkey(), true, &t.1, &t.2);
    let meta = env
        .send(
            &[client::swap(&accounts, &exact_in(amount, left + pool_leg))],
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
    // The quote vault holds the pool's reserve plus every unclaimed fee.
    let (creator_quote, _, protocol_quote, _) = owed(&env, &m);
    assert_eq!(
        env.token_balance(&m.quote_vault),
        raise + extra + creator_quote + protocol_quote
    );
    assert!(protocol_quote > owed_quote_before);
    // The strategy logs its Graduated event.
    assert!(meta.logs.iter().any(|l| l.starts_with("Program data:")));

    // A 10,000x curve raises r / (1 + r) = 1/101 of the graduation market cap: $9.9M.
    let raise_usd = raise as f64 / USD as f64;
    assert!((raise_usd / 9_900_990.0 - 1.0).abs() < 1e-4, "{raise_usd}");
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
    let t = trader(&mut env, &m, 20_000_000 * USD);

    let (x, y) = curve_xy(0, CURVE_SUPPLY);
    let cost = cost_for(x, y, CURVE_SUPPLY);
    let amount = gross_up(cost);

    // One unit short leaves at least one curve token and does not graduate.
    let short = price_buy(&env, &m, amount - 1);
    assert!(short < CURVE_SUPPLY);
    assert_eq!(quoted(&env, &m, true, amount - 1), short);

    let got = buy(&mut env, &m, &t, amount);
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
    let whale = trader(&mut env, &m, 20_000_000 * USD);
    let (x, y) = curve_xy(0, CURVE_SUPPLY);
    let cost = cost_for(x, y, CURVE_SUPPLY);
    let amount = gross_up(cost);
    buy(&mut env, &m, &whale, amount);
    let (_, pool_quote, pool_token, _) = state(&env, &m);
    assert!(state(&env, &m).0);

    // A pool buy: constant product on real reserves, 0.3% fee kept by the pool.
    let t = trader(&mut env, &m, 1_000_000 * USD);
    let amount = 10_000 * USD;
    let got = buy(&mut env, &m, &t, amount);
    assert_eq!(got, pool_out(pool_quote, pool_token, net_in(amount)));
    // Just over $1 per token, since the pool opened at parity: 0.5% of fees, the 0.3%
    // pool fee and ~0.1% of impact on a $9.9M pool.
    let paid = amount as f64 / got as f64;
    assert!(paid > 1.0 && paid < 1.012, "{paid}");
    assert_eq!(
        reserves(&env, &m),
        (pool_quote + net_in(amount), pool_token - got)
    );

    // Selling nearly everything back (Pipeline takes its fee on a sell in the token, so
    // the trader keeps a little) leaves the pool richer and still graduated.
    let sold = got * 9_900 / 10_000;
    let back = sell(&mut env, &m, &t, sold);
    assert!(back < amount);
    let (graduated, quote_after, token_after, _) = state(&env, &m);
    assert!(graduated);
    assert_eq!(
        (quote_after, token_after),
        (
            pool_quote + net_in(amount) - back,
            pool_token - got + net_in(sold)
        )
    );
    let (creator_quote, _, protocol_quote, _) = owed(&env, &m);
    assert_eq!(
        env.token_balance(&m.quote_vault),
        quote_after + creator_quote + protocol_quote
    );

    // The whale dumping most of the supply moves the pool, never the curve: the sell is
    // priced on real reserves alone.
    let dump = CURVE_SUPPLY / 2;
    let out = sell(&mut env, &m, &whale, dump);
    assert_eq!(out, pool_out(token_after, quote_after, net_in(dump)));
    assert!(state(&env, &m).0);
}

#[test]
fn fees_accrue_in_the_vaults_and_are_claimed_by_the_creator_and_the_owner() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 1_000_000 * USD);

    // A buy and a sell leave fees in both mints.
    let amount = 50_000 * USD;
    let got = buy(&mut env, &m, &t, amount);
    let sold = got / 2;
    sell(&mut env, &m, &t, sold);
    let (creator_quote, creator_token, protocol_quote, protocol_token) = owed(&env, &m);
    assert_eq!(
        (creator_quote, protocol_quote),
        (trade_fees(amount).0, trade_fees(amount).1)
    );
    assert_eq!(
        (creator_token, protocol_token),
        (trade_fees(sold).0, trade_fees(sold).1)
    );
    assert!(creator_quote > protocol_quote && creator_token > protocol_token);

    // A stranger cannot claim.
    let stranger = env.funded_keypair();
    let ix = claim_fees_ix(&mut env, &m, &stranger.pubkey());
    assert_fails_with(env.send(&[ix], &[&stranger]), "NotFeeRecipient");

    // The creator claims the 0.3% side; the protocol's share stays put.
    let creator = m.creator.pubkey();
    let ix = claim_fees_ix(&mut env, &m, &creator);
    env.send(&[ix], &[&m.creator]).expect("creator claims");
    assert_eq!(
        env.token_balance(&pda::associated_token(&creator, &m.quote_mint, &TOKEN)),
        creator_quote
    );
    assert_eq!(
        env.token_balance(&pda::associated_token(&creator, &m.token_mint, &TOKEN)),
        creator_token
    );
    assert_eq!(owed(&env, &m), (0, 0, protocol_quote, protocol_token));

    // The program's upgrade authority claims the 0.2% side.
    let owner = m.owner.pubkey();
    let ix = claim_fees_ix(&mut env, &m, &owner);
    env.send(&[ix], &[&m.owner]).expect("owner claims");
    assert_eq!(
        env.token_balance(&pda::associated_token(&owner, &m.quote_mint, &TOKEN)),
        protocol_quote
    );
    assert_eq!(
        env.token_balance(&pda::associated_token(&owner, &m.token_mint, &TOKEN)),
        protocol_token
    );
    assert_eq!(owed(&env, &m), (0, 0, 0, 0));

    // Claims never touch the reserves, and trading continues on exactly them.
    let (quote_reserve, token_reserve) = reserves(&env, &m);
    assert_eq!(env.token_balance(&m.quote_vault), quote_reserve);
    assert_eq!(env.token_balance(&m.token_vault), token_reserve + RESERVED);
    buy(&mut env, &m, &t, 1_000 * USD);

    // Once the program is made immutable, nobody can claim the protocol side.
    set_upgrade_authority(&mut env.svm, &LAUNCH_PROGRAM_ID, None);
    let ix = claim_fees_ix(&mut env, &m, &owner);
    assert_fails_with(env.send(&[ix], &[&m.owner]), "NotFeeRecipient");
}
