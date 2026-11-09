//! SOS audit #2, H032: a declared vault can be a router-owned account.
//!
//! `MarketInfo.vaults` are not validated, so a hostile market can declare as its input
//! vault (1) its own fee ATA, (2) the treasury ATA, (3) another market's fee ATA, or (4)
//! another market's pool vault. These tests check, for each, that
//!   * the trader is debited exactly `net_sent + fees` and gets what the strategy paid;
//!   * `SwapEvent.net_sent`/`pipeline_fee` are the real amounts (fee >= 25 bps of the
//!     input), whatever balance the declared vault held before the swap;
//!   * the strategy can't take anything back out of a router-owned account;
//!   * a sweep moves exactly the fee ATA balance, and `FeesSwept.amount` exceeds the sum
//!     of `SwapEvent.pipeline_fee` by exactly the trade input donated to the ATA (the same
//!     divergence a plain token transfer into the fee ATA causes, so a control checks it).

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_events::PipelineEvent;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

const HONEST: u8 = 0;
const REFUND_MORE: u8 = 3;
const STEAL_FEES: u8 = 6;

const AMOUNT: u64 = 1_000_000;
const OWNER_MISMATCH: &str = "owner does not match";

struct EvilMarket {
    state: Pubkey,
    market: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
}

fn evil_state(mint_a: &Pubkey, mint_b: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0
}

fn market_address(mint_a: &Pubkey, mint_b: &Pubkey) -> Pubkey {
    pda::market(&EVIL_ID, &evil_state(mint_a, mint_b))
}

/// Evil market on (mint_a, mint_b) that declares `vault_a` as its A vault, or its own
/// associated token account for A if `None`.
fn evil_market(
    env: &mut Env,
    mint_a: Pubkey,
    mint_b: Pubkey,
    vault_a: Option<Pubkey>,
) -> EvilMarket {
    if env.svm.get_account(&pda::strategy(&EVIL_ID)).is_none() {
        env.register(&EVIL_ID, "test_strategy_evil");
    }
    let state = evil_state(&mint_a, &mint_b);
    let market = pda::market(&EVIL_ID, &state);
    let vault_a = vault_a.unwrap_or_else(|| env.create_ata(&state, &mint_a, &TOKEN));
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
    env.send(&[create], &[&creator])
        .expect("create evil market");
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &market, &mint_a, &TOKEN),
            client::init_fee_account(&payer, &market, &mint_b, &TOKEN),
        ],
        &[],
    )
    .expect("init evil fee accounts");
    env.mint_to(&mint_b, &TOKEN, &vault_b, 1_000_000_000_000);
    assert_eq!(
        env.market(&market).vaults[0],
        vault_a,
        "setup: declared vault"
    );
    EvilMarket {
        state,
        market,
        mint_a,
        mint_b,
        vault_a,
        vault_b,
    }
}

fn evil_swap(
    m: &EvilMarket,
    trader: &Pubkey,
    trader_a: &Pubkey,
    trader_b: &Pubkey,
    behaviour: u8,
    extra: Vec<AccountMeta>,
) -> Instruction {
    let mut strategy_accounts = vec![AccountMeta::new(m.vault_b, false)];
    strategy_accounts.extend(extra);
    let accounts = client::SwapAccounts {
        trader: *trader,
        strategy_program: EVIL_ID,
        market_state: m.state,
        trader_input: *trader_a,
        trader_output: *trader_b,
        input_vault: m.vault_a,
        input_mint: m.mint_a,
        output_mint: m.mint_b,
        input_token_program: TOKEN,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts,
        input_hook_accounts: vec![],
    };
    let mut args = exact_in(AMOUNT, 0);
    args.extra = vec![behaviour];
    client::swap(&accounts, &args)
}

struct Trader {
    keys: solana_keypair::Keypair,
    a: Pubkey,
    b: Pubkey,
}

fn trader(env: &mut Env, m: &EvilMarket) -> Trader {
    let (keys, a) = env.trader_with(&m.mint_a, &TOKEN, 100 * AMOUNT);
    let b = env.create_ata(&keys.pubkey(), &m.mint_b, &TOKEN);
    Trader { keys, a, b }
}

fn fee_of(amount: u64) -> u64 {
    amount.div_ceil(400)
}

/// One honest swap of AMOUNT. Asserts the trader's debit, payout and SwapEvent, and
/// returns the event's `pipeline_fee`.
fn swap_and_check(env: &mut Env, m: &EvilMarket, t: &Trader, what: &str) -> u64 {
    let a_before = env.token_balance(&t.a);
    let b_before = env.token_balance(&t.b);
    let ix = evil_swap(m, &t.keys.pubkey(), &t.a, &t.b, HONEST, vec![]);
    let meta = env
        .send(&[ix], &[&t.keys])
        .unwrap_or_else(|e| panic!("H032 ({what}): swap failed: {e:?}"));
    let events = swap_events(&meta);
    assert_eq!(events.len(), 1, "H032 ({what}): one SwapEvent");
    let event = &events[0];
    assert_eq!(event.market, m.market);
    assert_eq!(event.net_sent, AMOUNT, "H032 ({what}): net_sent");
    assert_eq!(event.received, AMOUNT, "H032 ({what}): received");
    assert_eq!(event.pipeline_fee, fee_of(AMOUNT), "H032 ({what}): fee");
    assert_eq!(
        a_before - env.token_balance(&t.a),
        AMOUNT + event.pipeline_fee,
        "H032 ({what}): trader debit = net_sent + fee"
    );
    assert_eq!(
        env.token_balance(&t.b) - b_before,
        AMOUNT,
        "H032 ({what}): trader payout"
    );
    event.pipeline_fee
}

fn sweep(env: &mut Env, market: &Pubkey, mint: &Pubkey) -> u64 {
    let treasury_owner = env.treasury_owner;
    let ix = client::sweep_fees(market, mint, &TOKEN, &treasury_owner, vec![]);
    let meta = env.send(&[ix], &[]).expect("sweep");
    let swept: Vec<_> = events(&meta)
        .into_iter()
        .filter_map(|e| match e {
            PipelineEvent::FeesSwept(f) => Some(f),
            _ => None,
        })
        .collect();
    assert_eq!(swept.len(), 1, "one FeesSwept");
    assert_eq!(swept[0].market, *market);
    assert_eq!(swept[0].mint, *mint);
    swept[0].amount
}

fn mints(env: &mut Env) -> (Pubkey, Pubkey) {
    (
        env.create_mint(&TOKEN, 6, None, None),
        env.create_mint(&TOKEN, 6, None, None),
    )
}

/// (1) Own fee ATA as the vault. Fee base and trader debit stay exact; the fee ATA gains
/// input + fee per swap; the sweep's `FeesSwept.amount` is the whole balance, i.e. the sum
/// of SwapEvent fees plus the donated trade input. Nothing can be taken back out.
#[test]
fn own_fee_ata_as_vault() {
    let mut env = Env::new();
    let (x, z) = mints(&mut env);
    let own_fee = pda::fee_account(&market_address(&x, &z), &x, &TOKEN);
    let m = evil_market(&mut env, x, z, Some(own_fee));
    assert_eq!(m.vault_a, pda::fee_account(&m.market, &x, &TOKEN));
    let treasury_owner = env.treasury_owner;
    let treasury_x = env.create_ata(&treasury_owner, &x, &TOKEN);
    let t = trader(&mut env, &m);

    // The strategy can't debit the fee ATA (owned by the fee-authority PDA, not the market).
    // Pre-fund it so a failure is the authority check, not "insufficient funds".
    env.mint_to(&x, &TOKEN, &m.vault_a, 10 * AMOUNT);
    let prefund = env.token_balance(&m.vault_a);
    let before = prefund;
    for behaviour in [REFUND_MORE, STEAL_FEES] {
        let ix = evil_swap(
            &m,
            &t.keys.pubkey(),
            &t.a,
            &t.b,
            behaviour,
            vec![AccountMeta::new(m.vault_a, false)],
        );
        assert_fails_with(env.send(&[ix], &[&t.keys]), OWNER_MISMATCH);
        assert_eq!(
            env.token_balance(&m.vault_a),
            before,
            "H032: strategy moved tokens out of its own fee ATA"
        );
    }

    let mut fees = 0;
    for _ in 0..3 {
        fees += swap_and_check(&mut env, &m, &t, "own fee ATA");
    }
    assert_eq!(
        env.token_balance(&m.vault_a),
        prefund + 3 * AMOUNT + fees,
        "fee ATA holds donated input plus fees"
    );

    let swept = sweep(&mut env, &m.market, &x);
    assert_eq!(
        swept,
        prefund + 3 * AMOUNT + fees,
        "FeesSwept is the whole balance"
    );
    assert_eq!(env.token_balance(&m.vault_a), 0);
    assert_eq!(env.token_balance(&treasury_x), swept, "treasury got it all");
    // Observation: FeesSwept.amount - sum(SwapEvent.pipeline_fee) = the donated input.
    assert_eq!(swept - fees, prefund + 3 * AMOUNT);
}

/// Control: a plain transfer into a fee ATA opens the same gap between `FeesSwept.amount`
/// and the sum of `SwapEvent.pipeline_fee`, with no vault trick at all. So the gap is not
/// a new capability of declaring the fee ATA as a vault.
#[test]
fn control_plain_donation_to_fee_ata_has_the_same_effect() {
    let mut env = Env::new();
    let (x, z) = mints(&mut env);
    let m = evil_market(&mut env, x, z, None);
    let treasury_owner = env.treasury_owner;
    env.create_ata(&treasury_owner, &x, &TOKEN);
    let t = trader(&mut env, &m);
    let fee_ata = pda::fee_account(&m.market, &x, &TOKEN);
    let fee = swap_and_check(&mut env, &m, &t, "control");
    env.mint_to(&x, &TOKEN, &fee_ata, 12_345);
    let swept = sweep(&mut env, &m.market, &x);
    assert_eq!(swept, fee + 12_345);
}

/// (2) Treasury ATA as the vault: trade input is a donation to the treasury; the strategy
/// can't take it back; the market's own fee accounting and its sweep stay clean.
#[test]
fn treasury_ata_as_vault() {
    let mut env = Env::new();
    let (x, z) = mints(&mut env);
    let treasury_owner = env.treasury_owner;
    let treasury_x = env.create_ata(&treasury_owner, &x, &TOKEN);
    env.mint_to(&x, &TOKEN, &treasury_x, 777);
    let m = evil_market(&mut env, x, z, Some(treasury_x));
    let t = trader(&mut env, &m);
    let fee_ata = pda::fee_account(&m.market, &x, &TOKEN);

    for behaviour in [REFUND_MORE, STEAL_FEES] {
        let ix = evil_swap(
            &m,
            &t.keys.pubkey(),
            &t.a,
            &t.b,
            behaviour,
            vec![AccountMeta::new(treasury_x, false)],
        );
        assert_fails_with(env.send(&[ix], &[&t.keys]), OWNER_MISMATCH);
        assert_eq!(
            env.token_balance(&treasury_x),
            777,
            "H032: strategy moved tokens out of the treasury ATA"
        );
    }

    // Pre-existing treasury balance does not leak into the fee base.
    let mut fees = 0;
    for _ in 0..2 {
        fees += swap_and_check(&mut env, &m, &t, "treasury ATA");
    }
    assert_eq!(env.token_balance(&treasury_x), 777 + 2 * AMOUNT);
    assert_eq!(env.token_balance(&fee_ata), fees, "fee ATA holds only fees");

    // The market's sweep reports only its fees and lands them in the same treasury ATA.
    let swept = sweep(&mut env, &m.market, &x);
    assert_eq!(swept, fees, "FeesSwept = sum of SwapEvent fees");
    assert_eq!(env.token_balance(&treasury_x), 777 + 2 * AMOUNT + fees);
}

/// (3) Another market's fee ATA as the vault, and (4) another market's pool vault as the
/// vault. A second evil market M1 owns fee ATA F1; hostile M3 declares F1 as its vault, and
/// hostile M4 declares the honest CP market's vault. Swaps on M3/M4 never change the fee
/// base of swaps on M1/CP, each market's SwapEvent is exact, and M1's sweep reports the
/// whole F1 balance (M1's fees plus M3's donated input).
#[test]
fn other_markets_fee_ata_and_vault_as_vault() {
    let mut env = Env::new();
    let x = env.create_mint(&TOKEN, 6, None, None);
    let y = env.create_mint(&TOKEN, 6, None, None);
    let z1 = env.create_mint(&TOKEN, 6, None, None);
    let z3 = env.create_mint(&TOKEN, 6, None, None);
    let z4 = env.create_mint(&TOKEN, 6, None, None);
    let cp = setup_cp_market_with_mints(
        &mut env,
        x,
        TOKEN,
        y,
        TOKEN,
        1_000_000_000_000,
        1_000_000_000_000,
        30,
        vec![],
    );

    let treasury_owner = env.treasury_owner;
    env.create_ata(&treasury_owner, &x, &TOKEN);
    let m1 = evil_market(&mut env, x, z1, None);
    let f1 = pda::fee_account(&m1.market, &x, &TOKEN);
    let m3 = evil_market(&mut env, x, z3, Some(f1));
    let m4 = evil_market(&mut env, x, z4, Some(cp.vault(&x)));
    let t1 = trader(&mut env, &m1);
    let t3 = trader(&mut env, &m3);
    let t4 = trader(&mut env, &m4);

    // Strategies can't pull anything back out of F1 or the CP vault. Pre-fund F1 so the
    // failure is the authority check.
    env.mint_to(&x, &TOKEN, &f1, 10 * AMOUNT);
    let f1_prefund = env.token_balance(&f1);
    for (m, t, target) in [(&m3, &t3, f1), (&m4, &t4, cp.vault(&x))] {
        let ix = evil_swap(
            m,
            &t.keys.pubkey(),
            &t.a,
            &t.b,
            REFUND_MORE,
            vec![AccountMeta::new(target, false)],
        );
        assert_fails_with(env.send(&[ix], &[&t.keys]), OWNER_MISMATCH);
    }

    // M1 swap, then M3 donates into F1, then M1 swaps again: M1's second event is not
    // painted by M3's donation.
    let m1_fee_1 = swap_and_check(&mut env, &m1, &t1, "M1 first");
    let m3_fee = swap_and_check(&mut env, &m3, &t3, "M3 vault = F1");
    assert_eq!(env.token_balance(&f1), f1_prefund + m1_fee_1 + AMOUNT);
    let m1_fee_2 = swap_and_check(&mut env, &m1, &t1, "M1 second");

    // M3's own fees went to M3's fee ATA, not F1.
    assert_eq!(
        env.token_balance(&pda::fee_account(&m3.market, &x, &TOKEN)),
        m3_fee
    );
    let swept = sweep(&mut env, &m1.market, &x);
    assert_eq!(swept, f1_prefund + m1_fee_1 + m1_fee_2 + AMOUNT);

    // M4 donates into the CP vault; the CP market's next swap is measured on its own
    // window, so its event reflects only that swap.
    let cp_vault_before = env.token_balance(&cp.vault(&x));
    swap_and_check(&mut env, &m4, &t4, "M4 vault = CP vault");
    assert_eq!(env.token_balance(&cp.vault(&x)), cp_vault_before + AMOUNT);
    let (cp_trader, cp_x) = env.trader_with(&x, &TOKEN, 10 * AMOUNT);
    let cp_y = env.create_ata(&cp_trader.pubkey(), &y, &TOKEN);
    let accounts = cp_swap_accounts(&cp, &cp_trader.pubkey(), &x, &cp_x, &y, &cp_y);
    let meta = env
        .send(
            &[client::swap(&accounts, &exact_in(AMOUNT, 0))],
            &[&cp_trader],
        )
        .expect("cp swap");
    let events = swap_events(&meta);
    assert_eq!(events[0].net_sent, AMOUNT, "H032: CP fee base painted");
    assert_eq!(
        events[0].pipeline_fee,
        fee_of(AMOUNT),
        "H032: CP fee painted"
    );
}
