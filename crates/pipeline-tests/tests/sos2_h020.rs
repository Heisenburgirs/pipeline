//! SOS audit #2, H020: the issuer's transfer hook as an in-transaction actor while the
//! strategy is off the stack (it runs inside the router's input transfer and fee
//! transfers, with the router, and Token-2022, on the stack).
//!
//! The hostile hook is `test-hook-refill`: on every transfer of its mint it moves the whole
//! balance of extra account 0 ("stash") into extra account 2 ("target") with a CPI to
//! extra account 3 ("program"), whose instruction data is `[3, amount]` (SPL Token
//! `Transfer`) and whose authority is extra account 1 (the hook's own PDA). The extra
//! account list is written with `set_account`, so the "program" can be anything.
//!
//! Asserted here:
//! (b) legacy-SPL `trader_output` top-up: with the target set to the trader's output
//!     account the swap succeeds, `received` includes the real refill, `net_sent` and both
//!     fees are exactly those of the un-refilled swap, and the trader's input spend is
//!     unchanged. A refill at a fee leg (after the measurement) is not counted in
//!     `received`. A refill can satisfy `min_amount_out`, but only with real tokens the
//!     trader holds afterwards;
//! (a)/(d) the hook cannot move a Token-2022 balance (trader input, input vault) or call
//!     the router: Token-2022 and the router are already on the stack, so the runtime
//!     refuses a re-entrant CPI, and SPL Token refuses Token-2022 accounts. Nothing the
//!     hook can do lowers `net_sent` below the trader's real spend;
//! (c) the hook can reach a strategy's direct instructions (CPI depth 4 under the router)
//!     but not through a Token-2022 transfer. Reach is shown with the CP strategy; what it
//!     could do there is a code-reading matter (see the finding).

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const REFILL_HOOK_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ4");
const EXECUTE: [u8; 8] = [105, 37, 101, 197, 75, 251, 102, 26];

const RESERVE: u64 = 1_000_000_000;
const TRADER_A: u64 = 10_000_000;
const AMOUNT_IN: u64 = 1_000_000;
const INTEGRATOR_BPS: u16 = 100;
const STASH: u64 = 4_242;

fn compute(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction::new_with_bytes(
        client::pubkey!("ComputeBudget111111111111111111111111111111"),
        &data,
        vec![],
    )
}

fn validation_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], &REFILL_HOOK_ID).0
}

fn stash_for(destination: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[destination.as_ref()], &REFILL_HOOK_ID).0
}

fn stash_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"stash"], &REFILL_HOOK_ID).0
}

/// Writes the refill hook's `ExtraAccountMetaList` as fixed-address entries
/// `(address, signer, writable)`.
fn write_meta_list(env: &mut Env, mint: &Pubkey, entries: &[(Pubkey, bool, bool)]) {
    let mut tlv = EXECUTE.to_vec();
    tlv.extend_from_slice(&(4 + 35 * entries.len() as u32).to_le_bytes());
    tlv.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (address, signer, writable) in entries {
        tlv.push(0);
        tlv.extend_from_slice(&address.to_bytes());
        tlv.push(*signer as u8);
        tlv.push(*writable as u8);
    }
    let lamports = env.svm.minimum_balance_for_rent_exemption(tlv.len());
    env.svm
        .set_account(
            validation_address(mint),
            Account {
                lamports,
                data: tlv,
                owner: REFILL_HOOK_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
}

/// The stash-by-destination variant of the list (stash PDA seeded by the transfer's
/// destination, account index 2), as in `sos2_h007`.
fn write_meta_list_pda_stash(env: &mut Env, mint: &Pubkey, target: &Pubkey) {
    let mut stash_seeds = [0u8; 32];
    stash_seeds[0] = 3;
    stash_seeds[1] = 2;
    let mut tlv = EXECUTE.to_vec();
    tlv.extend_from_slice(&(4 + 35 * 4u32).to_le_bytes());
    tlv.extend_from_slice(&4u32.to_le_bytes());
    let entries: [(u8, [u8; 32], bool, bool); 4] = [
        (1, stash_seeds, false, true),
        (0, stash_authority().to_bytes(), false, false),
        (0, target.to_bytes(), false, true),
        (0, TOKEN.to_bytes(), false, false),
    ];
    for (discriminator, config, signer, writable) in entries {
        tlv.push(discriminator);
        tlv.extend_from_slice(&config);
        tlv.push(signer as u8);
        tlv.push(writable as u8);
    }
    let lamports = env.svm.minimum_balance_for_rent_exemption(tlv.len());
    env.svm
        .set_account(
            validation_address(mint),
            Account {
                lamports,
                data: tlv,
                owner: REFILL_HOOK_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
}

fn put_spl_account(env: &mut Env, address: &Pubkey, mint: &Pubkey, owner: &Pubkey, amount: u64) {
    let mut data = vec![0u8; 165];
    data[..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1;
    let lamports = env.svm.minimum_balance_for_rent_exemption(165);
    env.svm
        .set_account(
            *address,
            Account {
                lamports,
                data,
                owner: TOKEN,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
}

fn hook_invocations(logs: &[String]) -> usize {
    let needle = format!("Program {REFILL_HOOK_ID} invoke");
    logs.iter().filter(|line| line.contains(&needle)).count()
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Leg {
    Input,
    PipelineFee,
}

struct World {
    env: Env,
    cp: CpMarket,
    mint_a: Pubkey,
    mint_b: Pubkey,
    trader: Keypair,
    trader_a: Pubkey,
    trader_b: Pubkey,
    integrator_a: Pubkey,
    fee_a: Pubkey,
}

impl World {
    fn destination(&self, leg: Leg) -> Pubkey {
        match leg {
            Leg::Input => self.cp.vault_a,
            Leg::PipelineFee => self.fee_a,
        }
    }
}

/// CP market between A (Token-2022 with the hostile hook) and B (legacy SPL).
fn world() -> World {
    let mut env = Env::new();
    env.svm
        .add_program_from_file(REFILL_HOOK_ID, sbf_dir().join("test_hook_refill.so"))
        .unwrap();
    let mint_a = env.create_mint(&TOKEN_2022, 6, None, Some(REFILL_HOOK_ID));
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    write_meta_list(&mut env, &mint_a, &[]);
    let cp = setup_cp_market_with_mints(
        &mut env,
        mint_a,
        TOKEN_2022,
        mint_b,
        TOKEN,
        RESERVE,
        RESERVE,
        30,
        vec![
            AccountMeta::new_readonly(REFILL_HOOK_ID, false),
            AccountMeta::new_readonly(validation_address(&mint_a), false),
        ],
    );
    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN_2022, TRADER_A);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let integrator_a = env.create_ata(&Pubkey::new_unique(), &mint_a, &TOKEN_2022);
    let fee_a = pda::fee_account(&cp.market, &mint_a, &TOKEN_2022);
    World {
        env,
        cp,
        mint_a,
        mint_b,
        trader,
        trader_a,
        trader_b,
        integrator_a,
        fee_a,
    }
}

/// Arms the PDA-stash hook so it moves `STASH` of mint B into `trader_output` during the
/// transfer to `funded`'s destination (empty stashes at the other legs).
fn arm_refill_to_output(w: &mut World, funded: Option<Leg>) {
    let (mint_a, mint_b, trader_b) = (w.mint_a, w.mint_b, w.trader_b);
    write_meta_list_pda_stash(&mut w.env, &mint_a, &trader_b);
    let authority = stash_authority();
    for leg in [Leg::Input, Leg::PipelineFee] {
        let amount = if Some(leg) == funded { STASH } else { 0 };
        let stash = stash_for(&w.destination(leg));
        put_spl_account(&mut w.env, &stash, &mint_b, &authority, amount);
    }
    // The integrator leg's stash must exist (the hook reads it) and be empty.
    let stash = stash_for(&w.integrator_a);
    put_spl_account(&mut w.env, &stash, &mint_b, &authority, 0);
}

fn pda_tail(w: &World) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(REFILL_HOOK_ID, false),
        AccountMeta::new_readonly(validation_address(&w.mint_a), false),
        AccountMeta::new(stash_for(&w.cp.vault_a), false),
        AccountMeta::new(stash_for(&w.fee_a), false),
        AccountMeta::new(stash_for(&w.integrator_a), false),
        AccountMeta::new_readonly(stash_authority(), false),
        AccountMeta::new(w.trader_b, false),
        AccountMeta::new_readonly(TOKEN, false),
    ]
}

fn swap_a(w: &mut World, tail: Vec<AccountMeta>, min_out: u64) -> TxResult {
    let mut accounts = cp_swap_accounts(
        &w.cp,
        &w.trader.pubkey(),
        &w.mint_a,
        &w.trader_a,
        &w.mint_b,
        &w.trader_b,
    );
    accounts.integrator_fee_account = Some(w.integrator_a);
    accounts.input_hook_accounts = tail;
    let mut args = exact_in(AMOUNT_IN, min_out);
    args.integrator_fee_bps = INTEGRATOR_BPS;
    let ix = client::swap(&accounts, &args);
    let trader = w.trader.insecure_clone();
    w.env.send(&[compute(1_400_000), ix], &[&trader])
}

fn print_result(label: &str, result: &TxResult) {
    match result {
        Ok(meta) => println!("[{label}] SUCCEEDED\n{}", meta.logs.join("\n")),
        Err(e) => println!("[{label}] failed: {:?}\n{}", e.err, e.meta.logs.join("\n")),
    }
}

struct Outcome {
    net_sent: u64,
    received: u64,
    pipeline_fee: u64,
    integrator_fee: u64,
    trader_a: u64,
    trader_b: u64,
    vault_a: u64,
    fee_a: u64,
    integrator_a: u64,
    hook_runs: usize,
}

fn outcome(w: &World, meta: &litesvm::types::TransactionMetadata) -> Outcome {
    let events = swap_events(meta);
    assert_eq!(events.len(), 1);
    let e = &events[0];
    Outcome {
        net_sent: e.net_sent,
        received: e.received,
        pipeline_fee: e.pipeline_fee,
        integrator_fee: e.integrator_fee,
        trader_a: w.env.token_balance(&w.trader_a),
        trader_b: w.env.token_balance(&w.trader_b),
        vault_a: w.env.token_balance(&w.cp.vault_a),
        fee_a: w.env.token_balance(&w.fee_a),
        integrator_a: w.env.token_balance(&w.integrator_a),
        hook_runs: hook_invocations(&meta.logs),
    }
}

/// The un-refilled swap: the hook runs at all three legs but every stash is empty.
fn baseline() -> Outcome {
    let mut w = world();
    arm_refill_to_output(&mut w, None);
    let tail = pda_tail(&w);
    let result = swap_a(&mut w, tail, 0);
    print_result("baseline", &result);
    let meta = result.expect("baseline swap");
    let o = outcome(&w, &meta);
    assert_eq!(o.hook_runs, 3, "the hook runs at all three legs");
    assert_eq!(o.net_sent, AMOUNT_IN);
    assert_eq!(o.trader_b, o.received);
    o
}

/// (b): a refill of the trader's legacy-SPL output account during the input transfer.
#[test]
fn refill_of_legacy_output_inflates_received_but_not_net_sent_or_fees() {
    let base = baseline();

    let mut w = world();
    arm_refill_to_output(&mut w, Some(Leg::Input));
    let tail = pda_tail(&w);
    let result = swap_a(&mut w, tail, 0);
    print_result("refill at input leg", &result);
    let meta = result.expect("setup: the refilled swap succeeds");
    let o = outcome(&w, &meta);
    assert_eq!(o.hook_runs, 3);

    // The refill is real: the trader holds exactly the strategy's output plus the stash.
    assert_eq!(o.trader_b, base.trader_b + STASH);
    assert_eq!(
        o.received,
        base.received + STASH,
        "received counts the refill"
    );
    // H020: the refill must not change what the trader is charged for or pays.
    assert_eq!(o.net_sent, base.net_sent, "H020: net_sent moved");
    assert_eq!(
        o.pipeline_fee, base.pipeline_fee,
        "H020: pipeline fee moved"
    );
    assert_eq!(
        o.integrator_fee, base.integrator_fee,
        "H020: integrator fee moved"
    );
    assert_eq!(o.trader_a, base.trader_a, "H020: trader input spend moved");
    assert_eq!(o.vault_a, base.vault_a);
    assert_eq!(o.fee_a, base.fee_a);
    assert_eq!(o.integrator_a, base.integrator_a);
    assert_eq!(
        TRADER_A - o.trader_a,
        AMOUNT_IN + o.pipeline_fee + o.integrator_fee,
        "trader pays exactly amount_in plus the stated fees"
    );
}

/// A refill at the Pipeline-fee leg comes after the measurement: it reaches the trader but
/// is not in `received`.
#[test]
fn refill_at_a_fee_leg_is_after_the_measurement() {
    let base = baseline();
    let mut w = world();
    arm_refill_to_output(&mut w, Some(Leg::PipelineFee));
    let tail = pda_tail(&w);
    let result = swap_a(&mut w, tail, 0);
    let meta = result.expect("setup: the swap succeeds");
    let o = outcome(&w, &meta);
    assert_eq!(
        o.trader_b,
        base.trader_b + STASH,
        "setup: the refill landed"
    );
    assert_eq!(
        o.received, base.received,
        "received is measured before the fee legs"
    );
    assert_eq!(o.net_sent, base.net_sent);
    assert_eq!(o.trader_a, base.trader_a);
}

/// `min_amount_out` is checked against `received`, so a refill can carry a swap over its
/// minimum. The trader still holds at least `min_amount_out` afterwards (the refill is
/// real), so the guarantee "receives at least min_amount_out" holds in token terms.
#[test]
fn refill_can_satisfy_min_amount_out_but_the_trader_holds_it() {
    let base = baseline();
    let min_out = base.received + STASH;

    let mut w = world();
    arm_refill_to_output(&mut w, None);
    let tail = pda_tail(&w);
    let result = swap_a(&mut w, tail, min_out);
    assert_fails_with(result, "InsufficientOutput");

    let mut w = world();
    arm_refill_to_output(&mut w, Some(Leg::Input));
    let tail = pda_tail(&w);
    let result = swap_a(&mut w, tail, min_out);
    let meta = result.expect("the refill carries the swap over min_amount_out");
    let o = outcome(&w, &meta);
    assert!(
        o.trader_b >= min_out,
        "H020: trader holds less than min_amount_out"
    );
}

/// Probes with the hook's CPI target ("program") and target account chosen freely.
/// Returns the swap result and the world.
fn probe(program: Pubkey, target: impl FnOnce(&mut World) -> Pubkey) -> (World, TxResult, Pubkey) {
    let mut w = world();
    let (mint_a, mint_b) = (w.mint_a, w.mint_b);
    let stash = Pubkey::new_unique();
    let authority = stash_authority();
    put_spl_account(&mut w.env, &stash, &mint_b, &authority, STASH);
    let tgt = target(&mut w);
    write_meta_list(
        &mut w.env,
        &mint_a,
        &[
            (stash, false, true),
            (authority, false, false),
            (tgt, false, true),
            (program, false, false),
        ],
    );
    let tail = vec![
        AccountMeta::new_readonly(REFILL_HOOK_ID, false),
        AccountMeta::new_readonly(validation_address(&mint_a), false),
        AccountMeta::new(stash, false),
        AccountMeta::new_readonly(authority, false),
        AccountMeta::new(tgt, false),
        AccountMeta::new_readonly(program, false),
    ];
    let result = swap_a(&mut w, tail, 0);
    print_result(&format!("probe program {program}"), &result);
    (w, result, stash)
}

fn t22_balances(w: &World) -> (u64, u64) {
    (
        w.env.token_balance(&w.trader_a),
        w.env.token_balance(&w.cp.vault_a),
    )
}

/// Control for the probes: the same hook with SPL Token as the program and the trader's
/// legacy output as the target succeeds, so a failure below comes from the chosen
/// program or target and not from the probe plumbing.
#[test]
fn probe_control_spl_token_refill_works() {
    let (w, result, stash) = probe(TOKEN, |w: &mut World| w.trader_b);
    let meta = result.expect("control probe");
    assert_eq!(hook_invocations(&meta.logs), 3);
    assert_eq!(
        w.env.token_balance(&w.trader_b),
        swap_events(&meta)[0].received
    );
    assert_eq!(w.env.token_balance(&stash), 0);
}

/// (a)/(d): the hook tries to move the swap's own Token-2022 balances (trader input, input
/// vault). Token-2022 hands the hook its `source`/`destination` as read-only, whatever the
/// meta list says, so the hook's CPI is refused for writability before it matters which
/// program it calls. The swap reverts and nothing moves.
/// Picks the account a hook probe aims at.
type TargetFn = fn(&mut World) -> Pubkey;

#[test]
fn hook_cannot_move_the_swaps_token_2022_balances() {
    let before = {
        let w = world();
        t22_balances(&w)
    };
    let targets: [(&str, TargetFn); 2] = [
        ("trader input", |w| w.trader_a),
        ("input vault", |w| w.cp.vault_a),
    ];
    for (label, target) in targets {
        for program in [TOKEN_2022, TOKEN] {
            let (w, result, stash) = probe(program, target);
            let failed = result.expect_err("H020: a hook CPI that moves T22 balances must fail");
            let logs = failed.meta.logs.join("\n");
            assert_eq!(
                hook_invocations(&failed.meta.logs),
                1,
                "{label}: reached the hook"
            );
            assert!(
                logs.contains("writable privilege escalated"),
                "{label}, {program}: expected the writability refusal, got:\n{logs}"
            );
            assert_eq!(
                t22_balances(&w),
                before,
                "H020: T22 balance moved ({label}, {program})"
            );
            assert_eq!(w.env.token_balance(&stash), STASH);
        }
    }
}

/// (a): a Token-2022 account the hook CAN get writable (a bystander's, not part of the
/// transfer) still can't be moved: calling Token-2022 from the hook is a re-entrant CPI
/// (router, Token-2022, hook, Token-2022), which the runtime refuses. Through SPL Token it
/// is the wrong owner.
#[test]
fn hook_cannot_reenter_token_2022_even_for_a_bystander_account() {
    for program in [TOKEN_2022, TOKEN] {
        let mut bystander = Pubkey::default();
        let (w, result, stash) = probe(program, |w: &mut World| {
            let mint_a = w.mint_a;
            bystander = w
                .env
                .create_ata(&Pubkey::new_unique(), &mint_a, &TOKEN_2022);
            w.env.mint_to(&mint_a, &TOKEN_2022, &bystander, 777);
            bystander
        });
        let failed = result.expect_err("H020: the hook moved a Token-2022 balance");
        let logs = failed.meta.logs.join("\n");
        assert_eq!(hook_invocations(&failed.meta.logs), 1);
        if program == TOKEN_2022 {
            assert!(
                logs.contains("reentrancy") || logs.contains("Reentrancy"),
                "expected a re-entrancy refusal, got:\n{logs}"
            );
        }
        assert_eq!(w.env.token_balance(&bystander), 777, "H020 ({program})");
        assert_eq!(w.env.token_balance(&stash), STASH);
    }
}

/// The hook cannot call the router back during its own transfer (router, Token-2022 and
/// the hook are stacked; the router is not the hook's caller).
#[test]
fn hook_cannot_reenter_the_router() {
    let (w, result, _) = probe(client::PROGRAM_ID, |w: &mut World| w.trader_b);
    let failed = result.expect_err("H020: the hook re-entered the router");
    let logs = failed.meta.logs.join("\n");
    assert!(
        logs.contains("reentrancy") || logs.contains("Reentrancy"),
        "expected a re-entrancy refusal, got:\n{logs}"
    );
    assert_eq!(t22_balances(&w).0, TRADER_A);
}

/// (c): the hook can reach a strategy's direct instructions at CPI depth 4
/// (router 1, Token-2022 2, hook 3, strategy 4). Here the CP strategy gets the hook's
/// `[3, amount]` data, which is no instruction of it, so it fails; the point is the log
/// "Program <CP> invoke [4]" showing the strategy is callable while the router's input
/// transfer is in flight.
#[test]
fn hook_can_reach_a_strategy_at_depth_four() {
    let (_w, result, _) = probe(CP_PROGRAM_ID, |w: &mut World| w.trader_b);
    let failed = result.expect_err("setup: the CP strategy rejects the hook's raw data");
    let logs = failed.meta.logs.join("\n");
    let needle = format!("Program {CP_PROGRAM_ID} invoke [4]");
    assert!(
        logs.contains(&needle),
        "the strategy was not reached at depth 4:\n{logs}"
    );
}
