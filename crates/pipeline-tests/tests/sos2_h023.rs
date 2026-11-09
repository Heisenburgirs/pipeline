//! SOS audit #2, H023: `fetch_market_info` accepts return data only from the strategy.
//!
//! The router CPIs `pipeline_market_info` and then requires `get_return_data()` to be
//! (strategy_program, valid Borsh `MarketInfo`). Return data is keyed by the program
//! whose syscall set it, and the runtime resets it to (callee, empty) on every
//! instruction entry (`InvokeContext::process_executable_chain`), so:
//! - a strategy that sets nothing leaves (strategy, empty) -> `get_return_data` is None;
//! - a strategy that CPIs another program last leaves (that program, ...) -> mismatch;
//! - data set by an earlier instruction in the same transaction cannot survive.
//!
//! No shipped strategy CPIs out at the end of `market_info`, so the program-id mismatch
//! is settled by code and runtime source. What a test can do is use a builtin program
//! that succeeds on any instruction data without setting return data (ComputeBudget) as
//! a crafted "strategy": its `Strategy` PDA and a state account it owns are planted with
//! `set_account`. Properties asserted:
//! 1. control: the evil strategy's market registers (its own return data accepted);
//! 2. a strategy whose `market_info` succeeds but sets no return data -> `BadMarketInfo`;
//! 3. the same, right after an instruction in the same transaction that left valid
//!    `MarketInfo` return data from the evil strategy -> still `BadMarketInfo`;
//! 4. nothing is recorded in cases 2 and 3.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use solana_account::Account;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
const COMPUTE_BUDGET: Pubkey = client::pubkey!("ComputeBudget111111111111111111111111111111");

fn state_bytes(keys: [&Pubkey; 4]) -> Vec<u8> {
    let mut data = vec![0u8; 130];
    for (i, key) in keys.iter().enumerate() {
        data[i * 32..(i + 1) * 32].copy_from_slice(key.as_ref());
    }
    data
}

struct World {
    env: Env,
    /// Evil market state (owned by the evil strategy), valid for `register_existing_market`.
    evil_state: Pubkey,
    /// A state account owned by ComputeBudget; registering it must fail.
    cb_state: Pubkey,
}

fn world() -> World {
    let mut env = Env::new();
    let owner = env.register(&EVIL_ID, "test_strategy_evil");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let evil_state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&evil_state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&evil_state, &mint_b, &TOKEN);
    env.svm
        .set_account(
            evil_state,
            Account {
                lamports: SOL,
                data: state_bytes([&mint_a, &mint_b, &vault_a, &vault_b]),
                owner: EVIL_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    let _ = owner;

    // Plant a `Strategy` record for ComputeBudget by copying the evil one and patching
    // the program id and bump (the account layout is Anchor disc, program_id, string,
    // version, registered_at, market_count, bump).
    let evil_strategy = env.svm.get_account(&pda::strategy(&EVIL_ID)).unwrap();
    let cb_strategy_pda = pda::strategy(&COMPUTE_BUDGET);
    let (_, bump) =
        Pubkey::find_program_address(&[b"strategy", COMPUTE_BUDGET.as_ref()], &client::PROGRAM_ID);
    assert_eq!(
        Pubkey::find_program_address(&[b"strategy", COMPUTE_BUDGET.as_ref()], &client::PROGRAM_ID)
            .0,
        cb_strategy_pda
    );
    let mut data = evil_strategy.data.clone();
    data[8..40].copy_from_slice(COMPUTE_BUDGET.as_ref());
    let strategy = client::decode_strategy(&evil_strategy.data).unwrap();
    let bump_at = 8 + 32 + 4 + strategy.metadata_uri.len() + 1 + 8 + 8;
    data[bump_at] = bump;
    assert_eq!(
        client::decode_strategy(&data).unwrap().program_id,
        COMPUTE_BUDGET
    );
    env.svm
        .set_account(
            cb_strategy_pda,
            Account {
                lamports: SOL,
                data,
                owner: client::PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    // A state account owned by ComputeBudget, with perfectly valid-looking contents.
    let cb_state = Pubkey::new_unique();
    env.svm
        .set_account(
            cb_state,
            Account {
                lamports: SOL,
                data: state_bytes([&mint_a, &mint_b, &vault_a, &vault_b]),
                owner: COMPUTE_BUDGET,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    World {
        env,
        evil_state,
        cb_state,
    }
}

#[test]
fn builtin_strategy_is_executable_and_runs_market_info_without_return_data() {
    // Setup sanity: the crafted "strategy" is a real executable program, so a failure
    // in the tests below comes from the router's return-data checks, not Anchor's
    // `executable` constraint or a failed CPI.
    let w = world();
    let account = w.env.svm.get_account(&COMPUTE_BUDGET).expect("builtin");
    assert!(account.executable, "ComputeBudget must be executable");
}

#[test]
fn control_strategy_that_sets_its_own_return_data_registers() {
    let mut w = world();
    let payer = w.env.admin.pubkey();
    let ix = client::register_existing_market(&payer, &EVIL_ID, &w.evil_state, vec![]);
    w.env.send(&[ix], &[]).expect("evil market registers");
    let market = w.env.market(&pda::market(&EVIL_ID, &w.evil_state));
    assert_eq!(market.mints().len(), 2);
}

#[test]
fn strategy_that_sets_no_return_data_is_refused() {
    let mut w = world();
    let payer = w.env.admin.pubkey();
    let ix = client::register_existing_market(&payer, &COMPUTE_BUDGET, &w.cb_state, vec![]);
    let failed = w.env.send(&[ix], &[]).expect_err("must be refused");
    let logs = failed.meta.logs.join("\n");
    println!("{logs}");
    // Must reach the router's own check: the CPI itself has to have succeeded.
    assert!(
        logs.contains(&format!("Program {COMPUTE_BUDGET} success")),
        "setup: the strategy CPI must succeed:\n{logs}"
    );
    assert!(
        logs.contains("BadMarketInfo"),
        "H023: wrong failure:\n{logs}"
    );
    assert!(
        w.env
            .svm
            .get_account(&pda::market(&COMPUTE_BUDGET, &w.cb_state))
            .is_none_or(|a| a.data.is_empty()),
        "H023: a market was recorded from empty return data"
    );
}

#[test]
fn stale_return_data_from_an_earlier_instruction_does_not_survive() {
    let mut w = world();
    let payer = w.env.admin.pubkey();
    // Instruction 1 leaves (evil, valid MarketInfo) as the transaction's return data
    // when it ends (the router sets none after the strategy call).
    let first = client::register_existing_market(&payer, &EVIL_ID, &w.evil_state, vec![]);
    let second = client::register_existing_market(&payer, &COMPUTE_BUDGET, &w.cb_state, vec![]);
    let result = w.env.send(&[first, second], &[]);
    assert_fails_with(result, "BadMarketInfo");
    // Atomic: instruction 1's market was rolled back too, and neither is recorded.
    for (program, state) in [(EVIL_ID, w.evil_state), (COMPUTE_BUDGET, w.cb_state)] {
        assert!(
            w.env
                .svm
                .get_account(&pda::market(&program, &state))
                .is_none_or(|a| a.data.is_empty()),
            "H023: stale return data produced a market for {program}"
        );
    }
}
