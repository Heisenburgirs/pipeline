//! The gacha bonding-curve strategy through the router: curve pricing, the opt-in
//! gacha fee that funds the pot, tickets, and their resolution against a later block
//! hash.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_quoter::{MarketView, QuoteRequest, Quoter};
use pipeline_strategy_interface::{fees, TradeMode};
use sha2::{Digest, Sha256};
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_slot_hashes::SlotHashes;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const GACHA_PROGRAM_ID: Pubkey = client::pubkey!("2TmcmquxnCB8AHoLmLJFYxZnVQdm9pREMEsmHqByQq7G");
const SLOT_HASHES_ID: Pubkey = client::pubkey!("SysvarS1otHashes111111111111111111111111111");
const RESOLVE_WINDOW_SLOTS: u64 = 400;

/// One billion tokens with 6 decimals.
const SUPPLY: u64 = 1_000_000_000_000_000;
/// 30 "SOL" of virtual quote reserve (the quote mint has 9 decimals).
const VIRTUAL_QUOTE: u64 = 30_000_000_000;
/// 280 million tokens of virtual token reserve.
const VIRTUAL_TOKEN: u64 = 280_000_000_000_000;
const GACHA_FEE_BPS: u16 = 200;
const WIN_ONE_IN: u32 = 20;
const BONUS_BPS: u16 = 4_000;
const MAX_WIN_POT_BPS: u16 = 1_000;
const REVEAL_DELAY: u64 = 10;

/// Mirrors the strategy's `InitParams`.
#[derive(borsh::BorshSerialize)]
struct InitParams {
    supply: u64,
    virtual_quote: u64,
    virtual_token: u64,
    gacha_fee_bps: u16,
    win_one_in: u32,
    bonus_bps: u16,
    max_win_pot_bps: u16,
    reveal_delay_slots: u64,
}

fn params() -> InitParams {
    InitParams {
        supply: SUPPLY,
        virtual_quote: VIRTUAL_QUOTE,
        virtual_token: VIRTUAL_TOKEN,
        gacha_fee_bps: GACHA_FEE_BPS,
        win_one_in: WIN_ONE_IN,
        bonus_bps: BONUS_BPS,
        max_win_pot_bps: MAX_WIN_POT_BPS,
        reveal_delay_slots: REVEAL_DELAY,
    }
}

struct GachaMarket {
    market: Pubkey,
    curve: Pubkey,
    quote_mint: Pubkey,
    token_mint: Pubkey,
    quote_vault: Pubkey,
    token_vault: Pubkey,
    pot_vault: Pubkey,
}

fn curve_pda(token_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"curve", token_mint.as_ref()], &GACHA_PROGRAM_ID).0
}

fn curve_child(seed: &[u8], curve: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[seed, curve.as_ref()], &GACHA_PROGRAM_ID).0
}

fn ticket_pda(curve: &Pubkey, trader: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"ticket", curve.as_ref(), trader.as_ref()],
        &GACHA_PROGRAM_ID,
    )
    .0
}

/// Registers the strategy and creates a market whose quote token is a fresh 9-decimal
/// SPL mint. The token mint is a new keypair signing `create_market`.
fn setup(env: &mut Env) -> GachaMarket {
    if env
        .svm
        .get_account(&pda::strategy(&GACHA_PROGRAM_ID))
        .is_none()
    {
        env.register(&GACHA_PROGRAM_ID, "gacha_curve");
    }
    let quote_mint = env.create_mint(&TOKEN, 9, None, None);
    let token_mint = Keypair::new();
    let curve = curve_pda(&token_mint.pubkey());
    let quote_vault = curve_child(b"quote_vault", &curve);
    let token_vault = curve_child(b"token_vault", &curve);
    let pot_vault = curve_child(b"pot", &curve);

    let creator = env.funded_keypair();
    let ix = client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
        strategy_program: GACHA_PROGRAM_ID,
        market_state: curve,
        market_state_signs: false,
        init_data: borsh::to_vec(&params()).unwrap(),
        max_creator_lamports: MAX_SETUP_LAMPORTS,
        strategy_accounts: vec![
            AccountMeta::new_readonly(quote_mint, false),
            AccountMeta::new(token_mint.pubkey(), true),
            AccountMeta::new(quote_vault, false),
            AccountMeta::new(token_vault, false),
            AccountMeta::new(pot_vault, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(TOKEN, false),
        ],
    });
    env.send(&[ix], &[&creator, &token_mint])
        .expect("create_market");

    let market = pda::market(&GACHA_PROGRAM_ID, &curve);
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &market, &quote_mint, &TOKEN),
            client::init_fee_account(&payer, &market, &token_mint.pubkey(), &TOKEN),
        ],
        &[],
    )
    .expect("init fee accounts");

    GachaMarket {
        market,
        curve,
        quote_mint,
        token_mint: token_mint.pubkey(),
        quote_vault,
        token_vault,
        pot_vault,
    }
}

/// `(quote_reserve, token_reserve)` recorded in the curve account.
fn reserves(env: &Env, m: &GachaMarket) -> (u64, u64) {
    let data = env.account_data(&m.curve);
    let at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
    (at(8 + 160), at(8 + 168))
}

/// `(armed, slot, stake)` of a ticket, or `None` if the account is closed.
fn ticket(env: &Env, address: &Pubkey) -> Option<(bool, u64, u64)> {
    let data = env.account_data(address);
    if data.len() < 8 + 32 + 32 + 1 + 8 + 8 + 1 {
        return None;
    }
    let at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
    Some((data[72] == 1, at(73), at(81)))
}

/// The curve's pricing: `floor(reserve_out × in / (reserve_in + in))`.
fn curve_out(reserve_in: u128, reserve_out: u128, amount_in: u64) -> u64 {
    (reserve_out * amount_in as u128 / (reserve_in + amount_in as u128)) as u64
}

/// What a buy of `amount` gives the trader, and what its gacha fee buys for the pot.
fn price_buy(env: &Env, m: &GachaMarket, amount: u64, gacha: bool) -> (u64, u64) {
    let (quote_reserve, token_reserve) = reserves(env, m);
    let x = VIRTUAL_QUOTE as u128 + quote_reserve as u128;
    let y = VIRTUAL_TOKEN as u128 + token_reserve as u128;
    if !gacha {
        return (curve_out(x, y, amount).min(token_reserve), 0);
    }
    let fee = fees::ceil_bps(amount, GACHA_FEE_BPS as u64).unwrap();
    let base = curve_out(x, y, amount - fee).min(token_reserve);
    let pot =
        curve_out(x + (amount - fee) as u128, y - base as u128, fee).min(token_reserve - base);
    (base, pot)
}

fn price_sell(env: &Env, m: &GachaMarket, amount: u64) -> u64 {
    let (quote_reserve, token_reserve) = reserves(env, m);
    let x = VIRTUAL_QUOTE as u128 + quote_reserve as u128;
    let y = VIRTUAL_TOKEN as u128 + token_reserve as u128;
    curve_out(y, x, amount).min(quote_reserve)
}

fn swap_accounts(
    m: &GachaMarket,
    trader: &Pubkey,
    buy: bool,
    gacha: bool,
    trader_quote: &Pubkey,
    trader_token: &Pubkey,
) -> client::SwapAccounts {
    let mut strategy_accounts = vec![AccountMeta::new(
        if buy { m.token_vault } else { m.quote_vault },
        false,
    )];
    if gacha {
        strategy_accounts.push(AccountMeta::new(m.pot_vault, false));
        strategy_accounts.push(AccountMeta::new(ticket_pda(&m.curve, trader), false));
    }
    let (input_mint, output_mint, trader_input, trader_output, input_vault) = if buy {
        (
            m.quote_mint,
            m.token_mint,
            *trader_quote,
            *trader_token,
            m.quote_vault,
        )
    } else {
        (
            m.token_mint,
            m.quote_mint,
            *trader_token,
            *trader_quote,
            m.token_vault,
        )
    };
    client::SwapAccounts {
        trader: *trader,
        strategy_program: GACHA_PROGRAM_ID,
        market_state: m.curve,
        trader_input,
        trader_output,
        input_vault,
        input_mint,
        output_mint,
        input_token_program: TOKEN,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts,
        input_hook_accounts: vec![],
    }
}

fn gacha_args(amount_in: u64, min_amount_out: u64) -> client::SwapArgs {
    client::SwapArgs {
        extra: vec![1],
        ..exact_in(amount_in, min_amount_out)
    }
}

fn open_ticket_ix(m: &GachaMarket, trader: &Pubkey) -> Instruction {
    Instruction::new_with_bytes(
        GACHA_PROGRAM_ID,
        &client::instruction_discriminator("open_ticket"),
        vec![
            AccountMeta::new(*trader, true),
            AccountMeta::new_readonly(m.curve, false),
            AccountMeta::new(ticket_pda(&m.curve, trader), false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    )
}

fn close_ticket_ix(m: &GachaMarket, trader: &Pubkey) -> Instruction {
    Instruction::new_with_bytes(
        GACHA_PROGRAM_ID,
        &client::instruction_discriminator("close_ticket"),
        vec![
            AccountMeta::new(*trader, true),
            AccountMeta::new(ticket_pda(&m.curve, trader), false),
        ],
    )
}

fn resolve_ticket_ix(m: &GachaMarket, trader: &Pubkey, trader_token: &Pubkey) -> Instruction {
    Instruction::new_with_bytes(
        GACHA_PROGRAM_ID,
        &client::instruction_discriminator("resolve_ticket"),
        vec![
            AccountMeta::new(ticket_pda(&m.curve, trader), false),
            AccountMeta::new_readonly(m.curve, false),
            AccountMeta::new(*trader, false),
            AccountMeta::new(m.pot_vault, false),
            AccountMeta::new(*trader_token, false),
            AccountMeta::new_readonly(m.token_mint, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(SLOT_HASHES_ID, false),
        ],
    )
}

/// A funded trader holding `quote` of the quote token, with an empty token account.
fn trader(env: &mut Env, m: &GachaMarket, quote: u64) -> (Keypair, Pubkey, Pubkey) {
    let (trader, trader_quote) = env.trader_with(&m.quote_mint, &TOKEN, quote);
    let trader_token = env.create_ata(&trader.pubkey(), &m.token_mint, &TOKEN);
    (trader, trader_quote, trader_token)
}

/// Buys `amount` with the gacha, in one transaction with `open_ticket`. Returns the
/// trader's base output and what the pot received.
fn gacha_buy(
    env: &mut Env,
    m: &GachaMarket,
    t: &(Keypair, Pubkey, Pubkey),
    amount: u64,
) -> (u64, u64) {
    let (base, pot) = price_buy(env, m, amount, true);
    let accounts = swap_accounts(m, &t.0.pubkey(), true, true, &t.1, &t.2);
    env.send(
        &[
            open_ticket_ix(m, &t.0.pubkey()),
            client::swap(&accounts, &gacha_args(amount, base)),
        ],
        &[&t.0],
    )
    .expect("gacha buy");
    (base, pot)
}

fn current_slot(env: &Env) -> u64 {
    env.svm.get_sysvar::<solana_clock::Clock>().slot
}

/// The strategy's roll, mirrored: sha256(block hash ‖ ticket ‖ buy slot) as a little-
/// endian u64, won when divisible by `WIN_ONE_IN`.
fn wins(hash: &[u8; 32], ticket: &Pubkey, buy_slot: u64) -> bool {
    let digest = Sha256::new()
        .chain_update(hash)
        .chain_update(ticket.as_ref())
        .chain_update(buy_slot.to_le_bytes())
        .finalize();
    u64::from_le_bytes(digest[..8].try_into().unwrap()) % WIN_ONE_IN as u64 == 0
}

/// A block hash that makes `ticket` win (or lose) its roll.
fn hash_that(ticket: &Pubkey, buy_slot: u64, should_win: bool) -> [u8; 32] {
    (0u64..)
        .map(|nonce| {
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&nonce.to_le_bytes());
            hash
        })
        .find(|hash| wins(hash, ticket, buy_slot) == should_win)
        .unwrap()
}

/// Installs a SlotHashes sysvar holding exactly `entries` (newest first).
fn set_slot_hashes(env: &mut Env, entries: &[(u64, [u8; 32])]) {
    let mut sorted: Vec<(u64, Hash)> = entries
        .iter()
        .map(|(slot, hash)| (*slot, Hash::new_from_array(*hash)))
        .collect();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    env.svm.set_sysvar(&SlotHashes::new(&sorted));
}

// ------------------------------------------------------------------- the quoter

fn quoter_for(env: &Env, m: &GachaMarket, extra_keys: &[Pubkey]) -> Quoter {
    let mut quoter = Quoter::new();
    for (id, name) in [
        (client::PROGRAM_ID, "pipeline"),
        (GACHA_PROGRAM_ID, "gacha_curve"),
    ] {
        let elf = std::fs::read(sbf_dir().join(format!("{name}.so"))).unwrap();
        quoter.load_program(id, &elf).unwrap();
    }
    let mut keys = vec![
        pda::config(),
        m.market,
        m.curve,
        m.quote_mint,
        m.token_mint,
        m.quote_vault,
        m.token_vault,
        m.pot_vault,
        pda::fee_account(&m.market, &m.quote_mint, &TOKEN),
        pda::fee_account(&m.market, &m.token_mint, &TOKEN),
    ];
    keys.extend_from_slice(extra_keys);
    for key in keys {
        if let Some(account) = env.svm.get_account(&key) {
            quoter.set_account(key, account).unwrap();
        }
    }
    quoter
}

fn view(env: &Env, m: &GachaMarket) -> MarketView {
    MarketView::from_market(m.market, &env.market(&m.market), vec![TOKEN, TOKEN]).unwrap()
}

fn quote_request(
    m: &GachaMarket,
    buy: bool,
    amount: u64,
    trader: Option<Pubkey>,
    extra: Vec<u8>,
) -> QuoteRequest {
    let (input_mint, output_mint) = if buy {
        (m.quote_mint, m.token_mint)
    } else {
        (m.token_mint, m.quote_mint)
    };
    QuoteRequest {
        input_mint,
        output_mint,
        mode: TradeMode::ExactIn,
        amount,
        trader,
        integrator_fee_bps: 0,
        extra,
    }
}

// ------------------------------------------------------------------------ tests

#[test]
fn market_creation_mints_a_fixed_supply_into_the_curve() {
    let mut env = Env::new();
    let m = setup(&mut env);

    assert_eq!(env.token_balance(&m.token_vault), SUPPLY);
    assert_eq!(env.token_balance(&m.quote_vault), 0);
    assert_eq!(env.token_balance(&m.pot_vault), 0);
    assert_eq!(reserves(&env, &m), (0, SUPPLY));

    // The mint authority is gone and the supply is exactly what the curve holds.
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
fn plain_buys_and_sells_follow_the_curve_and_match_quotes() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 20_000_000_000);
    let (trader_key, trader_quote, trader_token) = (t.0.pubkey(), t.1, t.2);

    // Buy 1 quote unit (1e9) on the fresh curve.
    let amount = 1_000_000_000;
    let (expected, _) = price_buy(&env, &m, amount, false);
    assert_eq!(
        expected,
        curve_out(
            VIRTUAL_QUOTE as u128,
            (VIRTUAL_TOKEN + SUPPLY) as u128,
            amount
        )
    );

    let mut quoter = quoter_for(&env, &m, &[]);
    let request = quote_request(&m, true, amount, None, vec![]);
    let resolved = quoter
        .resolve_accounts(&view(&env, &m), &request, |_, _| {})
        .unwrap();
    assert_eq!(resolved, vec![AccountMeta::new(m.token_vault, false)]);
    let quote = quoter.quote(&view(&env, &m), &request, &resolved).unwrap();
    assert_eq!(quote.amount_out, expected);
    assert_eq!(quote.total_spend, amount + fees::pipeline_fee(amount));

    let accounts = swap_accounts(&m, &trader_key, true, false, &trader_quote, &trader_token);
    let meta = env
        .send(
            &[client::swap(&accounts, &exact_in(amount, expected))],
            &[&t.0],
        )
        .expect("buy");
    assert_eq!(env.token_balance(&trader_token), expected);
    assert_eq!(
        env.token_balance(&trader_quote),
        20_000_000_000 - amount - fees::pipeline_fee(amount)
    );
    assert_eq!(reserves(&env, &m), (amount, SUPPLY - expected));
    let event = &swap_events(&meta)[0];
    assert_eq!((event.net_sent, event.received), (amount, expected));

    // Sell half back; the quote reserve is what pays, and the quoter agrees.
    let sell = expected / 2;
    let expected_quote = price_sell(&env, &m, sell);
    assert!(expected_quote > 0 && expected_quote < amount);
    let mut quoter = quoter_for(&env, &m, &[]);
    let request = quote_request(&m, false, sell, None, vec![]);
    let resolved = quoter
        .resolve_accounts(&view(&env, &m), &request, |_, _| {})
        .unwrap();
    assert_eq!(resolved, vec![AccountMeta::new(m.quote_vault, false)]);
    let quote = quoter.quote(&view(&env, &m), &request, &resolved).unwrap();
    assert_eq!(quote.amount_out, expected_quote);

    let accounts = swap_accounts(&m, &trader_key, false, false, &trader_quote, &trader_token);
    env.send(
        &[client::swap(&accounts, &exact_in(sell, expected_quote))],
        &[&t.0],
    )
    .expect("sell");
    // Pipeline's fee on a sell is taken in the token.
    assert_eq!(
        env.token_balance(&trader_token),
        expected - sell - fees::pipeline_fee(sell)
    );
    assert_eq!(
        reserves(&env, &m),
        (amount - expected_quote, SUPPLY - expected + sell)
    );

    // Exact-out is refused by the router because the market does not support it.
    let accounts = swap_accounts(&m, &trader_key, true, false, &trader_quote, &trader_token);
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
fn a_gacha_buy_pays_the_curve_price_on_its_base_and_funds_the_pot() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 20_000_000_000);
    let (trader_key, trader_quote, trader_token) = (t.0.pubkey(), t.1, t.2);
    let ticket_key = ticket_pda(&m.curve, &trader_key);

    let amount = 2_000_000_000;
    let fee = fees::ceil_bps(amount, GACHA_FEE_BPS as u64).unwrap();
    let (base, pot) = price_buy(&env, &m, amount, true);
    assert_eq!(fee, 40_000_000);
    assert!(pot > 0 && pot < base);
    // The gacha buyer gets less than a plain buyer of the same amount: the fee's share.
    let (plain, _) = price_buy(&env, &m, amount, false);
    assert!(base < plain && base + pot <= plain);

    // The quoter resolves the pot and the trader's ticket, and quotes the base amount.
    let mut quoter = quoter_for(&env, &m, &[]);
    let request = quote_request(&m, true, amount, Some(trader_key), vec![1]);
    let resolved = quoter
        .resolve_accounts(&view(&env, &m), &request, |_, _| {})
        .unwrap();
    assert_eq!(
        resolved,
        vec![
            AccountMeta::new(m.token_vault, false),
            AccountMeta::new(m.pot_vault, false),
            AccountMeta::new(ticket_key, false),
        ]
    );
    let quote = quoter.quote(&view(&env, &m), &request, &resolved).unwrap();
    assert_eq!(quote.amount_out, base);
    // Anonymous gacha quotes omit the ticket but still price the base.
    let anonymous = quote_request(&m, true, amount, None, vec![1]);
    let resolved_anonymous = quoter
        .resolve_accounts(&view(&env, &m), &anonymous, |_, _| {})
        .unwrap();
    assert_eq!(resolved_anonymous.len(), 2);
    assert_eq!(
        quoter
            .quote(&view(&env, &m), &anonymous, &resolved_anonymous)
            .unwrap()
            .amount_out,
        base
    );

    // Without an open ticket the swap fails.
    let accounts = swap_accounts(&m, &trader_key, true, true, &trader_quote, &trader_token);
    assert_fails_with(
        env.send(
            &[client::swap(&accounts, &gacha_args(amount, base))],
            &[&t.0],
        ),
        "WrongAccount",
    );

    let slot = current_slot(&env);
    let (got_base, got_pot) = gacha_buy(&mut env, &m, &t, amount);
    assert_eq!((got_base, got_pot), (base, pot));
    assert_eq!(env.token_balance(&trader_token), base);
    assert_eq!(env.token_balance(&m.pot_vault), pot);
    // Pipeline's fee is charged on the whole input, gacha fee included.
    assert_eq!(
        env.token_balance(&trader_quote),
        20_000_000_000 - amount - fees::pipeline_fee(amount)
    );
    assert_eq!(reserves(&env, &m), (amount, SUPPLY - base - pot));
    assert_eq!(ticket(&env, &ticket_key), Some((true, slot, base)));

    // A second gacha buy must wait for the ticket; plain buys still work.
    let accounts = swap_accounts(&m, &trader_key, true, true, &trader_quote, &trader_token);
    assert_fails_with(
        env.send(&[client::swap(&accounts, &gacha_args(amount, 0))], &[&t.0]),
        "TicketPending",
    );
    let (plain, _) = price_buy(&env, &m, amount, false);
    let accounts = swap_accounts(&m, &trader_key, true, false, &trader_quote, &trader_token);
    env.send(
        &[client::swap(&accounts, &exact_in(amount, plain))],
        &[&t.0],
    )
    .expect("plain buy with an armed ticket");

    // An armed ticket cannot be closed, only resolved.
    assert_fails_with(
        env.send(&[close_ticket_ix(&m, &trader_key)], &[&t.0]),
        "TicketArmed",
    );

    // The gacha flag means nothing on a sell, and unknown flags are rejected.
    let accounts = swap_accounts(&m, &trader_key, false, false, &trader_quote, &trader_token);
    assert_fails_with(
        env.send(
            &[client::swap(&accounts, &gacha_args(1_000_000, 0))],
            &[&t.0],
        ),
        "BadExtra",
    );
    let accounts = swap_accounts(&m, &trader_key, true, false, &trader_quote, &trader_token);
    let odd = client::SwapArgs {
        extra: vec![7],
        ..exact_in(1_000_000, 0)
    };
    assert_fails_with(
        env.send(&[client::swap(&accounts, &odd)], &[&t.0]),
        "BadExtra",
    );
}

#[test]
fn an_unarmed_ticket_can_be_closed_for_its_rent() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 1_000_000_000);
    let trader_key = t.0.pubkey();
    let ticket_key = ticket_pda(&m.curve, &trader_key);

    let before = env.lamports(&trader_key);
    env.send(&[open_ticket_ix(&m, &trader_key)], &[&t.0])
        .expect("open");
    assert_eq!(ticket(&env, &ticket_key), Some((false, 0, 0)));
    assert!(env.lamports(&trader_key) < before);
    env.send(&[close_ticket_ix(&m, &trader_key)], &[&t.0])
        .expect("close");
    assert_eq!(ticket(&env, &ticket_key), None);
    assert_eq!(env.lamports(&trader_key), before);

    // Only the trader can close their ticket.
    env.send(&[open_ticket_ix(&m, &trader_key)], &[&t.0])
        .expect("open again");
    let stranger = env.funded_keypair();
    let mut ix = close_ticket_ix(&m, &trader_key);
    ix.accounts[0] = AccountMeta::new(stranger.pubkey(), true);
    assert!(env.send(&[ix], &[&stranger]).is_err());
}

#[test]
fn tickets_resolve_against_the_first_block_hash_at_or_after_the_reveal_slot() {
    let mut env = Env::new();
    let m = setup(&mut env);

    // Several gacha buys build up the pot before the winner's.
    for _ in 0..3 {
        let other = trader(&mut env, &m, 10_000_000_000);
        gacha_buy(&mut env, &m, &other, 3_000_000_000);
    }
    let t = trader(&mut env, &m, 10_000_000_000);
    let (trader_key, _, trader_token) = (t.0.pubkey(), t.1, t.2);
    let ticket_key = ticket_pda(&m.curve, &trader_key);
    let buy_slot = current_slot(&env);
    let (stake, _) = gacha_buy(&mut env, &m, &t, 1_000_000_000);
    let reveal = buy_slot + REVEAL_DELAY;
    let pot_before = env.token_balance(&m.pot_vault);
    let rent = env.lamports(&ticket_key);
    let trader_lamports = env.lamports(&trader_key);

    // Before the reveal slot has a block, resolution is too early.
    set_slot_hashes(&mut env, &[(buy_slot, [9u8; 32])]);
    assert_fails_with(
        env.send(&[resolve_ticket_ix(&m, &trader_key, &trader_token)], &[]),
        "TooEarly",
    );
    env.svm.warp_to_slot(reveal + 3);
    assert_fails_with(
        env.send(&[resolve_ticket_ix(&m, &trader_key, &trader_token)], &[]),
        "TooEarly",
    );

    // The reveal slot was skipped; the next block's hash decides. Make it a win, and
    // make the exact reveal slot's would-be hash a loss to prove which one was used.
    let winning = hash_that(&ticket_key, buy_slot, true);
    let losing = hash_that(&ticket_key, buy_slot, false);
    set_slot_hashes(
        &mut env,
        &[
            (reveal + 2, winning),
            (reveal - 1, losing),
            (buy_slot, [9u8; 32]),
        ],
    );
    // Resolved by someone other than the trader (the test payer), as a keeper would.
    env.send(&[resolve_ticket_ix(&m, &trader_key, &trader_token)], &[])
        .expect("resolve");

    let bonus = stake as u128 * BONUS_BPS as u128 / 10_000;
    let cap = pot_before as u128 * MAX_WIN_POT_BPS as u128 / 10_000;
    let payout = bonus.min(cap) as u64;
    assert!(payout > 0);
    assert_eq!(env.token_balance(&trader_token), stake + payout);
    assert_eq!(env.token_balance(&m.pot_vault), pot_before - payout);
    // The ticket is closed and its rent returned to the trader, whoever resolved it.
    assert_eq!(ticket(&env, &ticket_key), None);
    assert_eq!(env.lamports(&trader_key), trader_lamports + rent);

    // The trader can play again.
    let (stake2, _) = gacha_buy(&mut env, &m, &t, 1_000_000_000);
    assert_eq!(ticket(&env, &ticket_key), Some((true, reveal + 3, stake2)));
}

#[test]
fn losing_tickets_pay_nothing_and_still_close() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 10_000_000_000);
    let (trader_key, _, trader_token) = (t.0.pubkey(), t.1, t.2);
    let ticket_key = ticket_pda(&m.curve, &trader_key);
    let buy_slot = current_slot(&env);
    let (stake, pot) = gacha_buy(&mut env, &m, &t, 1_000_000_000);
    let reveal = buy_slot + REVEAL_DELAY;

    env.svm.warp_to_slot(reveal + 1);
    set_slot_hashes(
        &mut env,
        &[(reveal, hash_that(&ticket_key, buy_slot, false))],
    );
    env.send(&[resolve_ticket_ix(&m, &trader_key, &trader_token)], &[])
        .expect("resolve a loss");
    assert_eq!(env.token_balance(&trader_token), stake);
    assert_eq!(env.token_balance(&m.pot_vault), pot);
    assert_eq!(ticket(&env, &ticket_key), None);

    // Resolving a closed ticket fails.
    assert!(env
        .send(&[resolve_ticket_ix(&m, &trader_key, &trader_token)], &[])
        .is_err());
}

#[test]
fn tickets_left_past_the_resolve_window_expire_without_a_payout() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 10_000_000_000);
    let (trader_key, _, trader_token) = (t.0.pubkey(), t.1, t.2);
    let ticket_key = ticket_pda(&m.curve, &trader_key);
    let buy_slot = current_slot(&env);
    let (stake, pot) = gacha_buy(&mut env, &m, &t, 1_000_000_000);
    let reveal = buy_slot + REVEAL_DELAY;

    // Even a winning hash pays nothing once the window has passed.
    env.svm.warp_to_slot(reveal + RESOLVE_WINDOW_SLOTS + 1);
    set_slot_hashes(
        &mut env,
        &[(reveal, hash_that(&ticket_key, buy_slot, true))],
    );
    env.send(&[resolve_ticket_ix(&m, &trader_key, &trader_token)], &[])
        .expect("expire");
    assert_eq!(env.token_balance(&trader_token), stake);
    assert_eq!(env.token_balance(&m.pot_vault), pot);
    assert_eq!(ticket(&env, &ticket_key), None);
}

#[test]
fn wins_are_paid_only_to_the_tickets_trader() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 10_000_000_000);
    let trader_key = t.0.pubkey();
    let ticket_key = ticket_pda(&m.curve, &trader_key);
    let buy_slot = current_slot(&env);
    gacha_buy(&mut env, &m, &t, 1_000_000_000);
    let reveal = buy_slot + REVEAL_DELAY;
    env.svm.warp_to_slot(reveal + 1);
    set_slot_hashes(
        &mut env,
        &[(reveal, hash_that(&ticket_key, buy_slot, true))],
    );

    // A keeper pointing the payout at their own token account is refused.
    let keeper = Pubkey::new_unique();
    let keeper_token = env.create_ata(&keeper, &m.token_mint, &TOKEN);
    assert!(env
        .send(&[resolve_ticket_ix(&m, &trader_key, &keeper_token)], &[])
        .is_err());
    assert_eq!(ticket(&env, &ticket_key).map(|t| t.0), Some(true));
}
