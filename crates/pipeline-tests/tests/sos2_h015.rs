//! SOS audit #2, H015: reentrancy is closed on every path into the router.
//!
//! The router keeps no state across its CPIs because the runtime refuses a CPI into a
//! program that is already on the invocation stack unless that program is the *current*
//! (last) frame (`invoke_context.rs`, `push`: `contains && !is_last`). The router is frame 1
//! for every one of its instructions, so any program other than the router itself that
//! calls it back is refused, however many frames sit in between.
//!
//! The hostile callers here are Token-2022 transfer hooks (`test-hook-refill`), which can
//! CPI an arbitrary program named in their extra-account list: it moves a stash with an
//! instruction whose program is extra account 3. Pointing that program at the router makes
//! the hook call the router back. Asserted (the control for each is the same hook pointed
//! at SPL Token, which moves the stash, proving the hook CPI is reached at that point):
//!
//! A. input-mint hook -> router at each of the router's three input-mint transfers (the
//!    trader -> vault leg, the Pipeline fee leg and the integrator fee leg of `swap`);
//! B. output-mint hook -> router during the strategy's payout (stack: router, strategy,
//!    Token-2022, hook, router);
//! C. the fee hook -> router during `sweep_fees`;
//! D. an issuer whose hook program IS the router (crafted state: the mint's hook program
//!    patched to the router and a router-owned validation account), during a swap, and the
//!    control that the same mint calls the router fine when the router is not on the stack.
//!
//! Strategy -> router is `adversarial::reentering_the_router_fails`. Hook -> strategy ->
//! router cannot be built with the prebuilt fixtures (the refill hook's CPI data is fixed
//! `[3, amount]`, which is no instruction of any strategy fixture that forwards to the
//! router); the finding settles that one by code reading.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const ROUTER: Pubkey = client::PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
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

fn validation_address(hook: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], hook).0
}

fn stash_for(destination: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[destination.as_ref()], &REFILL_HOOK_ID).0
}

fn stash_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"stash"], &REFILL_HOOK_ID).0
}

fn set_validation_account(env: &mut Env, address: Pubkey, owner: Pubkey, tlv: Vec<u8>) {
    let lamports = env.svm.minimum_balance_for_rent_exemption(tlv.len());
    env.svm
        .set_account(
            address,
            Account {
                lamports,
                data: tlv,
                owner,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
}

/// The refill hook's `ExtraAccountMetaList` as fixed-address entries
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
    set_validation_account(
        env,
        validation_address(&REFILL_HOOK_ID, mint),
        REFILL_HOOK_ID,
        tlv,
    );
}

/// The stash-by-destination variant: stash PDA seeded by the transfer's destination
/// (account index 2), authority, `target`, and `program` as the hook's CPI target.
fn write_pda_stash_list(env: &mut Env, mint: &Pubkey, target: &Pubkey, program: &Pubkey) {
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
        (0, program.to_bytes(), false, false),
    ];
    for (discriminator, config, signer, writable) in entries {
        tlv.push(discriminator);
        tlv.extend_from_slice(&config);
        tlv.push(signer as u8);
        tlv.push(writable as u8);
    }
    set_validation_account(
        env,
        validation_address(&REFILL_HOOK_ID, mint),
        REFILL_HOOK_ID,
        tlv,
    );
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

fn router_invocations_at(logs: &[String], depth: usize) -> usize {
    let needle = format!("Program {ROUTER} invoke [{depth}]");
    logs.iter().filter(|line| line.contains(&needle)).count()
}

fn is_reentrancy(logs: &str) -> bool {
    logs.contains("reentrancy") || logs.contains("Reentrancy")
}

fn print_result(label: &str, result: &TxResult) {
    match result {
        Ok(meta) => println!("[{label}] SUCCEEDED\n{}", meta.logs.join("\n")),
        Err(e) => println!("[{label}] failed: {:?}\n{}", e.err, e.meta.logs.join("\n")),
    }
}

// ------------------------------------------------------------- A: input-mint hook legs

#[derive(Clone, Copy, Debug, PartialEq)]
enum Leg {
    Input,
    PipelineFee,
    IntegratorFee,
}

const LEGS: [Leg; 3] = [Leg::Input, Leg::PipelineFee, Leg::IntegratorFee];

impl Leg {
    /// How many hook runs a swap shows when the hook fails at this leg.
    fn hook_runs_when_failing(self) -> usize {
        match self {
            Leg::Input => 1,
            Leg::PipelineFee => 2,
            Leg::IntegratorFee => 3,
        }
    }
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
            Leg::IntegratorFee => self.integrator_a,
        }
    }
}

/// CP market between A (Token-2022, hostile refill hook) and B (legacy SPL).
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
            AccountMeta::new_readonly(validation_address(&REFILL_HOOK_ID, &mint_a), false),
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

/// Arms the hook so that during the transfer to `funded`'s destination it CPIs `program`
/// (with the whole stash of mint B, target the trader's legacy output account). The hook
/// does nothing at the other legs (empty stash).
fn arm(w: &mut World, funded: Leg, program: &Pubkey) {
    let (mint_a, mint_b, trader_b) = (w.mint_a, w.mint_b, w.trader_b);
    write_pda_stash_list(&mut w.env, &mint_a, &trader_b, program);
    let authority = stash_authority();
    for leg in LEGS {
        let amount = if leg == funded { STASH } else { 0 };
        let stash = stash_for(&w.destination(leg));
        put_spl_account(&mut w.env, &stash, &mint_b, &authority, amount);
    }
}

fn tail(w: &World, program: &Pubkey) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(REFILL_HOOK_ID, false),
        AccountMeta::new_readonly(validation_address(&REFILL_HOOK_ID, &w.mint_a), false),
        AccountMeta::new(stash_for(&w.cp.vault_a), false),
        AccountMeta::new(stash_for(&w.fee_a), false),
        AccountMeta::new(stash_for(&w.integrator_a), false),
        AccountMeta::new_readonly(stash_authority(), false),
        AccountMeta::new(w.trader_b, false),
        AccountMeta::new_readonly(*program, false),
    ]
}

fn swap_a(w: &mut World, tail: Vec<AccountMeta>) -> TxResult {
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
    let mut args = exact_in(AMOUNT_IN, 0);
    args.integrator_fee_bps = INTEGRATOR_BPS;
    let ix = client::swap(&accounts, &args);
    let trader = w.trader.insecure_clone();
    w.env.send(&[compute(1_400_000), ix], &[&trader])
}

/// Control: the same hook pointed at SPL Token is allowed to run its CPI at each of the three
/// legs (it moves the stash into the trader's legacy output account), the swap completes,
/// and the router's own `emit_cpi!` self-CPI (router -> router, depth 2) is allowed.
#[test]
fn control_hook_cpi_is_reached_at_every_input_leg_and_the_router_self_cpi_works() {
    for leg in LEGS {
        let mut w = world();
        arm(&mut w, leg, &TOKEN);
        let stash = stash_for(&w.destination(leg));
        let tail = tail(&w, &TOKEN);
        let result = swap_a(&mut w, tail);
        print_result(&format!("control {leg:?}"), &result);
        let meta =
            result.unwrap_or_else(|e| panic!("setup: control swap at {leg:?} failed: {:?}", e.err));
        assert_eq!(
            hook_invocations(&meta.logs),
            3,
            "{leg:?}: hook runs at all three legs"
        );
        assert_eq!(
            w.env.token_balance(&stash),
            0,
            "{leg:?}: the hook's CPI did not run"
        );
        assert_eq!(swap_events(&meta).len(), 1);
        assert_eq!(
            router_invocations_at(&meta.logs, 2),
            1,
            "{leg:?}: the router's emit_cpi self-CPI (A -> A) must be allowed"
        );
    }
}

/// A: the hook calls the router back at the input leg, the Pipeline fee leg and the
/// integrator fee leg. Each is refused by the runtime, the swap reverts as a whole and
/// nothing moves.
#[test]
fn input_hook_cannot_reenter_the_router_at_any_of_the_three_transfers() {
    for leg in LEGS {
        let mut w = world();
        arm(&mut w, leg, &ROUTER);
        let stash = stash_for(&w.destination(leg));
        let tail = tail(&w, &ROUTER);
        let result = swap_a(&mut w, tail);
        print_result(&format!("router as hook target, {leg:?}"), &result);
        let failed = result.expect_err(&format!(
            "H015: the input-mint hook re-entered the router at {leg:?} and the swap succeeded"
        ));
        let logs = failed.meta.logs.join("\n");
        assert!(
            is_reentrancy(&logs),
            "H015: expected ReentrancyNotAllowed at {leg:?}, got {:?}:\n{logs}",
            failed.err
        );
        assert_eq!(
            hook_invocations(&failed.meta.logs),
            leg.hook_runs_when_failing(),
            "{leg:?}: the failure happened at a different leg than intended"
        );
        // Nothing moved: the whole transaction reverted.
        assert_eq!(w.env.token_balance(&w.trader_a), TRADER_A, "{leg:?}");
        assert_eq!(w.env.token_balance(&w.trader_b), 0, "{leg:?}");
        assert_eq!(w.env.token_balance(&w.cp.vault_a), RESERVE, "{leg:?}");
        assert_eq!(w.env.token_balance(&w.fee_a), 0, "{leg:?}");
        assert_eq!(w.env.token_balance(&w.integrator_a), 0, "{leg:?}");
        assert_eq!(w.env.token_balance(&stash), STASH, "{leg:?}");
    }
}

// ------------------------------------------------------------ B: output-mint hook

struct OutWorld {
    env: Env,
    cp: CpMarket,
    mint_in: Pubkey,
    mint_out: Pubkey,
    trader: Keypair,
    trader_in: Pubkey,
    trader_out: Pubkey,
    stash: Pubkey,
    bystander: Pubkey,
    hook_accounts: Vec<AccountMeta>,
}

/// CP market with a legacy SPL input mint and a Token-2022 output mint whose refill hook
/// moves `STASH` (an SPL account of the input mint) to `bystander` through `program`.
/// The stash is funded only after the market's seed deposit, which also runs the hook.
fn out_world(program: &Pubkey) -> OutWorld {
    let mut env = Env::new();
    env.svm
        .add_program_from_file(REFILL_HOOK_ID, sbf_dir().join("test_hook_refill.so"))
        .unwrap();
    let mint_in = env.create_mint(&TOKEN, 6, None, None);
    let mint_out = env.create_mint(&TOKEN_2022, 6, None, Some(REFILL_HOOK_ID));
    let authority = stash_authority();
    let stash = env.create_ata(&authority, &mint_in, &TOKEN);
    let bystander = env.create_ata(&Pubkey::new_unique(), &mint_in, &TOKEN);

    let mut init_data = b"refilini".to_vec();
    for (key, writable) in [(stash, 1u8), (authority, 0), (bystander, 1), (*program, 0)] {
        init_data.extend_from_slice(key.as_ref());
        init_data.push(writable);
    }
    let validation = validation_address(&REFILL_HOOK_ID, &mint_out);
    let init = Instruction::new_with_bytes(
        REFILL_HOOK_ID,
        &init_data,
        vec![
            AccountMeta::new(env.admin.pubkey(), true),
            AccountMeta::new(validation, false),
            AccountMeta::new_readonly(mint_out, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    );
    env.send(&[init], &[]).expect("setup: hook init");
    let hook_accounts = vec![
        AccountMeta::new_readonly(REFILL_HOOK_ID, false),
        AccountMeta::new_readonly(validation, false),
        AccountMeta::new(stash, false),
        AccountMeta::new_readonly(authority, false),
        AccountMeta::new(bystander, false),
        AccountMeta::new_readonly(*program, false),
    ];
    let cp = setup_cp_market_with_mints(
        &mut env,
        mint_in,
        TOKEN,
        mint_out,
        TOKEN_2022,
        RESERVE,
        RESERVE,
        30,
        hook_accounts.clone(),
    );
    env.mint_to(&mint_in, &TOKEN, &stash, STASH);
    let (trader, trader_in) = env.trader_with(&mint_in, &TOKEN, TRADER_A);
    let trader_out = env.create_ata(&trader.pubkey(), &mint_out, &TOKEN_2022);
    OutWorld {
        env,
        cp,
        mint_in,
        mint_out,
        trader,
        trader_in,
        trader_out,
        stash,
        bystander,
        hook_accounts,
    }
}

fn swap_out(w: &mut OutWorld) -> TxResult {
    let mut accounts = cp_swap_accounts(
        &w.cp,
        &w.trader.pubkey(),
        &w.mint_in,
        &w.trader_in,
        &w.mint_out,
        &w.trader_out,
    );
    accounts.strategy_accounts.extend(w.hook_accounts.clone());
    let ix = client::swap(&accounts, &exact_in(AMOUNT_IN, 1));
    let trader = w.trader.insecure_clone();
    w.env.send(&[compute(1_400_000), ix], &[&trader])
}

/// Control: with SPL Token as the hook's CPI target the output hook's CPI is reached (the
/// stash moves to the bystander) during the strategy's payout, and the swap completes.
#[test]
fn control_output_hook_cpi_is_reached_during_the_strategys_payout() {
    let mut w = out_world(&TOKEN);
    let result = swap_out(&mut w);
    print_result("output control", &result);
    let meta = result.unwrap_or_else(|e| panic!("setup: control swap failed: {:?}", e.err));
    assert_eq!(hook_invocations(&meta.logs), 1, "the output hook runs once");
    assert_eq!(
        w.env.token_balance(&w.bystander),
        STASH,
        "the hook's CPI did not run"
    );
    assert_eq!(w.env.token_balance(&w.stash), 0);
}

/// B: router(1) -> strategy(2) -> Token-2022(3) -> hook(4) -> router. Refused.
#[test]
fn output_hook_cannot_reenter_the_router_during_the_strategys_payout() {
    let mut w = out_world(&ROUTER);
    let result = swap_out(&mut w);
    print_result("output hook -> router", &result);
    let failed = result.expect_err("H015: the output-mint hook re-entered the router");
    let logs = failed.meta.logs.join("\n");
    assert!(
        is_reentrancy(&logs),
        "H015: expected ReentrancyNotAllowed, got {:?}:\n{logs}",
        failed.err
    );
    assert_eq!(
        hook_invocations(&failed.meta.logs),
        1,
        "the failure was at the output hook"
    );
    assert_eq!(w.env.token_balance(&w.trader_in), TRADER_A);
    assert_eq!(w.env.token_balance(&w.trader_out), 0);
    assert_eq!(w.env.token_balance(&w.bystander), 0);
    assert_eq!(w.env.token_balance(&w.stash), STASH);
}

// ----------------------------------------------------------------- C: sweep_fees hook

const FEES_A: u64 = 1_000;

struct SweepWorld {
    env: Env,
    market: Pubkey,
    mint_a: Pubkey,
    fee_a: Pubkey,
    treasury_a: Pubkey,
    sink: Pubkey,
    stash: Pubkey,
}

/// An evil-strategy market (mint A Token-2022 with the refill hook, mint B legacy) whose
/// fee account in A holds `FEES_A`, with a stash of B and a sink for it. The hook's CPI
/// target is `program`.
fn sweep_world(program: &Pubkey) -> SweepWorld {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    env.svm
        .add_program_from_file(REFILL_HOOK_ID, sbf_dir().join("test_hook_refill.so"))
        .unwrap();
    let mint_a = env.create_mint(&TOKEN_2022, 6, None, Some(REFILL_HOOK_ID));
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    write_meta_list(&mut env, &mint_a, &[]);

    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN_2022);
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
    env.send(&[ix], &[&creator]).expect("setup: create market");
    let market = pda::market(&EVIL_ID, &state);
    let payer = env.admin.pubkey();
    env.send(
        &[client::init_fee_account(
            &payer,
            &market,
            &mint_a,
            &TOKEN_2022,
        )],
        &[],
    )
    .expect("setup: init fee account");
    let fee_a = pda::fee_account(&market, &mint_a, &TOKEN_2022);
    env.mint_to(&mint_a, &TOKEN_2022, &fee_a, FEES_A);
    let treasury_owner = env.treasury_owner;
    let treasury_a = env.create_ata(&treasury_owner, &mint_a, &TOKEN_2022);
    let sink = env.create_ata(&Pubkey::new_unique(), &mint_b, &TOKEN);
    let authority = stash_authority();
    let stash = env.create_ata(&authority, &mint_b, &TOKEN);
    env.mint_to(&mint_b, &TOKEN, &stash, STASH);
    write_meta_list(
        &mut env,
        &mint_a,
        &[
            (stash, false, true),
            (authority, false, false),
            (sink, false, true),
            (*program, false, false),
        ],
    );
    SweepWorld {
        env,
        market,
        mint_a,
        fee_a,
        treasury_a,
        sink,
        stash,
    }
}

fn sweep(w: &mut SweepWorld, program: &Pubkey) -> TxResult {
    let tail = vec![
        AccountMeta::new_readonly(REFILL_HOOK_ID, false),
        AccountMeta::new_readonly(validation_address(&REFILL_HOOK_ID, &w.mint_a), false),
        AccountMeta::new(w.stash, false),
        AccountMeta::new_readonly(stash_authority(), false),
        AccountMeta::new(w.sink, false),
        AccountMeta::new_readonly(*program, false),
    ];
    let ix = client::sweep_fees(
        &w.market,
        &w.mint_a,
        &TOKEN_2022,
        &w.env.treasury_owner,
        tail,
    );
    w.env.send(&[compute(400_000), ix], &[])
}

#[test]
fn control_hook_cpi_is_reached_during_sweep_fees() {
    let mut w = sweep_world(&TOKEN);
    let result = sweep(&mut w, &TOKEN);
    print_result("sweep control", &result);
    result.unwrap_or_else(|e| panic!("setup: control sweep failed: {:?}", e.err));
    assert_eq!(
        w.env.token_balance(&w.sink),
        STASH,
        "the hook's CPI did not run"
    );
    assert_eq!(w.env.token_balance(&w.treasury_a), FEES_A);
}

/// C: router(1) -> Token-2022(2) -> hook(3) -> router. Refused.
#[test]
fn fee_hook_cannot_reenter_the_router_during_sweep_fees() {
    let mut w = sweep_world(&ROUTER);
    let result = sweep(&mut w, &ROUTER);
    print_result("sweep hook -> router", &result);
    let failed = result.expect_err("H015: the hook re-entered the router during sweep_fees");
    let logs = failed.meta.logs.join("\n");
    assert!(
        is_reentrancy(&logs),
        "H015: expected ReentrancyNotAllowed, got {:?}:\n{logs}",
        failed.err
    );
    assert_eq!(hook_invocations(&failed.meta.logs), 1);
    assert_eq!(w.env.token_balance(&w.fee_a), FEES_A);
    assert_eq!(w.env.token_balance(&w.treasury_a), 0);
    assert_eq!(w.env.token_balance(&w.sink), 0);
    assert_eq!(w.env.token_balance(&w.stash), STASH);
}

// -------------------------------------------------------------- D: hook program = router

/// The world of A with mint A's hook program patched from the refill hook to the router, and
/// a router-owned (crafted: nobody can create one) empty validation account for it.
/// The market was set up with the refill hook (the seed deposit would otherwise have called
/// the router as a hook directly).
fn router_hook_world() -> (World, Pubkey) {
    let mut w = world();
    let mint_a = w.mint_a;
    let mut mint = w.env.svm.get_account(&mint_a).expect("mint A");
    let at = mint
        .data
        .windows(32)
        .position(|window| window == REFILL_HOOK_ID.as_ref())
        .expect("the hook program id is stored in the mint's extension");
    mint.data[at..at + 32].copy_from_slice(ROUTER.as_ref());
    w.env.svm.set_account(mint_a, mint).unwrap();

    let mut tlv = EXECUTE.to_vec();
    tlv.extend_from_slice(&4u32.to_le_bytes());
    tlv.extend_from_slice(&0u32.to_le_bytes());
    let validation = validation_address(&ROUTER, &mint_a);
    set_validation_account(&mut w.env, validation, ROUTER, tlv);
    (w, validation)
}

/// Control: with the router as the mint's hook program and the router NOT on the stack (a
/// direct Token-2022 transfer), Token-2022 does call the router (depth 2), and it fails for
/// another reason (the router has no `Execute` instruction), not for reentrancy. So the
/// refusal in the next test is the stack rule.
#[test]
fn control_router_as_hook_program_is_reachable_when_it_is_not_on_the_stack() {
    let (mut w, validation) = router_hook_world();
    let destination = w
        .env
        .create_ata(&Pubkey::new_unique(), &w.mint_a, &TOKEN_2022);
    let mut ix = spl_token_2022_interface::instruction::transfer_checked(
        &TOKEN_2022,
        &w.trader_a,
        &w.mint_a,
        &destination,
        &w.trader.pubkey(),
        &[],
        1_000,
        6,
    )
    .unwrap();
    ix.accounts.push(AccountMeta::new_readonly(ROUTER, false));
    ix.accounts
        .push(AccountMeta::new_readonly(validation, false));
    let trader = w.trader.insecure_clone();
    let result = w.env.send(&[ix], &[&trader]);
    print_result("direct transfer, hook = router", &result);
    let failed = result.expect_err("setup: the router has no Execute instruction");
    let logs = failed.meta.logs.join("\n");
    assert!(
        router_invocations_at(&failed.meta.logs, 2) >= 1,
        "setup: Token-2022 never called the router as the hook:\n{logs}"
    );
    assert!(
        !is_reentrancy(&logs),
        "the control must fail for a reason other than reentrancy:\n{logs}"
    );
}

/// D: a swap whose input mint's hook program is the router itself. router(1) ->
/// Token-2022(2) -> router(3) is refused, so a mint like that is simply unsellable.
#[test]
fn a_mint_whose_hook_program_is_the_router_cannot_be_traded_through_it() {
    let (mut w, validation) = router_hook_world();
    let tail = vec![
        AccountMeta::new_readonly(ROUTER, false),
        AccountMeta::new_readonly(validation, false),
    ];
    let result = swap_a(&mut w, tail);
    print_result("swap, hook = router", &result);
    let failed = result.expect_err("H015: a swap whose hook program is the router succeeded");
    let logs = failed.meta.logs.join("\n");
    assert!(
        is_reentrancy(&logs),
        "H015: expected ReentrancyNotAllowed, got {:?}:\n{logs}",
        failed.err
    );
    assert_eq!(w.env.token_balance(&w.trader_a), TRADER_A);
    assert_eq!(w.env.token_balance(&w.cp.vault_a), RESERVE);
    assert_eq!(w.env.token_balance(&w.fee_a), 0);
}
