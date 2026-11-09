//! SOS audit #2, H014: in a multi-hop route every later hop's `amount_in` is fixed at
//! signing. If the trader already holds the intermediate mint in the account that is hop 1's
//! output and hop 2's input, and hop 1 has no minimum of its own, an under-delivering
//! (sandwiched) hop 1 is silently topped up from that balance and the route still succeeds.
//!
//! Status: fixed in the guidance (`docs/INTEGRATOR-GUIDE.md` §3, multi-hop): every hop gets
//! its own `min_amount_out` (its quote less slippage), with the end-to-end minimum on the
//! last. The router enforces each hop's minimum exactly; these tests show the guidance
//! refuses the sandwich, and pin the zero-first-hop-minimum backfill it exists to prevent.

mod common;

use common::*;
use pipeline_client::{self as client, Pubkey};
use pipeline_strategy_interface::fees;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;

const POOL: u64 = 10_000_000;
const HOP1_IN: u64 = 1_000_000;
/// The trader's working balance of the intermediate mint (a desk or bot float).
const PRE_HELD_B: u64 = 5_000_000;
/// The sandwicher's front-run, A→B on hop 1's market.
const FRONT_RUN_A: u64 = 30_000_000;
const POOL_FEE_BPS: u16 = 30;

fn hop1_budget() -> u64 {
    fees::max_total_spend(HOP1_IN, 0).unwrap()
}

struct Route {
    env: Env,
    ab: CpMarket,
    bc: CpMarket,
    mint_c: Pubkey,
    trader: Keypair,
    trader_a: Pubkey,
    trader_c: Pubkey,
    attacker: Keypair,
    attacker_a: Pubkey,
    attacker_b: Pubkey,
    /// Hop 2's `amount_in`, fixed at signing from the quoted hop-1 output.
    second_in: u64,
    /// The quoted end-to-end output, used as the last hop's minimum.
    second_out: u64,
}

fn setup() -> Route {
    let mut env = Env::new();
    let ab = setup_cp_market(&mut env, TOKEN, TOKEN, POOL, POOL, POOL_FEE_BPS);
    let mint_c = env.create_mint(&TOKEN, 6, None, None);
    let bc = setup_cp_market_with_mints(
        &mut env,
        ab.mint_b,
        TOKEN,
        mint_c,
        TOKEN,
        POOL,
        POOL,
        POOL_FEE_BPS,
        vec![],
    );

    // Enough A for hop 1 and its fee.
    let (trader, trader_a) = env.trader_with(&ab.mint_a, &TOKEN, hop1_budget());
    let trader_c = env.create_ata(&trader.pubkey(), &mint_c, &TOKEN);

    let (attacker, attacker_a) = env.trader_with(&ab.mint_a, &TOKEN, 100_000_000);
    let attacker_b = env.create_ata(&attacker.pubkey(), &ab.mint_b, &TOKEN);

    // The route the guide describes, quoted at the current (un-skewed) reserves.
    let first_out = cp_out(POOL, POOL, HOP1_IN, POOL_FEE_BPS);
    let second_in = fees::amount_for_budget(first_out, 0);
    let second_out = cp_out(POOL, POOL, second_in, POOL_FEE_BPS);
    assert!(second_in > 0 && second_out > 0);

    Route {
        env,
        ab,
        bc,
        mint_c,
        trader,
        trader_a,
        trader_c,
        attacker,
        attacker_a,
        attacker_b,
        second_in,
        second_out,
    }
}

impl Route {
    fn hop1(&self, trader_b: &Pubkey, min_amount_out: u64) -> client::Instruction {
        let accounts = cp_swap_accounts(
            &self.ab,
            &self.trader.pubkey(),
            &self.ab.mint_a,
            &self.trader_a,
            &self.ab.mint_b,
            trader_b,
        );
        client::swap(&accounts, &exact_in(HOP1_IN, min_amount_out))
    }

    fn hop2(&self, trader_b: &Pubkey) -> client::Instruction {
        let accounts = cp_swap_accounts(
            &self.bc,
            &self.trader.pubkey(),
            &self.ab.mint_b,
            trader_b,
            &self.mint_c,
            &self.trader_c,
        );
        client::swap(&accounts, &exact_in(self.second_in, self.second_out))
    }

    /// The sandwicher's front-run: an ordinary A→B swap on hop 1's market, ordered before
    /// the trader's route.
    fn front_run(&mut self) {
        let accounts = cp_swap_accounts(
            &self.ab,
            &self.attacker.pubkey(),
            &self.ab.mint_a,
            &self.attacker_a,
            &self.ab.mint_b,
            &self.attacker_b,
        );
        let ix = client::swap(&accounts, &exact_in(FRONT_RUN_A, 0));
        let attacker = self.attacker.insecure_clone();
        self.env
            .send(&[ix], &[&attacker])
            .expect("front-run succeeds");
        assert!(self.env.token_balance(&self.attacker_b) > 0);
    }

    fn reserves_ab(&self) -> (u64, u64) {
        (
            self.env.token_balance(&self.ab.vault_a),
            self.env.token_balance(&self.ab.vault_b),
        )
    }
}

/// The guidance: hop 1's minimum is its quote less slippage. The sandwiched route reverts on
/// hop 1 (`InsufficientOutput`) and the pre-held intermediate balance is untouched; the
/// un-sandwiched route still succeeds and never dips below the pre-held B.
#[test]
fn h014_first_hop_minimum_refuses_sandwiched_route() {
    let hop1_quote = cp_out(POOL, POOL, HOP1_IN, POOL_FEE_BPS);
    let hop1_min = hop1_quote - hop1_quote / 100;

    // Sandwiched: reverts, nothing consumed.
    let mut r = setup();
    let trader_b = r.env.create_ata(&r.trader.pubkey(), &r.ab.mint_b, &TOKEN);
    let mint_b = r.ab.mint_b;
    r.env.mint_to(&mint_b, &TOKEN, &trader_b, PRE_HELD_B);
    r.front_run();
    let route = [r.hop1(&trader_b, hop1_min), r.hop2(&trader_b)];
    let trader = r.trader.insecure_clone();
    assert_fails_with(r.env.send(&route, &[&trader]), "InsufficientOutput");
    assert_eq!(r.env.token_balance(&r.trader_a), hop1_budget());
    assert_eq!(r.env.token_balance(&trader_b), PRE_HELD_B);
    assert_eq!(r.env.token_balance(&r.trader_c), 0);

    // Not sandwiched: succeeds, the pre-held B is never consumed.
    let mut r = setup();
    let trader_b = r.env.create_ata(&r.trader.pubkey(), &r.ab.mint_b, &TOKEN);
    let mint_b = r.ab.mint_b;
    r.env.mint_to(&mint_b, &TOKEN, &trader_b, PRE_HELD_B);
    let route = [r.hop1(&trader_b, hop1_min), r.hop2(&trader_b)];
    let trader = r.trader.insecure_clone();
    r.env
        .send(&route, &[&trader])
        .expect("un-sandwiched route succeeds");
    assert_eq!(r.env.token_balance(&r.trader_a), 0);
    assert!(r.env.token_balance(&trader_b) >= PRE_HELD_B);
    assert_eq!(r.env.token_balance(&r.trader_c), r.second_out);
}

/// Pinned demonstration of why the guidance matters: with no minimum on hop 1 (only the
/// end-to-end minimum on hop 2) and a pre-held intermediate balance in the same account, a
/// sandwiched hop 1 that delivers far less than hop 2 spends is topped up from the trader's
/// existing B and the route succeeds. The router behaves as specified (each hop's own
/// minimum is enforced); the integrator must not leave hop 1's minimum at zero.
#[test]
fn h014_zero_first_hop_minimum_backfills_sandwiched_hop_from_preheld_balance() {
    let mut r = setup();
    let trader_b = r.env.create_ata(&r.trader.pubkey(), &r.ab.mint_b, &TOKEN);
    let mint_b = r.ab.mint_b;
    r.env.mint_to(&mint_b, &TOKEN, &trader_b, PRE_HELD_B);
    r.front_run();

    // What hop 1 will now actually deliver at the skewed reserves.
    let (reserve_a, reserve_b) = r.reserves_ab();
    let hop1_delivers = cp_out(reserve_a, reserve_b, HOP1_IN, POOL_FEE_BPS);
    let hop2_spends = fees::max_total_spend(r.second_in, 0).unwrap();
    assert!(
        hop1_delivers * 10 < hop2_spends,
        "setup: the front-run must make hop 1 deliver far less than hop 2 spends"
    );

    let route = [r.hop1(&trader_b, 0), r.hop2(&trader_b)];
    let trader = r.trader.insecure_clone();
    r.env
        .send(&route, &[&trader])
        .expect("the route with no hop-1 minimum succeeds");

    let backfilled = PRE_HELD_B - r.env.token_balance(&trader_b);
    assert_eq!(backfilled, hop2_spends - hop1_delivers);
    assert_eq!(r.env.token_balance(&r.trader_a), 0);
    assert_eq!(r.env.token_balance(&r.trader_c), r.second_out);
}

/// Control: the same sandwich against the same route with a fresh, empty intermediate
/// account (the case `router.rs::two_hop_route_in_one_transaction_pays_each_hop` covers).
/// Hop 2 can't spend B the route didn't produce, so the whole route reverts.
#[test]
fn h014_control_fresh_intermediate_account_reverts_sandwiched_route() {
    let mut r = setup();
    let trader_b = r.env.create_ata(&r.trader.pubkey(), &r.ab.mint_b, &TOKEN);
    r.front_run();

    let route = [r.hop1(&trader_b, 0), r.hop2(&trader_b)];
    let trader = r.trader.insecure_clone();
    assert!(r.env.send(&route, &[&trader]).is_err());
    assert_eq!(r.env.token_balance(&r.trader_a), hop1_budget());
    assert_eq!(r.env.token_balance(&trader_b), 0);
    assert_eq!(r.env.token_balance(&r.trader_c), 0);
}
