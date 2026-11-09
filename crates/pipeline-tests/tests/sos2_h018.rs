//! SOS audit #2, H018: the unmeasured window after `settle`. `swap` checks the trader's
//! bound (`trader_spent <= amount_in`, `received >= min_amount_out`) in `settle`, then makes
//! the Pipeline-fee and integrator-fee transfers of the input mint. When the input mint is
//! Token-2022 with a transfer hook, the hook runs in those two transfers with the trader's
//! balances never re-read. What can the hook move there?
//!
//! The hostile hook is `test-hook-refill`: on every transfer of its mint it moves the whole
//! balance of extra account 0 ("stash") to extra account 2 ("sink") with a Transfer of
//! extra account 3 (the token program) whose authority is extra account 1 (the hook's own
//! PDA). Its `ExtraAccountMetaList` is written with `set_account` so that the stash is the
//! trader's output account (or input account) and the hook PDA is only a delegate of it
//! where a test says so.
//!
//! Asserted here:
//! - control: the hook runs at the input leg and both fee legs, and the trader's input
//!   debit is exactly `amount_in` plus the two fees, within `max_total_spend`;
//! - characterisation (accepted: the trader's own delegation): a legacy-SPL
//!   `trader_output` on which the trader earlier approved the hook's PDA as delegate is
//!   emptied during the fee transfers, after `settle` accepted `received`;
//! - without that approval the same hook cannot move `trader_output` (the swap reverts);
//! - a Token-2022 `trader_output` or `trader_input` is out of the hook's reach even with
//!   the delegate approved, because Token-2022 is on the call stack (re-entrancy refusal);
//! - the trader's own signature does not reach the hook (see also H007), so the wallet's
//!   lamports and its non-delegated accounts are untouched.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_strategy_interface::fees;
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
const FEE_BPS: u16 = 30;

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

fn stash_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"stash"], &REFILL_HOOK_ID).0
}

/// Writes the hook's `ExtraAccountMetaList` for `mint`: stash (writable), the hook PDA as
/// authority, sink (writable), token program.
fn set_validation(
    env: &mut Env,
    mint: &Pubkey,
    stash: &Pubkey,
    sink: &Pubkey,
    token_program: &Pubkey,
) {
    let entries: [(u8, [u8; 32], bool, bool); 4] = [
        (0, stash.to_bytes(), false, true),
        (0, stash_authority().to_bytes(), false, false),
        (0, sink.to_bytes(), false, true),
        (0, token_program.to_bytes(), false, false),
    ];
    write_meta_list(env, mint, &entries);
}

fn write_meta_list(env: &mut Env, mint: &Pubkey, entries: &[(u8, [u8; 32], bool, bool)]) {
    let mut tlv = EXECUTE.to_vec();
    tlv.extend_from_slice(&(4 + 35 * entries.len() as u32).to_le_bytes());
    tlv.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (discriminator, config, signer, writable) in entries {
        tlv.push(*discriminator);
        tlv.extend_from_slice(config);
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

/// The trader's earlier `Approve`: sets `delegate` with an unlimited allowance on a token
/// account (the base layout is the same in both token programs).
fn approve_delegate(env: &mut Env, account: &Pubkey, delegate: &Pubkey) {
    let mut stored = env.svm.get_account(account).expect("account exists");
    stored.data[72..76].copy_from_slice(&1u32.to_le_bytes());
    stored.data[76..108].copy_from_slice(delegate.as_ref());
    stored.data[121..129].copy_from_slice(&u64::MAX.to_le_bytes());
    env.svm.set_account(*account, stored).unwrap();
}

fn hook_invocations(logs: &[String]) -> usize {
    let needle = format!("Program {REFILL_HOOK_ID} invoke");
    logs.iter().filter(|line| line.contains(&needle)).count()
}

struct World {
    env: Env,
    cp: CpMarket,
    /// Token-2022 mint with the hostile hook (the swap's input).
    mint_a: Pubkey,
    /// The swap's output mint: legacy SPL or Token-2022 (no hook).
    mint_b: Pubkey,
    token_program_b: Pubkey,
    trader: Keypair,
    trader_a: Pubkey,
    trader_b: Pubkey,
    integrator_a: Pubkey,
    fee_a: Pubkey,
    /// The hook issuer's account in mint B.
    sink: Pubkey,
}

fn world(token_program_b: Pubkey) -> World {
    let mut env = Env::new();
    env.svm
        .add_program_from_file(REFILL_HOOK_ID, sbf_dir().join("test_hook_refill.so"))
        .unwrap();
    let mint_a = env.create_mint(&TOKEN_2022, 6, None, Some(REFILL_HOOK_ID));
    let mint_b = env.create_mint(&token_program_b, 6, None, None);
    write_meta_list(&mut env, &mint_a, &[]);
    let cp = setup_cp_market_with_mints(
        &mut env,
        mint_a,
        TOKEN_2022,
        mint_b,
        token_program_b,
        RESERVE,
        RESERVE,
        FEE_BPS,
        vec![
            AccountMeta::new_readonly(REFILL_HOOK_ID, false),
            AccountMeta::new_readonly(validation_address(&mint_a), false),
        ],
    );
    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN_2022, TRADER_A);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &token_program_b);
    let integrator_a = env.create_ata(&Pubkey::new_unique(), &mint_a, &TOKEN_2022);
    let fee_a = pda::fee_account(&cp.market, &mint_a, &TOKEN_2022);
    let sink = env.create_ata(&Pubkey::new_unique(), &mint_b, &token_program_b);
    World {
        env,
        cp,
        mint_a,
        mint_b,
        token_program_b,
        trader,
        trader_a,
        trader_b,
        integrator_a,
        fee_a,
        sink,
    }
}

/// Swaps `AMOUNT_IN` of A for B (exact-in, `min_out` as the bound) with an integrator fee,
/// so the input leg and both fee legs run the input mint's hook. `tail` is the hook tail.
fn swap_a(w: &mut World, min_out: u64, stash: &Pubkey, token_program: &Pubkey) -> TxResult {
    let mut accounts = cp_swap_accounts(
        &w.cp,
        &w.trader.pubkey(),
        &w.mint_a,
        &w.trader_a,
        &w.mint_b,
        &w.trader_b,
    );
    accounts.integrator_fee_account = Some(w.integrator_a);
    accounts.input_hook_accounts = vec![
        AccountMeta::new_readonly(REFILL_HOOK_ID, false),
        AccountMeta::new_readonly(validation_address(&w.mint_a), false),
        AccountMeta::new(*stash, false),
        AccountMeta::new_readonly(stash_authority(), false),
        AccountMeta::new(w.sink, false),
        AccountMeta::new_readonly(*token_program, false),
    ];
    let mut args = exact_in(AMOUNT_IN, min_out);
    args.integrator_fee_bps = INTEGRATOR_BPS;
    let ix = client::swap(&accounts, &args);
    let trader = w.trader.insecure_clone();
    w.env.send(&[compute(1_400_000), ix], &[&trader])
}

fn quote() -> u64 {
    cp_out(RESERVE, RESERVE, AMOUNT_IN, FEE_BPS)
}

fn print_result(label: &str, result: &TxResult) {
    match result {
        Ok(meta) => println!("[{label}] SUCCEEDED\n{}", meta.logs.join("\n")),
        Err(e) => println!("[{label}] failed: {:?}\n{}", e.err, e.meta.logs.join("\n")),
    }
}

/// Control: the hook (with a harmless stash, the trader's empty output account) runs at the
/// input leg and both fee legs, and the trader's input debit is exactly `amount_in` plus
/// the two fees, so the hook's presence in the window changes nothing in the normal case.
#[test]
fn control_hook_runs_at_all_three_legs_and_debit_is_within_bound() {
    let mut w = world(TOKEN);
    let (mint_a, sink) = (w.mint_a, w.sink);
    let empty_stash = w
        .env
        .create_ata(&Pubkey::new_unique(), &w.mint_b, &w.token_program_b);
    set_validation(&mut w.env, &mint_a, &empty_stash, &sink, &TOKEN);
    let result = swap_a(&mut w, quote(), &empty_stash, &TOKEN);
    print_result("control (empty stash)", &result);
    let meta = result.expect("control swap");
    assert_eq!(hook_invocations(&meta.logs), 3, "hook runs at all 3 legs");
    let debit = TRADER_A - w.env.token_balance(&w.trader_a);
    let fee = w.env.token_balance(&w.fee_a);
    let integrator = w.env.token_balance(&w.integrator_a);
    assert_eq!(debit, AMOUNT_IN + fee + integrator);
    assert!(debit <= fees::max_total_spend(AMOUNT_IN, INTEGRATOR_BPS).unwrap());
    assert_eq!(w.env.token_balance(&w.trader_b), quote());
}

/// Characterisation of the residue: the trader earlier approved the hook's PDA as delegate
/// of a legacy-SPL `trader_output`. During the first fee transfer the hook uses that
/// delegation and empties `trader_output`, after `settle` accepted `received >=
/// min_amount_out`. The trader's input debit is still within `max_total_spend`. The swap
/// succeeds, and the event reports `received` as measured at `settle`.
///
/// This is the delegate exercising a power the trader granted (it could do the same in
/// any other instruction, before or after the swap), so it is not asserted as a router
/// bug: the test passes and documents the behaviour.
#[test]
fn characterise_delegated_spl_output_is_emptied_in_the_fee_window() {
    let mut w = world(TOKEN);
    let (mint_a, trader_b, sink) = (w.mint_a, w.trader_b, w.sink);
    set_validation(&mut w.env, &mint_a, &trader_b, &sink, &TOKEN);
    approve_delegate(&mut w.env, &trader_b, &stash_authority());
    assert_eq!(w.env.token_balance(&trader_b), 0);

    let min_out = quote();
    let result = swap_a(&mut w, min_out, &trader_b, &TOKEN);
    print_result("delegated spl output", &result);
    let meta = result.expect("setup: the swap itself succeeds");
    // The input leg ran with an empty trader_output, then the fee legs ran with it full.
    assert_eq!(hook_invocations(&meta.logs), 3);
    let event = swap_events(&meta).pop().expect("swap event");
    assert_eq!(event.received, min_out, "settle measured the bound as met");
    println!(
        "H018 characterisation: event.received = {}, trader_output after = {}, sink = {}",
        event.received,
        w.env.token_balance(&w.trader_b),
        w.env.token_balance(&w.sink)
    );
    // The delegate's use of the trader's approval:
    assert_eq!(w.env.token_balance(&w.trader_b), 0);
    assert_eq!(w.env.token_balance(&w.sink), min_out);
    // The input side stays within the signed bound.
    let debit = TRADER_A - w.env.token_balance(&w.trader_a);
    assert!(debit <= fees::max_total_spend(AMOUNT_IN, INTEGRATOR_BPS).unwrap());
    assert_eq!(
        debit,
        AMOUNT_IN + w.env.token_balance(&w.fee_a) + w.env.token_balance(&w.integrator_a)
    );
}

/// Safety: the same hook and the same stash, but no delegation. The hook runs and tries to
/// move `trader_output` in the fee window, the token program refuses (the hook PDA has no
/// authority over the account), and the hook's failure reverts the whole swap. Nothing of
/// the trader's moves.
#[test]
fn without_a_delegation_the_hook_cannot_move_trader_output() {
    let mut w = world(TOKEN);
    let (mint_a, trader_b, sink) = (w.mint_a, w.trader_b, w.sink);
    set_validation(&mut w.env, &mint_a, &trader_b, &sink, &TOKEN);
    let result = swap_a(&mut w, quote(), &trader_b, &TOKEN);
    print_result("no delegation", &result);
    let failed = result.expect_err("the hook's refused transfer must fail the swap");
    assert_eq!(
        hook_invocations(&failed.meta.logs),
        2,
        "the failure is at the first fee leg, after the (empty-stash) input leg"
    );
    assert_eq!(
        w.env.token_balance(&w.sink),
        0,
        "H018: loot reached the sink"
    );
    assert_eq!(w.env.token_balance(&w.trader_a), TRADER_A);
    assert_eq!(w.env.token_balance(&w.trader_b), 0);
}

/// Safety: a Token-2022 `trader_output` with the hook's PDA approved as delegate is out of
/// the hook's reach in the fee window. The hook's CPI back into Token-2022 (which is on the
/// stack, running the transfer that called the hook) is refused, so the swap reverts
/// instead of letting the hook empty the trader's output.
#[test]
fn token2022_output_is_out_of_the_hooks_reach_even_when_delegated() {
    let mut w = world(TOKEN_2022);
    let (mint_a, trader_b, sink) = (w.mint_a, w.trader_b, w.sink);
    set_validation(&mut w.env, &mint_a, &trader_b, &sink, &TOKEN_2022);
    approve_delegate(&mut w.env, &trader_b, &stash_authority());
    let result = swap_a(&mut w, quote(), &trader_b, &TOKEN_2022);
    print_result("t22 output delegated", &result);
    let failed = result.expect_err("a hook CPI into Token-2022 must be refused");
    assert_eq!(
        hook_invocations(&failed.meta.logs),
        2,
        "refused at the first fee leg (the input leg's stash was still empty)"
    );
    let logs = failed.meta.logs.join("\n").to_lowercase();
    assert!(
        logs.contains("reentr") || logs.contains("re-entr"),
        "expected a re-entrancy refusal, got:\n{logs}"
    );
    assert_eq!(w.env.token_balance(&w.sink), 0, "H018: T22 output moved");
    assert_eq!(w.env.token_balance(&w.trader_a), TRADER_A);
}

/// Safety: the trader's own Token-2022 `trader_input` (the account the fee transfers
/// debit) with the hook's PDA approved as delegate: the hook can't spend it during any
/// leg either (it is the transfer's read-only source in the hook's frame, and the CPI
/// would re-enter Token-2022 besides), so the trader's input debit can't be pushed past the signed bound by the
/// hook.
#[test]
fn token2022_input_is_out_of_the_hooks_reach_even_when_delegated() {
    let mut w = world(TOKEN);
    let (mint_a, trader_a, sink) = (w.mint_a, w.trader_a, w.sink);
    // A T22 sink for a T22 stash: the hook's sink must be a mint-A account.
    let sink_a = w
        .env
        .create_ata(&Pubkey::new_unique(), &mint_a, &TOKEN_2022);
    w.sink = sink_a;
    let _ = sink;
    set_validation(&mut w.env, &mint_a, &trader_a, &sink_a, &TOKEN_2022);
    approve_delegate(&mut w.env, &trader_a, &stash_authority());
    let result = swap_a(&mut w, quote(), &trader_a, &TOKEN_2022);
    print_result("t22 input delegated", &result);
    let failed = result.expect_err("a hook CPI into Token-2022 must be refused");
    assert_eq!(hook_invocations(&failed.meta.logs), 1, "first leg already");
    // Token-2022 hands the hook the transfer's source as a read-only account, so the
    // hook's writable use of `trader_input` is a privilege escalation before the
    // re-entrancy rule even applies.
    let logs = failed.meta.logs.join("\n").to_lowercase();
    assert!(
        logs.contains("writable privilege escalated") || logs.contains("reentr"),
        "expected a privilege or re-entrancy refusal, got:\n{logs}"
    );
    assert_eq!(w.env.token_balance(&sink_a), 0, "H018: T22 input moved");
    assert_eq!(w.env.token_balance(&w.trader_a), TRADER_A);
}

/// The trader's own signature is not available to the hook in the fee window: the hook
/// names the trader's legacy-SPL output account as the stash and the trader's wallet as the
/// transfer authority (instead of a delegate PDA), as a signer. Token-2022 and the router
/// both clear the signer flag, so the CPI is refused and nothing moves (H007 covers
/// co-signers; this covers the trader, the one signer the router does pass to
/// Token-2022).
#[test]
fn the_traders_own_signature_does_not_reach_the_hook() {
    let mut w = world(TOKEN);
    let (mint_a, trader_b, sink) = (w.mint_a, w.trader_b, w.sink);
    let trader = w.trader.pubkey();
    // The authority entry is the trader's wallet and is marked as a signer.
    let entries: [(u8, [u8; 32], bool, bool); 4] = [
        (0, trader_b.to_bytes(), false, true),
        (0, trader.to_bytes(), true, false),
        (0, sink.to_bytes(), false, true),
        (0, TOKEN.to_bytes(), false, false),
    ];
    write_meta_list(&mut w.env, &mint_a, &entries);

    let mut accounts = cp_swap_accounts(
        &w.cp,
        &trader,
        &w.mint_a,
        &w.trader_a,
        &w.mint_b,
        &w.trader_b,
    );
    accounts.integrator_fee_account = Some(w.integrator_a);
    accounts.input_hook_accounts = vec![
        AccountMeta::new_readonly(REFILL_HOOK_ID, false),
        AccountMeta::new_readonly(validation_address(&w.mint_a), false),
        AccountMeta::new(trader_b, false),
        AccountMeta::new_readonly(trader, true),
        AccountMeta::new(w.sink, false),
        AccountMeta::new_readonly(TOKEN, false),
    ];
    let mut args = exact_in(AMOUNT_IN, quote());
    args.integrator_fee_bps = INTEGRATOR_BPS;
    let ix = client::swap(&accounts, &args);
    let signer = w.trader.insecure_clone();
    let result = w.env.send(&[compute(1_400_000), ix], &[&signer]);
    print_result("trader signature", &result);
    let failed = result.expect_err("the hook must not get the trader's signature");
    // The hook's Transfer names the trader as the authority and signs only with its own
    // PDA seeds; the trader's signature is not available in the hook's frame.
    assert_eq!(hook_invocations(&failed.meta.logs), 2);
    assert_eq!(
        w.env.token_balance(&w.sink),
        0,
        "H018: trader's tokens moved"
    );
    assert_eq!(w.env.token_balance(&w.trader_a), TRADER_A);
    assert_eq!(w.env.token_balance(&w.trader_b), 0);
}
