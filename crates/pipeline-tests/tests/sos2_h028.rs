//! SOS2 H028: fee completeness on the generic path after `settle` moved into the
//! interface, against the CP strategy (which credits through `credited_input`).
//!
//! Property: no sequence of pre-deposits, donations, dust swaps, ExactOut with an
//! overstated `amount_in`, or hops lets a trader trade value V through Pipeline for
//! less than `ceil(25 bps x V)`, where V is the input the strategy actually priced
//! (measured from the vault, not from what the router or strategy claims).
//!
//! Every test asserts the safe behaviour. A failure names H028.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::wallet::NATIVE_MINT;
use pipeline_client::{self as client, pda, Instruction, Pubkey};
use pipeline_strategy_interface::{fees, TradeMode};
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const UNIT: u64 = 1_000_000;
const FEE_BPS: u16 = 30;
const FEE_BPS_2: u16 = 100;

fn compute(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction::new_with_bytes(
        client::pubkey!("ComputeBudget111111111111111111111111111111"),
        &data,
        vec![],
    )
}

fn args(mode: TradeMode, amount_in: u64, amount_out: u64, min_out: u64) -> client::SwapArgs {
    client::SwapArgs {
        mode,
        amount_in,
        amount_out,
        min_amount_out: min_out,
        integrator_fee_bps: 0,
        input_hook_accounts: 0,
        extra: vec![],
    }
}

fn donate(
    from: &Pubkey,
    mint: &Pubkey,
    vault: &Pubkey,
    owner: &Pubkey,
    amount: u64,
    decimals: u8,
) -> Instruction {
    spl_token_2022_interface::instruction::transfer_checked(
        &TOKEN,
        from,
        mint,
        vault,
        owner,
        &[],
        amount,
        decimals,
    )
    .unwrap()
}

/// A token/wSOL CP market plus the donations made to each vault (so the strategy's
/// recorded reserve is `vault - donated`).
struct Venue {
    m: CpMarket,
    /// [token, wSOL]
    donated: [u64; 2],
}

const T: usize = 0;
const Q: usize = 1;

impl Venue {
    fn new(env: &mut Env, token_mint: Pubkey, fee_bps: u16) -> Venue {
        let m = setup_cp_market_with_mints(
            env,
            token_mint,
            TOKEN,
            NATIVE_MINT,
            TOKEN,
            1_000_000 * UNIT,
            100 * SOL,
            fee_bps,
            vec![],
        );
        Venue { m, donated: [0; 2] }
    }
    fn token_mint(&self) -> Pubkey {
        self.m.mint_a
    }
    fn vault(&self, side: usize) -> Pubkey {
        if side == T {
            self.m.vault_a
        } else {
            self.m.vault_b
        }
    }
    fn reserve(&self, env: &Env, side: usize) -> u64 {
        env.token_balance(&self.vault(side)) - self.donated[side]
    }
    fn fee_account(&self, input: &Pubkey) -> Pubkey {
        pda::associated_token(&pda::fee_authority(&self.m.market), input, &TOKEN)
    }
    fn accounts(&self, trader: &Trader, buying: bool) -> client::SwapAccounts {
        let (i, o) = if buying {
            (NATIVE_MINT, self.token_mint())
        } else {
            (self.token_mint(), NATIVE_MINT)
        };
        let (ia, oa) = if buying {
            (trader.wsol, trader.token)
        } else {
            (trader.token, trader.wsol)
        };
        cp_swap_accounts(&self.m, &trader.kp.pubkey(), &i, &ia, &o, &oa)
    }
    fn donate_ix(&self, trader: &Trader, buying: bool, amount: u64) -> Instruction {
        if buying {
            donate(
                &trader.wsol,
                &NATIVE_MINT,
                &self.vault(Q),
                &trader.kp.pubkey(),
                amount,
                9,
            )
        } else {
            donate(
                &trader.token,
                &self.token_mint(),
                &self.vault(T),
                &trader.kp.pubkey(),
                amount,
                6,
            )
        }
    }
}

struct Trader {
    kp: Keypair,
    wsol: Pubkey,
    token: Pubkey,
}

fn new_trader(env: &mut Env, token_mint: &Pubkey, wsol: u64) -> Trader {
    let kp = env.funded_keypair();
    let w = env.create_ata(&kp.pubkey(), &NATIVE_MINT, &TOKEN);
    env.mint_to(&NATIVE_MINT, &TOKEN, &w, wsol);
    let t = env.create_ata(&kp.pubkey(), token_mint, &TOKEN);
    Trader {
        kp,
        wsol: w,
        token: t,
    }
}

fn buy_big(env: &mut Env, v: &Venue, tr: &Trader, lamports: u64) {
    let ix = client::swap(
        &v.accounts(tr, true),
        &args(TradeMode::ExactIn, lamports, 0, 0),
    );
    env.send(&[compute(400_000), ix], &[&tr.kp])
        .expect("setup buy must succeed");
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

// ---------------------------------------------------------- (a) pre-deposit, 1 unit

/// Move almost all of the intended input straight into the vault earlier in the same
/// transaction, then swap 1 unit through Pipeline. The strategy must credit 1 (so
/// price(1) is paid out) and Pipeline must take the fee on the whole 1 (>= 1 unit).
#[test]
fn predeposit_then_one_unit_swap_credits_one_and_pays_fee() {
    let mut env = Env::new();
    env.svm.airdrop(&env.admin.pubkey(), 3_000 * SOL).unwrap();
    let mint = env.create_mint(&TOKEN, 6, None, None);
    let mut v = Venue::new(&mut env, mint, FEE_BPS);
    let tr = new_trader(&mut env, &mint, 90 * SOL);
    buy_big(&mut env, &v, &tr, 20 * SOL);

    for buying in [true, false] {
        let (side, other) = if buying { (Q, T) } else { (T, Q) };
        let (in_acct, out_acct) = if buying {
            (tr.wsol, tr.token)
        } else {
            (tr.token, tr.wsol)
        };
        let in_mint = if buying { NATIVE_MINT } else { mint };
        let fee_acct = v.fee_account(&in_mint);
        for pre in [
            1_000u64,
            1_000_000,
            if buying { 5 * SOL } else { 5_000 * UNIT },
        ] {
            let ri = v.reserve(&env, side);
            let ro = v.reserve(&env, other);
            let expected_out = cp_out(ri, ro, 1, FEE_BPS);
            let fee0 = env.token_balance(&fee_acct);
            let in0 = env.token_balance(&in_acct);
            let out0 = env.token_balance(&out_acct);
            let vault0 = env.token_balance(&v.vault(side));
            let ixs = vec![
                compute(400_000),
                v.donate_ix(&tr, buying, pre),
                client::swap(&v.accounts(&tr, buying), &args(TradeMode::ExactIn, 1, 0, 0)),
            ];
            let meta = env
                .send(&ixs, &[&tr.kp])
                .expect("setup: pre-deposit + 1 unit swap must succeed");
            v.donated[side] += pre;
            let ev = swap_events(&meta).pop().expect("swap event");
            let fee = env.token_balance(&fee_acct) - fee0;
            let got = env.token_balance(&out_acct) - out0;
            let spent = in0 - env.token_balance(&in_acct);
            let vault_gain = env.token_balance(&v.vault(side)) - vault0;
            println!(
                "[H028 predeposit buying={buying}] pre={pre} net_sent={} fee={fee} out={got} price(1)={expected_out} spent={spent}",
                ev.net_sent
            );
            assert_eq!(ev.net_sent, 1, "H028: net_sent must be the router-sent 1");
            assert!(
                fee >= fees::pipeline_fee(1),
                "H028: fee below ceil(25bps*1)"
            );
            assert_eq!(fee, ev.pipeline_fee);
            assert_eq!(
                got, expected_out,
                "H028: output must be price(1), not price of pre-deposit"
            );
            assert_eq!(spent, pre + 1 + fee, "H028: trader's spend ledger");
            assert_eq!(vault_gain, pre + 1);
            // Reserve ledger grew by exactly 1 beyond the donation.
            assert_eq!(v.reserve(&env, side), ri + 1, "H028: reserve credited != 1");
        }
    }
}

// --------------------------------------------------- (b)+(c) randomised swap trace

/// A deterministic pseudo-random trace of exact-in dust and ordinary swaps, ExactOut
/// swaps with a heavily overstated `amount_in`, standing and in-transaction
/// donations, in both directions. For every swap, the input the strategy priced is
/// measured from the vault (vault delta minus the donation), and Pipeline's fee must
/// be at least `ceil(25 bps x priced)`, and the fee account must receive it.
#[test]
fn random_trace_fee_covers_priced_input() {
    let mut env = Env::new();
    env.svm.airdrop(&env.admin.pubkey(), 3_000 * SOL).unwrap();
    let mint = env.create_mint(&TOKEN, 6, None, None);
    let mut v = Venue::new(&mut env, mint, FEE_BPS);
    let tr = new_trader(&mut env, &mint, 90 * SOL);
    buy_big(&mut env, &v, &tr, 20 * SOL);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    let mut priced_total = [0u64; 2];
    let mut fee_total = [0u64; 2];
    let mut swaps = 0u32;
    for step in 0..70 {
        let buying = rng.next().is_multiple_of(2);
        let (side, other) = if buying { (Q, T) } else { (T, Q) };
        let (in_acct, out_acct) = if buying {
            (tr.wsol, tr.token)
        } else {
            (tr.token, tr.wsol)
        };
        let in_mint = if buying { NATIVE_MINT } else { mint };
        let fee_acct = v.fee_account(&in_mint);
        let vault_in = v.vault(side);

        // Standing donation now and then.
        if rng.next().is_multiple_of(5) {
            let d = rng.pick(&[1u64, 77, 100_000, 3_000_000]);
            env.send(&[v.donate_ix(&tr, buying, d)], &[&tr.kp])
                .expect("setup: standing donation");
            v.donated[side] += d;
        }
        let tx_don = rng.pick(&[0u64, 0, 1, 399, 10_000, 2_000_000]);
        let exact_out = rng.next().is_multiple_of(3);
        let amount = rng.pick(&[1u64, 2, 3, 399, 400, 401, 1_000, 12_345, 1_234_567]);

        let ri = v.reserve(&env, side);
        let ro = v.reserve(&env, other);
        let (a, expected_priced, expected_out) = if exact_out {
            let want = amount.min(ro / 2);
            let need = cp_in(ri, ro, want, FEE_BPS);
            let max_in = need * 40 + 100;
            (args(TradeMode::ExactOut, max_in, want, want), need, want)
        } else {
            (
                args(TradeMode::ExactIn, amount, 0, 0),
                amount,
                cp_out(ri, ro, amount, FEE_BPS),
            )
        };
        let mut ixs = vec![compute(400_000)];
        if tx_don > 0 {
            ixs.push(v.donate_ix(&tr, buying, tx_don));
        }
        ixs.push(client::swap(&v.accounts(&tr, buying), &a));

        let fee0 = env.token_balance(&fee_acct);
        let in0 = env.token_balance(&in_acct);
        let out0 = env.token_balance(&out_acct);
        let vault0 = env.token_balance(&vault_in);
        let meta = match env.send(&ixs, &[&tr.kp]) {
            Ok(m) => m,
            Err(e) => {
                // A refusal moves nothing; it is not a bypass.
                println!("[H028 step {step}] refused: {:?}", e.err);
                assert_eq!(env.token_balance(&in_acct), in0);
                assert_eq!(env.token_balance(&vault_in), vault0);
                continue;
            }
        };
        v.donated[side] += tx_don;
        swaps += 1;
        let ev = swap_events(&meta).pop().expect("swap event");
        let fee_paid = env.token_balance(&fee_acct) - fee0;
        let got = env.token_balance(&out_acct) - out0;
        let spent = in0 - env.token_balance(&in_acct);
        let priced = (env.token_balance(&vault_in) - vault0) - tx_don;
        println!(
            "[H028 step {step} buying={buying} exact_out={exact_out}] amount={amount} tx_don={tx_don} priced={priced} fee={fee_paid} out={got}"
        );
        assert_eq!(
            priced, expected_priced,
            "H028: strategy priced a different input"
        );
        assert_eq!(got, expected_out);
        assert!(priced >= 1 || got == 0, "H028: output on zero priced input");
        assert!(
            fee_paid >= fees::pipeline_fee(priced),
            "H028: fee {fee_paid} below ceil(25bps x priced {priced})"
        );
        assert_eq!(fee_paid, ev.pipeline_fee, "H028: event vs fee account");
        assert_eq!(ev.net_sent, priced, "H028: net_sent vs priced");
        assert_eq!(
            spent,
            tx_don + priced + fee_paid,
            "H028: trader spend ledger"
        );
        priced_total[side] += priced;
        fee_total[side] += fee_paid;
    }
    assert!(swaps >= 40, "trace too sparse: {swaps} swaps ran");
    for s in [T, Q] {
        assert!(
            fee_total[s] >= fees::pipeline_fee(priced_total[s]),
            "H028: aggregate fee {} below ceil(25bps x total priced {})",
            fee_total[s],
            priced_total[s]
        );
    }
}

// ------------------------------------------------------------------ (b) dust splitting

/// 250 swaps of 100 units (a fee of 1 each, 100% of nothing saved) against one swap of
/// 25,000: the split pays at least as much fee in total, and gets no more output.
#[test]
fn dust_splitting_pays_at_least_the_whole_swap_fee() {
    let mut env = Env::new();
    env.svm.airdrop(&env.admin.pubkey(), 3_000 * SOL).unwrap();
    let mint = env.create_mint(&TOKEN, 6, None, None);
    let v = Venue::new(&mut env, mint, FEE_BPS);
    let tr = new_trader(&mut env, &mint, 90 * SOL);
    let fee_acct = v.fee_account(&NATIVE_MINT);
    let (n, each) = (250u64, 100u64);
    let total = n * each;
    let single_out = cp_out(v.reserve(&env, Q), v.reserve(&env, T), total, FEE_BPS);
    let fee0 = env.token_balance(&fee_acct);
    let out0 = env.token_balance(&tr.token);
    for i in 0..n {
        let ix = client::swap(
            &v.accounts(&tr, true),
            &args(TradeMode::ExactIn, each, 0, 0),
        );
        let meta = env
            .send(&[compute(400_000), ix], &[&tr.kp])
            .unwrap_or_else(|e| panic!("setup: dust swap {i} failed {:?}", e.err));
        let ev = swap_events(&meta).pop().unwrap();
        assert_eq!(ev.net_sent, each);
    }
    let fee = env.token_balance(&fee_acct) - fee0;
    let out = env.token_balance(&tr.token) - out0;
    println!(
        "[H028 dust] {n} x {each}: fee={fee} vs single fee {} ; out split={out} single={single_out}",
        fees::pipeline_fee(total)
    );
    assert!(
        fee >= fees::pipeline_fee(total),
        "H028: splitting paid less fee than one swap"
    );
    assert!(
        out <= single_out + n,
        "H028: splitting extracted more output"
    );
}

// ------------------------------------------------------------------------ (d) multi-hop

/// wSOL -> token on one CP market and token -> wSOL on a second CP market (different
/// pool fee) in a single transaction, with a donation to each hop's input vault.
/// Each hop pays `ceil(25 bps)` of its own input, so the round trip cannot avoid fees.
#[test]
fn two_hop_round_trip_pays_every_hop() {
    let mut env = Env::new();
    env.svm.airdrop(&env.admin.pubkey(), 3_000 * SOL).unwrap();
    let mint = env.create_mint(&TOKEN, 6, None, None);
    let v1 = Venue::new(&mut env, mint, FEE_BPS);
    let v2 = Venue::new(&mut env, mint, FEE_BPS_2);
    let tr = new_trader(&mut env, &mint, 90 * SOL);

    let hop1_in = 3 * SOL;
    let hop1_out = cp_out(v1.reserve(&env, Q), v1.reserve(&env, T), hop1_in, FEE_BPS);
    let hop2_in = hop1_out - hop1_out / 100;
    let d1 = 12_345u64;
    let d2 = 4_321u64;
    let ixs = vec![
        compute(1_000_000),
        v1.donate_ix(&tr, true, d1),
        client::swap(
            &v1.accounts(&tr, true),
            &args(TradeMode::ExactIn, hop1_in, 0, 0),
        ),
        v2.donate_ix(&tr, false, d2),
        client::swap(
            &v2.accounts(&tr, false),
            &args(TradeMode::ExactIn, hop2_in, 0, 0),
        ),
    ];
    let f1 = v1.fee_account(&NATIVE_MINT);
    let f2 = v2.fee_account(&mint);
    let (f1_0, f2_0) = (env.token_balance(&f1), env.token_balance(&f2));
    let meta = env.send(&ixs, &[&tr.kp]).expect("setup: two-hop route");
    let evs = swap_events(&meta);
    assert_eq!(evs.len(), 2);
    let paid = [env.token_balance(&f1) - f1_0, env.token_balance(&f2) - f2_0];
    let ins = [hop1_in, hop2_in];
    for i in 0..2 {
        println!(
            "[H028 hop{}] net_sent={} fee={} account_delta={}",
            i + 1,
            evs[i].net_sent,
            evs[i].pipeline_fee,
            paid[i]
        );
        assert_eq!(evs[i].net_sent, ins[i], "H028: hop {} net_sent", i + 1);
        assert_eq!(
            paid[i],
            fees::pipeline_fee(ins[i]),
            "H028: hop {} fee",
            i + 1
        );
    }
}
