//! Compute used by the router itself on top of the strategy, per swap.
//!
//! Run with `--nocapture` to print the table. The test fails if router overhead
//! exceeds the design target, so regressions show up in CI.

mod common;

use common::*;
use pipeline_client::{self as client, Pubkey};
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const ROUTER_OVERHEAD_TARGET: u64 = 60_000;

/// Compute units the strategy reported (including its own inner calls).
fn strategy_units(logs: &[String], strategy: &Pubkey) -> u64 {
    let prefix = format!("Program {strategy} consumed ");
    logs.iter()
        .find_map(|line| line.strip_prefix(&prefix))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .expect("strategy compute line")
}

struct Row {
    name: &'static str,
    total: u64,
    strategy: u64,
}

fn measure(
    name: &'static str,
    input_program: Pubkey,
    transfer_fee: Option<(u16, u64)>,
    integrator_bps: u16,
) -> Row {
    let mut env = Env::new();
    let mint_a = env.create_mint(&input_program, 6, transfer_fee, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let market = setup_cp_market_with_mints(
        &mut env,
        mint_a,
        input_program,
        mint_b,
        TOKEN,
        1_000_000_000,
        1_000_000_000,
        30,
        vec![],
    );
    let (trader, trader_a) = env.trader_with(&mint_a, &input_program, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let mut accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &mint_a,
        &trader_a,
        &mint_b,
        &trader_b,
    );
    let mut args = exact_in(1_000_000, 1);
    if integrator_bps > 0 {
        accounts.integrator_fee_account =
            Some(env.create_ata(&Pubkey::new_unique(), &mint_a, &input_program));
        args.integrator_fee_bps = integrator_bps;
    }
    let meta = env
        .send(&[client::swap(&accounts, &args)], &[&trader])
        .expect("swap");
    Row {
        name,
        total: meta.compute_units_consumed,
        strategy: strategy_units(&meta.logs, &CP_PROGRAM_ID),
    }
}

#[test]
fn router_overhead_stays_within_target() {
    let rows = [
        measure("SPL Token, no integrator fee", TOKEN, None, 0),
        measure("SPL Token, integrator fee", TOKEN, None, 50),
        measure("Token-2022, no extensions", TOKEN_2022, None, 0),
        measure(
            "Token-2022 with transfer fee",
            TOKEN_2022,
            Some((100, u64::MAX)),
            0,
        ),
    ];
    println!("| Swap (exact-in, CP strategy) | Total CU | Strategy CU | Router overhead CU |");
    println!("|---|---|---|---|");
    for row in &rows {
        let overhead = row.total - row.strategy;
        println!(
            "| {} | {} | {} | {} |",
            row.name, row.total, row.strategy, overhead
        );
        assert!(
            overhead <= ROUTER_OVERHEAD_TARGET,
            "{}: router overhead {overhead} CU exceeds {ROUTER_OVERHEAD_TARGET}",
            row.name
        );
    }
}
