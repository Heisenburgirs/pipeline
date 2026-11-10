//! The perp-backed curve through the router: the treasury fee on buys and the burn on
//! sells, deploying the treasury into a position on the test venue, and holders burning
//! tokens to close their share of it.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_strategy_interface::fees;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const PERP_CURVE_ID: Pubkey = client::pubkey!("4LHmX6x7xgB2eNeuRsuF3U3huhFoyfYeddRLzsBwzvPc");
const VENUE_ID: Pubkey = client::pubkey!("8FcD14vCkywa5zPAMmy3g9VpnESJYUZM36c6hzJBdsYi");

/// The quote token stands in for USDC: 6 decimals, so 1 unit is $1.
const USD: u64 = 1_000_000;
/// The launch curve's default preset: a $100k start, a $1B graduation, $9.9M locked.
const SUPPLY: u64 = 1_000_000_000 * USD;
const CURVE_SUPPLY: u64 = 990_099_009_900_990;
const RESERVED: u64 = SUPPLY - CURVE_SUPPLY;
const VIRTUAL_TOKEN: u64 = 10_001_000_100_010;
const VIRTUAL_QUOTE: u64 = 100_010_001_000;
const POOL_FEE_BPS: u16 = 0;
/// 0.3% creator, 0.2% protocol (compiled in), 0.3% treasury.
const CREATOR_FEE_BPS: u16 = 30;
const PROTOCOL_FEE_BPS: u16 = 20;
const PERP_FEE_BPS: u16 = 30;
/// A 2x long on SOL-PERP, deployed in chunks of at least $100.
const LONG: u8 = 0;
const SHORT: u8 = 1;
const LEVERAGE: u8 = 2;
const MIN_DEPLOY: u64 = 100 * USD;
/// Venue prices are quote base units per whole base asset; base is in millionths.
const BASE_UNIT: u64 = 1_000_000;
const SOL_PRICE: u64 = 150 * USD;
const SYMBOL: &[u8; 16] = b"SOL-PERP\0\0\0\0\0\0\0\0";

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
    perp_market: [u8; 32],
    perp_direction: u8,
    perp_leverage: u8,
    perp_fee_bps: u16,
    min_deploy: u64,
}

struct PerpMarket {
    launch: Pubkey,
    creator: Keypair,
    quote_mint: Pubkey,
    token_mint: Pubkey,
    quote_vault: Pubkey,
    token_vault: Pubkey,
    /// The venue market, its vault and the launch's position on it.
    venue_market: Pubkey,
    venue_vault: Pubkey,
    position: Pubkey,
}

fn launch_pda(token_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"launch", token_mint.as_ref()], &PERP_CURVE_ID).0
}

fn launch_child(seed: &[u8], launch: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[seed, launch.as_ref()], &PERP_CURVE_ID).0
}

fn venue_market_pda(symbol: &[u8; 16]) -> Pubkey {
    Pubkey::find_program_address(&[b"market", symbol], &VENUE_ID).0
}

fn venue_vault_pda(market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault", market.as_ref()], &VENUE_ID).0
}

fn position_pda(market: &Pubkey, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"position", market.as_ref(), owner.as_ref()], &VENUE_ID).0
}

/// Loads the venue and creates a SOL-PERP market quoting `quote_mint` at `SOL_PRICE`,
/// with the test admin as its price authority and $10M of house liquidity in its vault.
fn setup_venue(env: &mut Env, quote_mint: &Pubkey) -> (Pubkey, Pubkey) {
    if env.svm.get_account(&VENUE_ID).is_none() {
        env.svm
            .add_program_from_file(VENUE_ID, sbf_dir().join("test_perp.so"))
            .expect("venue program");
    }
    let market = venue_market_pda(SYMBOL);
    let vault = venue_vault_pda(&market);
    let admin = env.admin.pubkey();
    let mut data = client::instruction_discriminator("create_market").to_vec();
    data.extend_from_slice(SYMBOL);
    data.extend_from_slice(&SOL_PRICE.to_le_bytes());
    let ix = Instruction::new_with_bytes(
        VENUE_ID,
        &data,
        vec![
            AccountMeta::new(admin, true),
            AccountMeta::new(market, false),
            AccountMeta::new_readonly(*quote_mint, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    );
    env.send(&[ix], &[]).expect("create venue market");
    env.mint_to(quote_mint, &TOKEN, &vault, 10_000_000 * USD);
    (market, vault)
}

fn set_price_ix(market: &Pubkey, authority: &Pubkey, price: u64) -> Instruction {
    let mut data = client::instruction_discriminator("set_price").to_vec();
    data.extend_from_slice(&price.to_le_bytes());
    Instruction::new_with_bytes(
        VENUE_ID,
        &data,
        vec![
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(*market, false),
        ],
    )
}

fn set_price(env: &mut Env, m: &PerpMarket, price: u64) {
    let admin = env.admin.pubkey();
    env.send(&[set_price_ix(&m.venue_market, &admin, price)], &[])
        .expect("set price");
}

/// Registers the strategy, sets up the venue and creates a market with `direction`.
fn setup_with(env: &mut Env, direction: u8) -> PerpMarket {
    if env
        .svm
        .get_account(&pda::strategy(&PERP_CURVE_ID))
        .is_none()
    {
        env.register(&PERP_CURVE_ID, "perp_curve");
    }
    let quote_mint = env.create_mint(&TOKEN, 6, None, None);
    let (venue_market, venue_vault) = setup_venue(env, &quote_mint);
    let creator = env.funded_keypair();
    let token_mint = Keypair::new();
    let launch = launch_pda(&token_mint.pubkey());
    let quote_vault = launch_child(b"quote_vault", &launch);
    let token_vault = launch_child(b"token_vault", &launch);
    let params = InitParams {
        supply: SUPPLY,
        curve_supply: CURVE_SUPPLY,
        virtual_quote: VIRTUAL_QUOTE,
        virtual_token: VIRTUAL_TOKEN,
        pool_fee_bps: POOL_FEE_BPS,
        creator_fee_bps: CREATOR_FEE_BPS,
        creator: creator.pubkey().to_bytes(),
        perp_market: venue_market.to_bytes(),
        perp_direction: direction,
        perp_leverage: LEVERAGE,
        perp_fee_bps: PERP_FEE_BPS,
        min_deploy: MIN_DEPLOY,
    };
    let ix = client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
        strategy_program: PERP_CURVE_ID,
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
    env.send(&[ix], &[&creator, &token_mint])
        .expect("create_market");

    let market = pda::market(&PERP_CURVE_ID, &launch);
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &market, &quote_mint, &TOKEN),
            client::init_fee_account(&payer, &market, &token_mint.pubkey(), &TOKEN),
        ],
        &[],
    )
    .expect("init fee accounts");

    PerpMarket {
        launch,
        creator,
        quote_mint,
        token_mint: token_mint.pubkey(),
        quote_vault,
        token_vault,
        venue_market,
        venue_vault,
        position: position_pda(&venue_market, &launch),
    }
}

fn setup(env: &mut Env) -> PerpMarket {
    setup_with(env, LONG)
}

/// `(graduated, quote_reserve, token_reserve)`.
fn state(env: &Env, m: &PerpMarket) -> (bool, u64, u64) {
    let data = env.account_data(&m.launch);
    let at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
    (data[204] == 1, at(205), at(213))
}

/// `(creator quote, creator token, protocol quote, protocol token, treasury quote,
/// treasury token, burned)`.
fn owed(env: &Env, m: &PerpMarket) -> (u64, u64, u64, u64, u64, u64, u64) {
    let data = env.account_data(&m.launch);
    let at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
    (
        at(229),
        at(237),
        at(245),
        at(253),
        at(337),
        at(345),
        at(353),
    )
}

/// `(direction, leverage, collateral, base, entry_price)` of the venue position, or
/// `None` before it is opened.
fn position(env: &Env, m: &PerpMarket) -> Option<(u8, u8, u64, u64, u64)> {
    let data = env.account_data(&m.position);
    if data.len() < 99 {
        return None;
    }
    let at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
    Some((data[72], data[73], at(74), at(82), at(90)))
}

/// The venue's equity formula, mirrored.
fn equity(position: (u8, u8, u64, u64, u64), price: u64) -> u64 {
    let (direction, _, collateral, base, entry) = position;
    let delta = price as i128 - entry as i128;
    let signed = if direction == LONG { delta } else { -delta };
    let pnl = base as i128 * signed / BASE_UNIT as i128;
    (collateral as i128 + pnl).clamp(0, u64::MAX as i128) as u64
}

/// Tokens in holders' hands, mirrored from the strategy.
fn circulating(env: &Env, m: &PerpMarket) -> u64 {
    let (graduated, _, token_reserve) = state(env, m);
    let (_, creator_token, _, protocol_token, _, treasury_token, burned) = owed(env, m);
    SUPPLY
        - burned
        - token_reserve
        - if graduated { 0 } else { RESERVED }
        - creator_token
        - protocol_token
        - treasury_token
}

/// The fees on an input: `(creator, protocol, treasury)`.
fn trade_fees(amount: u64) -> (u64, u64, u64) {
    let bps = |rate: u16| fees::ceil_bps(amount, rate as u64).unwrap();
    (
        bps(CREATOR_FEE_BPS),
        bps(PROTOCOL_FEE_BPS),
        bps(PERP_FEE_BPS),
    )
}

fn net_in(amount: u64) -> u64 {
    let (c, p, t) = trade_fees(amount);
    amount - c - p - t
}

fn out_given_in(reserve_in: u128, reserve_out: u128, amount_in: u64) -> u64 {
    (reserve_out * amount_in as u128 / (reserve_in + amount_in as u128)) as u64
}

/// What a buy gives on the curve (before graduation).
fn price_buy(env: &Env, m: &PerpMarket, amount: u64) -> u64 {
    let (_, quote_reserve, token_reserve) = state(env, m);
    let x = VIRTUAL_QUOTE as u128 + quote_reserve as u128;
    let y = VIRTUAL_TOKEN as u128 + token_reserve as u128;
    out_given_in(x, y, net_in(amount)).min(token_reserve)
}

/// What a sell pays on the curve (before graduation).
fn price_sell(env: &Env, m: &PerpMarket, amount: u64) -> u64 {
    let (_, quote_reserve, token_reserve) = state(env, m);
    let x = VIRTUAL_QUOTE as u128 + quote_reserve as u128;
    let y = VIRTUAL_TOKEN as u128 + token_reserve as u128;
    out_given_in(y, x, net_in(amount)).min(quote_reserve)
}

fn swap_accounts(
    m: &PerpMarket,
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
        strategy_program: PERP_CURVE_ID,
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

fn trader(env: &mut Env, m: &PerpMarket, quote: u64) -> (Keypair, Pubkey, Pubkey) {
    let (trader, trader_quote) = env.trader_with(&m.quote_mint, &TOKEN, quote);
    let trader_token = env.create_ata(&trader.pubkey(), &m.token_mint, &TOKEN);
    (trader, trader_quote, trader_token)
}

fn buy(env: &mut Env, m: &PerpMarket, t: &(Keypair, Pubkey, Pubkey), amount: u64) -> u64 {
    let expected = price_buy(env, m, amount);
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

fn sell(env: &mut Env, m: &PerpMarket, t: &(Keypair, Pubkey, Pubkey), amount: u64) -> u64 {
    let expected = price_sell(env, m, amount);
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

fn deploy_ix(m: &PerpMarket, payer: &Pubkey) -> Instruction {
    Instruction::new_with_bytes(
        PERP_CURVE_ID,
        &client::instruction_discriminator("deploy"),
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(m.launch, false),
            AccountMeta::new(m.quote_vault, false),
            AccountMeta::new(m.token_vault, false),
            AccountMeta::new(m.token_mint, false),
            AccountMeta::new_readonly(m.venue_market, false),
            AccountMeta::new(m.position, false),
            AccountMeta::new(m.venue_vault, false),
            AccountMeta::new_readonly(m.quote_mint, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(VENUE_ID, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    )
}

fn burn_to_close_ix(
    m: &PerpMarket,
    holder: &Pubkey,
    holder_token: &Pubkey,
    holder_quote: &Pubkey,
    amount: u64,
) -> Instruction {
    let mut data = client::instruction_discriminator("burn_to_close").to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction::new_with_bytes(
        PERP_CURVE_ID,
        &data,
        vec![
            AccountMeta::new_readonly(*holder, true),
            AccountMeta::new(m.launch, false),
            AccountMeta::new(*holder_token, false),
            AccountMeta::new(*holder_quote, false),
            AccountMeta::new(m.token_mint, false),
            AccountMeta::new_readonly(m.quote_mint, false),
            AccountMeta::new_readonly(m.venue_market, false),
            AccountMeta::new(m.position, false),
            AccountMeta::new(m.venue_vault, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(VENUE_ID, false),
        ],
    )
}

fn mint_supply(env: &Env, m: &PerpMarket) -> u64 {
    let mint = env.account_data(&m.token_mint);
    u64::from_le_bytes(mint[36..44].try_into().unwrap())
}

// ------------------------------------------------------------------------ tests

#[test]
fn buys_accrue_the_treasury_and_deploy_opens_the_position() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 1_000_000 * USD);

    // 0.3% of a $10k buy is $30: below the $100 minimum, so nothing deploys yet.
    let amount = 10_000 * USD;
    buy(&mut env, &m, &t, amount);
    let (creator, protocol, treasury) = trade_fees(amount);
    assert_eq!(
        (creator, protocol, treasury),
        (30 * USD, 20 * USD, 30 * USD)
    );
    assert_eq!(owed(&env, &m), (creator, 0, protocol, 0, treasury, 0, 0));
    assert_eq!(state(&env, &m).1, net_in(amount));
    assert_eq!(env.token_balance(&m.quote_vault), amount);
    assert_eq!(position(&env, &m), None);
    let keeper = env.funded_keypair();
    assert_fails_with(
        env.send(&[deploy_ix(&m, &keeper.pubkey())], &[&keeper]),
        "NothingToDeploy",
    );

    // Three more buys bring it to $120; anyone deploys it, paying the position's rent.
    for _ in 0..3 {
        buy(&mut env, &m, &t, amount);
    }
    let accrued = 4 * treasury;
    assert_eq!(owed(&env, &m).4, accrued);
    let vault_before = env.token_balance(&m.quote_vault);
    let venue_before = env.token_balance(&m.venue_vault);
    env.send(&[deploy_ix(&m, &keeper.pubkey())], &[&keeper])
        .expect("deploy");
    assert_eq!(owed(&env, &m).4, 0);
    assert_eq!(env.token_balance(&m.quote_vault), vault_before - accrued);
    assert_eq!(env.token_balance(&m.venue_vault), venue_before + accrued);
    // A 2x long: $120 of collateral buys 1.6 SOL of base at $150.
    let expected_base = accrued * LEVERAGE as u64 * BASE_UNIT / SOL_PRICE;
    assert_eq!(
        position(&env, &m),
        Some((LONG, LEVERAGE, accrued, expected_base, SOL_PRICE))
    );
    // The reserves never included the treasury; the vault held it alongside them.
    let (_, quote_reserve, _) = state(&env, &m);
    let (creator_q, _, protocol_q, _, _, _, _) = owed(&env, &m);
    assert_eq!(
        env.token_balance(&m.quote_vault),
        quote_reserve + creator_q + protocol_q
    );

    // A second deploy adds at the price of the day and averages the entry.
    for _ in 0..4 {
        buy(&mut env, &m, &t, amount);
    }
    set_price(&mut env, &m, 200 * USD);
    env.send(&[deploy_ix(&m, &keeper.pubkey())], &[&keeper])
        .expect("deploy again");
    let added_base = accrued * LEVERAGE as u64 * BASE_UNIT / (200 * USD);
    let (_, _, collateral, base, entry) = position(&env, &m).unwrap();
    assert_eq!(
        (collateral, base),
        (2 * accrued, expected_base + added_base)
    );
    let expected_entry = (expected_base as u128 * SOL_PRICE as u128
        + added_base as u128 * 200 * USD as u128)
        / (expected_base + added_base) as u128;
    assert_eq!(entry as u128, expected_entry);
}

#[test]
fn sells_burn_the_treasury_fee() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 1_000_000 * USD);
    let got = buy(&mut env, &m, &t, 10_000 * USD);
    assert_eq!(mint_supply(&env, &m), SUPPLY);

    let sold = got / 2;
    sell(&mut env, &m, &t, sold);
    let (creator, protocol, to_burn) = trade_fees(sold);
    let (_, creator_t, _, protocol_t, treasury_q, treasury_t, burned) = owed(&env, &m);
    assert_eq!(
        (creator_t, protocol_t, treasury_t, burned),
        (creator, protocol, to_burn, 0)
    );
    // The fee waits in the token vault, outside the reserve and the circulating supply,
    // until a deploy burns it. The quote side is below the minimum, so the deploy only
    // burns.
    let (_, _, token_reserve) = state(&env, &m);
    assert_eq!(
        env.token_balance(&m.token_vault),
        token_reserve + RESERVED + creator + protocol + to_burn
    );
    assert_eq!(circulating(&env, &m), got - sold);
    assert!(treasury_q < MIN_DEPLOY);
    let keeper = env.funded_keypair();
    env.send(&[deploy_ix(&m, &keeper.pubkey())], &[&keeper])
        .expect("deploy burns");
    let (_, _, _, _, treasury_q_after, treasury_t_after, burned) = owed(&env, &m);
    assert_eq!(
        (treasury_q_after, treasury_t_after, burned),
        (treasury_q, 0, to_burn)
    );
    assert_eq!(mint_supply(&env, &m), SUPPLY - to_burn);
    assert_eq!(
        env.token_balance(&m.token_vault),
        token_reserve + RESERVED + creator + protocol
    );
    assert_eq!(position(&env, &m), None);
    assert_eq!(circulating(&env, &m), got - sold);
    // With nothing left to burn and still too little quote, there is nothing to do.
    assert_fails_with(
        env.send(&[deploy_ix(&m, &keeper.pubkey())], &[&keeper]),
        "NothingToDeploy",
    );
}

#[test]
fn holders_burn_tokens_to_close_their_share_of_the_position() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 1_000_000 * USD);
    let (holder, holder_quote, holder_token) = (t.0.pubkey(), t.1, t.2);
    let got = buy(&mut env, &m, &t, 100_000 * USD);
    let keeper = env.funded_keypair();

    // Before the position exists, burning is refused.
    assert_fails_with(
        env.send(
            &[burn_to_close_ix(
                &m,
                &holder,
                &holder_token,
                &holder_quote,
                got / 10,
            )],
            &[&t.0],
        ),
        "NoPosition",
    );
    env.send(&[deploy_ix(&m, &keeper.pubkey())], &[&keeper])
        .expect("deploy");
    let pos = position(&env, &m).unwrap();
    assert_eq!(pos.2, 300 * USD);

    // SOL rises 20%: at 2x the position is worth 40% more. Burning a tenth of the
    // circulating supply pays a tenth of that equity.
    set_price(&mut env, &m, 180 * USD);
    let worth = equity(pos, 180 * USD);
    assert_eq!(worth, 420 * USD);
    let circ = circulating(&env, &m);
    assert_eq!(circ, got);
    let burn = circ / 10;
    let quote_before = env.token_balance(&holder_quote);
    let curve_would_pay = price_sell(&env, &m, burn);
    env.send(
        &[burn_to_close_ix(
            &m,
            &holder,
            &holder_token,
            &holder_quote,
            burn,
        )],
        &[&t.0],
    )
    .expect("burn to close");
    let payout = env.token_balance(&holder_quote) - quote_before;
    let base_closed = pos.3 as u128 * burn as u128 / circ as u128;
    assert_eq!(payout as u128, worth as u128 * base_closed / pos.3 as u128);
    assert!(payout > 0);
    assert_eq!(env.token_balance(&holder_token), got - burn);
    assert_eq!(mint_supply(&env, &m), SUPPLY - burn);
    assert_eq!(owed(&env, &m).6, burn);
    assert_eq!(circulating(&env, &m), circ - burn);
    // The position shrank by the share, the curve did not move.
    let after = position(&env, &m).unwrap();
    assert_eq!(after.3 as u128, pos.3 as u128 - base_closed);
    assert_eq!(
        after.2 as u128,
        pos.2 as u128 - pos.2 as u128 * base_closed / pos.3 as u128
    );
    assert_eq!(price_sell(&env, &m, burn), curve_would_pay);

    // More than the circulating supply, or nothing, is refused.
    assert_fails_with(
        env.send(
            &[burn_to_close_ix(
                &m,
                &holder,
                &holder_token,
                &holder_quote,
                circ,
            )],
            &[&t.0],
        ),
        "BadBurn",
    );
    assert_fails_with(
        env.send(
            &[burn_to_close_ix(
                &m,
                &holder,
                &holder_token,
                &holder_quote,
                0,
            )],
            &[&t.0],
        ),
        "BadBurn",
    );

    // SOL halves: a 2x long has no equity left. A burn still burns, pays nothing, and
    // wipes the position; the curve is still there to sell into.
    set_price(&mut env, &m, 75 * USD);
    let quote_before = env.token_balance(&holder_quote);
    env.send(
        &[burn_to_close_ix(
            &m,
            &holder,
            &holder_token,
            &holder_quote,
            burn,
        )],
        &[&t.0],
    )
    .expect("burn into a liquidated position");
    assert_eq!(env.token_balance(&holder_quote), quote_before);
    assert_eq!(position(&env, &m).unwrap().3, 0);
    assert!(sell(&mut env, &m, &t, burn) > 0);
}

#[test]
fn a_short_profits_when_the_price_falls() {
    let mut env = Env::new();
    let m = setup_with(&mut env, SHORT);
    let t = trader(&mut env, &m, 1_000_000 * USD);
    let (holder, holder_quote, holder_token) = (t.0.pubkey(), t.1, t.2);
    let got = buy(&mut env, &m, &t, 100_000 * USD);
    let keeper = env.funded_keypair();
    env.send(&[deploy_ix(&m, &keeper.pubkey())], &[&keeper])
        .expect("deploy");
    let pos = position(&env, &m).unwrap();
    assert_eq!(pos.0, SHORT);

    // SOL falls 10%: a 2x short is worth 20% more.
    set_price(&mut env, &m, 135 * USD);
    let worth = equity(pos, 135 * USD);
    assert_eq!(worth, 360 * USD);
    let quote_before = env.token_balance(&holder_quote);
    env.send(
        &[burn_to_close_ix(
            &m,
            &holder,
            &holder_token,
            &holder_quote,
            got,
        )],
        &[&t.0],
    )
    .expect("burn everything");
    // The whole circulating supply closes the whole position.
    assert_eq!(env.token_balance(&holder_quote) - quote_before, worth);
    assert_eq!(position(&env, &m).unwrap().3, 0);
    assert_eq!(circulating(&env, &m), 0);
}

#[test]
fn the_curve_still_graduates_and_the_treasury_keeps_accruing() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 20_000_000 * USD);
    // Well past the $9.9M raise, fees included.
    let amount = 12_000_000 * USD;
    let before = env.token_balance(&t.2);
    let accounts = swap_accounts(&m, &t.0.pubkey(), true, &t.1, &t.2);
    env.send(&[client::swap(&accounts, &exact_in(amount, 1))], &[&t.0])
        .expect("graduating buy");
    assert!(env.token_balance(&t.2) - before > CURVE_SUPPLY);
    let (graduated, _, _) = state(&env, &m);
    assert!(graduated);
    assert_eq!(owed(&env, &m).4, trade_fees(amount).2);
    assert_eq!(circulating(&env, &m), env.token_balance(&t.2));

    // Pool-phase trades pay into the treasury and burn just the same.
    let keeper = env.funded_keypair();
    env.send(&[deploy_ix(&m, &keeper.pubkey())], &[&keeper])
        .expect("deploy");
    assert_eq!(owed(&env, &m).4, 0);
    let held = env.token_balance(&t.2);
    let sold = held / 100;
    let accounts = swap_accounts(&m, &t.0.pubkey(), false, &t.1, &t.2);
    env.send(&[client::swap(&accounts, &exact_in(sold, 1))], &[&t.0])
        .expect("pool sell");
    assert_eq!(owed(&env, &m).5, trade_fees(sold).2);
    env.send(&[deploy_ix(&m, &keeper.pubkey())], &[&keeper])
        .expect("deploy burns the pool-phase fee");
    assert_eq!(owed(&env, &m).6, trade_fees(sold).2);
    assert_eq!(mint_supply(&env, &m), SUPPLY - trade_fees(sold).2);
    let _ = m.creator.pubkey();
}
