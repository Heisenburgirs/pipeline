//! SOS H022: playground CP canonical-pool squatting, mint_a == mint_b, and `deposit`
//! sandwiching.
//!
//! Convention: a test that demonstrates a bug fails on the SAFE assertion. All tests
//! here confirm that a defence (or an accepted-by-design property) holds, so they pass.

mod common;

use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use pipeline_strategy_interface::fees;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;

// ------------------------------------------------------------------ helpers

/// Creates a CP market (router `create_market`) and its fee accounts, with NO liquidity.
fn create_empty_pool(
    env: &mut Env,
    creator: &Keypair,
    mint_a: Pubkey,
    mint_b: Pubkey,
    fee_bps: u16,
) -> CpMarket {
    let ix = cp_create_market_ix(&creator.pubkey(), &mint_a, &TOKEN, &mint_b, &TOKEN, fee_bps);
    env.send(&[ix], &[creator]).expect("create_market");
    let pool = cp_pool(&mint_a, &mint_b, fee_bps);
    let market_address = pda::market(&CP_PROGRAM_ID, &pool);
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &market_address, &mint_a, &TOKEN),
            client::init_fee_account(&payer, &market_address, &mint_b, &TOKEN),
        ],
        &[],
    )
    .expect("fee accounts");
    CpMarket {
        pool,
        market: market_address,
        mint_a,
        mint_b,
        token_program_a: TOKEN,
        token_program_b: TOKEN,
        vault_a: pda::associated_token(&pool, &mint_a, &TOKEN),
        vault_b: pda::associated_token(&pool, &mint_b, &TOKEN),
    }
}

struct Actor {
    key: Keypair,
    ata_a: Pubkey,
    ata_b: Pubkey,
}

fn actor(env: &mut Env, market_a: &Pubkey, market_b: &Pubkey, amount: u64) -> Actor {
    let key = env.funded_keypair();
    let ata_a = env.create_ata(&key.pubkey(), market_a, &TOKEN);
    let ata_b = env.create_ata(&key.pubkey(), market_b, &TOKEN);
    env.mint_to(market_a, &TOKEN, &ata_a, amount);
    env.mint_to(market_b, &TOKEN, &ata_b, amount);
    Actor { key, ata_a, ata_b }
}

fn deposit(env: &mut Env, who: &Actor, market: &CpMarket, a: u64, b: u64) {
    let ix = cp_deposit_ix(
        &who.key.pubkey(),
        market,
        &who.ata_a,
        &who.ata_b,
        a,
        b,
        vec![],
    );
    env.send(&[ix], &[&who.key]).expect("deposit");
}

/// Swaps `amount_in` of A (or B) for the other token through the router.
fn swap(env: &mut Env, who: &Actor, market: &CpMarket, a_in: bool, amount_in: u64) -> u64 {
    let (in_mint, in_ata, out_mint, out_ata) = if a_in {
        (market.mint_a, who.ata_a, market.mint_b, who.ata_b)
    } else {
        (market.mint_b, who.ata_b, market.mint_a, who.ata_a)
    };
    let before = env.token_balance(&out_ata);
    let accounts = cp_swap_accounts(
        market,
        &who.key.pubkey(),
        &in_mint,
        &in_ata,
        &out_mint,
        &out_ata,
    );
    let ix = client::swap(&accounts, &exact_in(amount_in, 0));
    env.send(&[ix], &[&who.key]).expect("swap");
    env.token_balance(&out_ata) - before
}

/// Value of an actor's holdings at the external fair price of 1 A = 1 B.
fn value(env: &Env, who: &Actor) -> i128 {
    env.token_balance(&who.ata_a) as i128 + env.token_balance(&who.ata_b) as i128
}

// -------------------------------------------------------------- pure model

/// A CP pool with the strategy's pricing and the router's 0.25% fee on input.
#[derive(Clone, Copy)]
struct Model {
    ra: u64,
    rb: u64,
    fee_bps: u16,
}

impl Model {
    /// Swap `x` of one side in. Returns (out, platform fee).
    fn swap(&mut self, a_in: bool, x: u64) -> (u64, u64) {
        let platform = fees::pipeline_fee(x);
        if a_in {
            let out = cp_out(self.ra, self.rb, x, self.fee_bps);
            self.ra += x;
            self.rb -= out;
            (out, platform)
        } else {
            let out = cp_out(self.rb, self.ra, x, self.fee_bps);
            self.rb += x;
            self.ra -= out;
            (out, platform)
        }
    }

    fn deposit(&mut self, a: u64, b: u64) {
        self.ra += a;
        self.rb += b;
    }
}

/// Attacker value change at fair price 1:1 from: front-run (dir1, x), then (optionally)
/// the victim deposit, then back-run (dir2, z). Unwinding is implicit: leftover
/// inventory is valued at 1:1.
fn attack_pnl(
    pool: Model,
    front: (bool, u64),
    deposit: Option<(u64, u64)>,
    back: (bool, u64),
) -> i128 {
    let mut m = pool;
    let mut pnl: i128 = 0;
    let (out1, fee1) = m.swap(front.0, front.1);
    pnl += out1 as i128 - front.1 as i128 - fee1 as i128;
    if let Some((a, b)) = deposit {
        m.deposit(a, b);
    }
    // The back-run can only spend what it can afford; the model lets it go negative on
    // that side (valued at 1:1), which is generous to the attacker.
    let limit = if back.0 { m.rb } else { m.ra };
    let z = back.1;
    let out_cap = if back.0 {
        cp_out(m.ra, m.rb, z, m.fee_bps)
    } else {
        cp_out(m.rb, m.ra, z, m.fee_bps)
    };
    assert!(out_cap <= limit);
    let (out2, fee2) = m.swap(back.0, z);
    pnl += out2 as i128 - z as i128 - fee2 as i128;
    pnl
}

fn grid(max: u64) -> Vec<u64> {
    let mut v = vec![0u64];
    let mut x = 1_000u64;
    while x <= max {
        v.push(x);
        v.push(x * 3);
        x *= 10;
    }
    v
}

/// Best attacker PnL over (front, back) grids, with or without the victim deposit and
/// with front-running allowed or not.
fn best(
    pool: Model,
    deposit: Option<(u64, u64)>,
    allow_front: bool,
) -> (i128, (bool, u64), (bool, u64)) {
    let mut best = (0i128, (true, 0u64), (true, 0u64));
    let xs = if allow_front {
        grid(4_000_000_000)
    } else {
        vec![0]
    };
    let zs = grid(4_000_000_000);
    for &fd in &[true, false] {
        for &x in &xs {
            for &bd in &[true, false] {
                for &z in &zs {
                    // Skip trades that would drain a side.
                    let mut m = pool;
                    let out_front = if fd {
                        cp_out(m.ra, m.rb, x, m.fee_bps)
                    } else {
                        cp_out(m.rb, m.ra, x, m.fee_bps)
                    };
                    if out_front >= if fd { m.rb } else { m.ra } {
                        continue;
                    }
                    m.swap(fd, x);
                    if let Some((a, b)) = deposit {
                        m.deposit(a, b);
                    }
                    let out_back = if bd {
                        cp_out(m.ra, m.rb, z, m.fee_bps)
                    } else {
                        cp_out(m.rb, m.ra, z, m.fee_bps)
                    };
                    if out_back >= if bd { m.rb } else { m.ra } {
                        continue;
                    }
                    let pnl = attack_pnl(pool, (fd, x), deposit, (bd, z));
                    if pnl > best.0 {
                        best = (pnl, (fd, x), (bd, z));
                    }
                }
            }
        }
    }
    best
}

// ------------------------------------------------------------------- tests

/// (1) Squatting the canonical pool: the squatter picks the opening ratio, but cannot
/// block anything and cannot profit from the creator's later deposit.
fn run_squat(squat_a: u64, squat_b: u64, victim: u64) -> (i128, u64, u64) {
    let mut env = Env::new();
    env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let squatter = actor(&mut env, &mint_a, &mint_b, 100_000_000_000_000);
    let victim_actor = actor(&mut env, &mint_a, &mint_b, 10_000_000_000_000);

    let start = value(&env, &squatter);
    let pool = create_empty_pool(&mut env, &squatter.key, mint_a, mint_b, 30);

    // The creator who wanted this pair/fee can't create it: clean failure, nothing lost.
    let creator = env.funded_keypair();
    let before_sol = env.lamports(&creator.pubkey());
    let ix = cp_create_market_ix(&creator.pubkey(), &mint_a, &TOKEN, &mint_b, &TOKEN, 30);
    let res = env.send(&[ix], &[&creator]);
    assert!(
        res.is_err(),
        "second create_market for the same pool must fail"
    );
    assert_eq!(env.lamports(&creator.pubkey()), before_sol);
    // ... and another fee tier is open to them, so squatting blocks nothing.
    let ix = cp_create_market_ix(&creator.pubkey(), &mint_a, &TOKEN, &mint_b, &TOKEN, 31);
    env.send(&[ix], &[&creator])
        .expect("another fee tier is free");

    // Squatter sets the opening ratio. (An empty pool cannot be priced: one-sided
    // deposits are refused while a side is empty.)
    let one_sided = env.send(
        &[cp_deposit_ix(
            &squatter.key.pubkey(),
            &pool,
            &squatter.ata_a,
            &squatter.ata_b,
            squat_a,
            0,
            vec![],
        )],
        &[&squatter.key],
    );
    assert_fails_with(one_sided, "OneSidedDeposit");
    deposit(&mut env, &squatter, &pool, squat_a, squat_b);

    // The creator (or anyone) then adds their real liquidity to the squatted pool.
    deposit(&mut env, &victim_actor, &pool, victim, victim);
    let ra = env.token_balance(&pool.vault_a);
    let rb = env.token_balance(&pool.vault_b);
    println!("  squat=({squat_a},{squat_b}) victim=({victim},{victim}) -> reserves ({ra},{rb})");

    // The squatter now does the best he can: arbitrage the pool back to 1:1 with the
    // optimal single swap (A in if A is scarce relative to B, else B in).
    let model = Model {
        ra,
        rb,
        fee_bps: 30,
    };
    let a_in = (rb as u128) > (ra as u128);
    let mut best_x = 0u64;
    let mut best_gain = 0i128;
    let mut x = 1_000u64;
    while x < 200_000_000_000 {
        let mut m = model;
        let (out, fee) = m.swap(a_in, x);
        let gain = out as i128 - x as i128 - fee as i128;
        if gain > best_gain {
            best_gain = gain;
            best_x = x;
        }
        x = x * 3 / 2 + 1;
    }
    if best_x > 0 {
        swap(&mut env, &squatter, &pool, a_in, best_x);
    }
    let net = value(&env, &squatter) - start;
    println!("  squatter best arbitrage swap {best_x}, net PnL incl. opening deposit = {net}");
    (net, ra, rb)
}

#[test]
fn squatting_the_canonical_pool_blocks_nothing_and_loses_money() {
    println!("-- dust squat at a 1:1000 ratio, victim deposits 1e9 each");
    let (net_dust, ra, rb) = run_squat(1_000, 1_000_000, 1_000_000_000);
    // The dust is swamped by the real deposit: the price is within 0.1% of 1:1.
    let skew = (rb as f64 / ra as f64 - 1.0).abs();
    println!("  residual price skew {skew:.6}");
    assert!(skew < 0.002);
    assert!(net_dust <= 0, "a dust squat must not be profitable");

    println!("-- large squat at 1:1000 ratio (1e9 A, 1e12 B), victim deposits 1e9 each");
    let (net_big, _, _) = run_squat(1_000_000_000, 1_000_000_000_000, 1_000_000_000);
    assert!(net_big < 0, "a large mispriced squat must lose money");
}

/// (2) mint_a == mint_b cannot be created: the router refuses the market, and the
/// strategy refuses a call that the router did not sign.
#[test]
fn same_mint_market_cannot_be_created() {
    let mut env = Env::new();
    env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    let mint = env.create_mint(&TOKEN, 6, None, None);
    let creator = env.funded_keypair();
    let ix = cp_create_market_ix(&creator.pubkey(), &mint, &TOKEN, &mint, &TOKEN, 30);
    let res = env.send(&[ix], &[&creator]);
    match &res {
        Ok(_) => panic!("same-mint market was created"),
        Err(f) => println!("  same-mint create_market failed: {:?}", f.err),
    }
    // Anchor's duplicate-mutable-account constraint on vault_a/vault_b trips first (the
    // two ATAs are the same account); the router's own distinct-mints check in
    // `fetch_market_info` (BadMarketInfo) is the second line of defence.
    assert_fails_with(res, "ConstraintDuplicateMutableAccount");
    // The pool account must not exist (the failed tx rolled back).
    assert!(env.account_data(&cp_pool(&mint, &mint, 30)).is_empty());
}

#[test]
fn pipeline_init_market_cannot_be_called_without_the_router() {
    use pipeline_client::{AccountMeta, Instruction};
    let mut env = Env::new();
    env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    let mint = env.create_mint(&TOKEN, 6, None, None);
    let attacker = env.funded_keypair();
    let pool = cp_pool(&mint, &mint, 30);
    // Whatever signer the attacker supplies in the market-signer slot, it is not the
    // router's market PDA for this pool.
    let data = {
        let mut d = pipeline_strategy_interface::discriminator::INIT_MARKET.to_vec();
        let init = borsh::to_vec(&30u16).unwrap();
        d.extend_from_slice(&(init.len() as u32).to_le_bytes());
        d.extend_from_slice(&init);
        d
    };
    let accounts = vec![
        AccountMeta::new_readonly(attacker.pubkey(), true), // fake market_signer
        AccountMeta::new(pool, false),
        AccountMeta::new(attacker.pubkey(), true), // payer
        AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        AccountMeta::new_readonly(mint, false),
        AccountMeta::new_readonly(mint, false),
        AccountMeta::new(pda::associated_token(&pool, &mint, &TOKEN), false),
        AccountMeta::new(pda::associated_token(&pool, &mint, &TOKEN), false),
        AccountMeta::new_readonly(TOKEN, false),
        AccountMeta::new_readonly(TOKEN, false),
        AccountMeta::new_readonly(client::ASSOCIATED_TOKEN_PROGRAM_ID, false),
    ];
    let ix = Instruction::new_with_bytes(CP_PROGRAM_ID, &data, accounts);
    let res = env.send(&[ix], &[&attacker]);
    match &res {
        Ok(_) => panic!("direct pipeline_init_market succeeded"),
        Err(f) => println!("  direct init failed: {:?}", f.err),
    }
    assert!(env.account_data(&pool).is_empty());
}

/// (3a) End-to-end sandwich of a CP `deposit`: with a deposit at the pool's own ratio
/// the sandwich loses money to price impact and fees.
#[test]
fn sandwiching_a_fair_ratio_deposit_loses_money_e2e() {
    let mut env = Env::new();
    env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let lp = actor(&mut env, &mint_a, &mint_b, 10_000_000_000_000);
    let victim = actor(&mut env, &mint_a, &mint_b, 10_000_000_000_000);
    let attacker = actor(&mut env, &mint_a, &mint_b, 10_000_000_000_000);
    let creator = env.funded_keypair();
    let pool = create_empty_pool(&mut env, &creator, mint_a, mint_b, 30);
    deposit(&mut env, &lp, &pool, 1_000_000_000, 1_000_000_000);

    let start = value(&env, &attacker);
    let victim_start = value(&env, &victim);

    // Front-run: skew the price by buying B with 500M A.
    let got_b = swap(&mut env, &attacker, &pool, true, 500_000_000);
    // Victim deposit lands, at the ratio they computed against the old price (1:1).
    deposit(&mut env, &victim, &pool, 100_000_000, 100_000_000);
    // Back-run: sell all B back.
    let back_a = swap(&mut env, &attacker, &pool, false, got_b);
    let net = value(&env, &attacker) - start;
    let victim_loss = victim_start - value(&env, &victim);
    println!("  front-run bought {got_b} B; back-run returned {back_a} A; attacker net {net}");
    println!("  victim paid exactly {victim_loss} (signed amounts: 200000000)");
    assert!(net < 0, "sandwich of a fair-ratio deposit must not profit");
    assert_eq!(
        victim_loss, 200_000_000,
        "victim loses only what they signed"
    );
}

/// (3b) Model sweep: a deposit at the pool's own ratio is never profitable to sandwich;
/// for lopsided/stale deposits the attacker's gain is bounded by the donation itself
/// (and the front-run edge over a plain back-run is printed).
#[test]
fn sandwich_profit_is_bounded_by_the_donation_itself() {
    let pool = Model {
        ra: 1_000_000_000,
        rb: 1_000_000_000,
        fee_bps: 30,
    };
    let dep = 100_000_000u64;

    let (fair_sandwich, f, b) = best(pool, Some((dep, dep)), true);
    println!("  fair-ratio deposit: best sandwich PnL = {fair_sandwich} (front {f:?}, back {b:?})");
    assert_eq!(
        fair_sandwich, 0,
        "no profitable sandwich of a fair-ratio deposit"
    );

    // Depositors whose ratio is stale or deliberately lopsided (incl. one-sided).
    for (a, b_amt) in [
        (dep, 2 * dep),
        (2 * dep, dep),
        (dep, 4 * dep),
        (dep, 0),
        (0, dep),
        (dep, dep / 4),
    ] {
        let (sandwich, sf, sb) = best(pool, Some((a, b_amt)), true);
        let (backrun_only, _, _) = best(pool, Some((a, b_amt)), false);
        println!(
            "  deposit ({a},{b_amt}): sandwich best {sandwich} (front {sf:?} back {sb:?}), \
             back-run-only best {backrun_only}, front-run advantage {}",
            sandwich - backrun_only
        );
        // Informational: a front-run can add to what a plain back-run captures, but only
        // out of the depositor's own (irrevocable, no-shares) donation.
        // And the captured value never exceeds the value the depositor put in.
        assert!(sandwich <= (a + b_amt) as i128);
        assert!(sandwich < (a + b_amt) as i128 / 2 || (a + b_amt) == 0);
    }
}
