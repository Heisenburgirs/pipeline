//! BOK runtime evidence for the router's `swap` handler: fee-account routing, pause,
//! reporting, aliasing, the hook split, exact-out and zero swaps, multi-hop fees.
//!
//! Invariants: `.bok/confirmed-invariants/04-swap-handler.md` and the LiteSVM parts of
//! `.bok/confirmed-invariants/01-settle.md`. Every test asserts the invariant on real
//! balances, the swap event and the return data.

#![allow(clippy::result_large_err)]

mod bok_swap_util;
mod common;

use bok_swap_util::*;
use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use pipeline_strategy_interface::{fees, TradeMode};
use solana_keypair::Keypair;
use solana_signer::Signer;

const RESERVE: u64 = 1_000_000_000;

struct Trader {
    keypair: Keypair,
    input: Pubkey,
    output: Pubkey,
}

fn cp_trader(env: &mut Env, m: &CpMarket, sell_a: bool, amount: u64) -> Trader {
    let (in_mint, out_mint) = if sell_a {
        (m.mint_a, m.mint_b)
    } else {
        (m.mint_b, m.mint_a)
    };
    let (keypair, input) = env.trader_with(&in_mint, &m.token_program(&in_mint), amount);
    let output = env.create_ata(&keypair.pubkey(), &out_mint, &m.token_program(&out_mint));
    Trader {
        keypair,
        input,
        output,
    }
}

fn cp_accounts(m: &CpMarket, t: &Trader, sell_a: bool) -> client::SwapAccounts {
    let (in_mint, out_mint) = if sell_a {
        (m.mint_a, m.mint_b)
    } else {
        (m.mint_b, m.mint_a)
    };
    cp_swap_accounts(
        m,
        &t.keypair.pubkey(),
        &in_mint,
        &t.input,
        &out_mint,
        &t.output,
    )
}

// ------------------------------------------------------------------- INV-SWAP-11

/// INV-SWAP-11: Pipeline's fee goes only to this market's fee account for the input
/// mint. Another market's fee account for the same mint, the trader's own input account
/// or any other account of the mint is refused with `WrongFeeAccount` and nothing
/// moves; on success the market's own fee account rises by `pipeline_fee − tfee` and no
/// other market's fee account changes. Run for SPL, Token-2022, and Token-2022 with a
/// transfer fee (uncapped 1%, and capped at 3 units).
/// A mint kind under test: label, token program, and an optional transfer fee
/// (basis points, maximum).
type MintKind = (&'static str, Pubkey, Option<(u16, u64)>);

#[test]
fn bok_inv_swap_11_fee_goes_only_to_this_markets_fee_account() {
    let kinds: [MintKind; 4] = [
        ("spl", TOKEN, None),
        ("token-2022", TOKEN_2022, None),
        ("token-2022 1% fee", TOKEN_2022, Some((100, u64::MAX))),
        ("token-2022 1% fee capped at 3", TOKEN_2022, Some((100, 3))),
    ];
    for (label, tp_a, tfee) in kinds {
        let mut env = Env::new();
        let mint_a = env.create_mint(&tp_a, 6, tfee, None);
        let mint_b = env.create_mint(&TOKEN, 6, None, None);
        // Two markets on the same pair (different CP fee tiers), so the same input mint
        // has two Pipeline fee accounts.
        let mine = setup_cp_market_with_mints(
            &mut env,
            mint_a,
            tp_a,
            mint_b,
            TOKEN,
            RESERVE,
            RESERVE,
            30,
            vec![],
        );
        let other = setup_cp_market_with_mints(
            &mut env,
            mint_a,
            tp_a,
            mint_b,
            TOKEN,
            RESERVE,
            RESERVE,
            100,
            vec![],
        );
        let my_fee = pda::fee_account(&mine.market, &mint_a, &tp_a);
        let other_fee = pda::fee_account(&other.market, &mint_a, &tp_a);
        let my_fee_b = pda::fee_account(&mine.market, &mint_b, &TOKEN);
        assert_eq!(
            my_fee,
            pda::associated_token(&pda::fee_authority(&mine.market), &mint_a, &tp_a)
        );
        assert_ne!(my_fee, other_fee);

        let t = cp_trader(&mut env, &mine, true, 10_000_000);
        let trader_other_account = {
            let owner = t.keypair.pubkey();
            // A second token account of the trader's in the input mint (not an ATA).
            let account = Keypair::new();
            let space = env.account_data(&t.input).len();
            let rent = env.svm.minimum_balance_for_rent_exemption(space);
            let create = solana_system_interface::instruction::create_account(
                &env.admin.pubkey(),
                &account.pubkey(),
                rent,
                space as u64,
                &tp_a,
            );
            let init = spl_token_2022_interface::instruction::initialize_account3(
                &tp_a,
                &account.pubkey(),
                &mint_a,
                &owner,
            )
            .unwrap();
            env.send(&[create, init], &[&account])
                .expect("second account");
            account.pubkey()
        };
        let watched = [
            t.input,
            t.output,
            my_fee,
            other_fee,
            my_fee_b,
            mine.vault_a,
            mine.vault_b,
            other.vault_a,
            trader_other_account,
        ];

        let amount = 1_000_000u64;
        for wrong in [other_fee, t.input, trader_other_account] {
            let before = balances(&env, &watched);
            let mut ix = client::swap(&cp_accounts(&mine, &t, true), &exact_in(amount, 0));
            assert_eq!(ix.accounts[ix::FEE_ACCOUNT].pubkey, my_fee);
            ix.accounts[ix::FEE_ACCOUNT].pubkey = wrong;
            assert_fails_with(send_swap(&mut env, ix, &t.keypair), "WrongFeeAccount");
            assert_eq!(balances(&env, &watched), before, "{label}: something moved");
        }

        let before = balances(&env, &watched);
        let ix = client::swap(&cp_accounts(&mine, &t, true), &exact_in(amount, 0));
        let meta = send_swap(&mut env, ix, &t.keypair).expect("own fee account");
        let (result, _) = reported(&meta);
        let after = balances(&env, &watched);
        let pipeline_fee = fees::pipeline_fee(amount);
        assert_eq!(result.pipeline_fee, pipeline_fee, "{label}");
        assert_eq!(
            after[2] - before[2],
            pipeline_fee - transfer_fee(pipeline_fee, tfee),
            "{label}: own fee account delta"
        );
        assert_eq!(after[3], before[3], "{label}: other market's fee account");
        assert_eq!(after[4], before[4], "{label}: output-mint fee account");
        assert_eq!(after[7], before[7], "{label}: other market's vault");
        assert_eq!(after[8], before[8], "{label}: trader's other account");
    }
}

// ------------------------------------------------------------------- INV-SWAP-18

/// INV-SWAP-18: a paused router moves nothing. `Paused` is the first check: it wins
/// over a wrong fee account and a missing integrator account. No balance (trader in and
/// out, both vaults, both fee accounts, integrator) and no trader lamport changes, for
/// exact-in, exact-out and integrator-fee swaps. Unpausing makes the same swap succeed,
/// so the fixture is otherwise valid.
#[test]
fn bok_inv_swap_18_paused_router_moves_nothing() {
    let mut env = Env::new();
    let m = setup_cp_market(&mut env, TOKEN, TOKEN, RESERVE, 2 * RESERVE, 30);
    let t = cp_trader(&mut env, &m, true, 10_000_000);
    let integrator = env.create_ata(&Pubkey::new_unique(), &m.mint_a, &TOKEN);
    let fee_a = pda::fee_account(&m.market, &m.mint_a, &TOKEN);
    let fee_b = pda::fee_account(&m.market, &m.mint_b, &TOKEN);
    let watched = [
        t.input, t.output, m.vault_a, m.vault_b, fee_a, fee_b, integrator,
    ];
    let admin = env.admin.pubkey();
    let pause = |paused| {
        client::update_config(
            &admin,
            &client::UpdateConfigArgs {
                paused: Some(paused),
                ..Default::default()
            },
        )
    };
    env.send(&[pause(true)], &[]).unwrap();

    let need = cp_in(RESERVE, 2 * RESERVE, 100_000, 30);
    let mut with_integrator = cp_accounts(&m, &t, true);
    with_integrator.integrator_fee_account = Some(integrator);
    let mut integrator_args = exact_in(1_000, 0);
    integrator_args.integrator_fee_bps = 100;
    let mut missing_integrator = exact_in(1_000, 0);
    missing_integrator.integrator_fee_bps = 100;
    let mut wrong_fee = client::swap(&cp_accounts(&m, &t, true), &exact_in(1_000, 0));
    wrong_fee.accounts[ix::FEE_ACCOUNT].pubkey = integrator;
    let cases = vec![
        (
            "exact-in",
            client::swap(&cp_accounts(&m, &t, true), &exact_in(1_000, 0)),
        ),
        (
            "exact-out",
            client::swap(
                &cp_accounts(&m, &t, true),
                &exact_out(need * 2, 100_000, 100_000),
            ),
        ),
        (
            "integrator",
            client::swap(&with_integrator, &integrator_args),
        ),
        ("wrong fee account", wrong_fee),
        (
            "missing integrator account",
            client::swap(&cp_accounts(&m, &t, true), &missing_integrator),
        ),
    ];
    for (label, ix) in &cases {
        let before = balances(&env, &watched);
        let lamports = env.lamports(&t.keypair.pubkey());
        assert_fails_with(send_swap(&mut env, ix.clone(), &t.keypair), "Paused");
        assert_eq!(balances(&env, &watched), before, "{label}: balances moved");
        assert_eq!(env.lamports(&t.keypair.pubkey()), lamports, "{label}");
    }

    env.send(&[pause(false)], &[]).unwrap();
    for (label, ix) in &cases[..3] {
        send_swap(&mut env, ix.clone(), &t.keypair)
            .unwrap_or_else(|e| panic!("{label} after unpause: {:?}", e.err));
    }
}

// ------------------------------------------------------------------- INV-SWAP-12

struct Case {
    label: &'static str,
    tfee: Option<(u16, u64)>,
    mode: TradeMode,
    integrator_bps: u16,
}

/// INV-SWAP-12 (adjusted): the return data and the event report exactly what moved.
/// For exact-in, exact-out with a refund, an integrator fee and a transfer-fee input:
/// event = return data on all four amounts; `received` = Δtrader_output;
/// Δfee_account = pipeline_fee − tfee; Δintegrator = integrator_fee − tfee; and the
/// trader's input fell by exactly `trader_spent + pipeline_fee + integrator_fee`, with
/// `net_sent = trader_spent` (no top-up in these honest swaps). The event's other fields
/// name the market, strategy, trader, mints, mode and integrator.
#[test]
fn bok_inv_swap_12_return_data_and_event_equal_what_moved() {
    let cases = [
        Case {
            label: "exact-in spl",
            tfee: None,
            mode: TradeMode::ExactIn,
            integrator_bps: 0,
        },
        Case {
            label: "exact-in spl + integrator",
            tfee: None,
            mode: TradeMode::ExactIn,
            integrator_bps: 75,
        },
        Case {
            label: "exact-out spl refund + integrator",
            tfee: None,
            mode: TradeMode::ExactOut,
            integrator_bps: 75,
        },
        Case {
            label: "exact-out spl refund",
            tfee: None,
            mode: TradeMode::ExactOut,
            integrator_bps: 0,
        },
        Case {
            label: "exact-in transfer-fee input + integrator",
            tfee: Some((100, u64::MAX)),
            mode: TradeMode::ExactIn,
            integrator_bps: 75,
        },
        Case {
            label: "exact-in transfer-fee input",
            tfee: Some((250, u64::MAX)),
            mode: TradeMode::ExactIn,
            integrator_bps: 0,
        },
    ];
    for case in cases {
        let label = case.label;
        let mut env = Env::new();
        let tp_a = if case.tfee.is_some() {
            TOKEN_2022
        } else {
            TOKEN
        };
        let mint_a = env.create_mint(&tp_a, 6, case.tfee, None);
        let mint_b = env.create_mint(&TOKEN, 6, None, None);
        let m = setup_cp_market_with_mints(
            &mut env,
            mint_a,
            tp_a,
            mint_b,
            TOKEN,
            RESERVE,
            2 * RESERVE,
            30,
            vec![],
        );
        let t = cp_trader(&mut env, &m, true, 100_000_000);
        let integrator = env.create_ata(&Pubkey::new_unique(), &mint_a, &tp_a);
        let fee_account = pda::fee_account(&m.market, &mint_a, &tp_a);
        let mut accounts = cp_accounts(&m, &t, true);
        if case.integrator_bps > 0 {
            accounts.integrator_fee_account = Some(integrator);
        }
        let reserve_a = env.token_balance(&m.vault_a);
        let reserve_b = env.token_balance(&m.vault_b);
        let (mut args, spend, expected_out) = match case.mode {
            TradeMode::ExactIn => {
                let amount = 1_000_001u64;
                let arrived = amount - transfer_fee(amount, case.tfee);
                (
                    exact_in(amount, 0),
                    amount,
                    cp_out(reserve_a, reserve_b, arrived, 30),
                )
            }
            TradeMode::ExactOut => {
                let want = 1_500_000u64;
                let need = cp_in(reserve_a, reserve_b, want, 30);
                (exact_out(need * 2, want, want), need, want)
            }
        };
        args.integrator_fee_bps = case.integrator_bps;

        let watched = [t.input, t.output, fee_account, integrator];
        let before = balances(&env, &watched);
        let meta = send_swap(&mut env, client::swap(&accounts, &args), &t.keypair)
            .unwrap_or_else(|e| panic!("{label}: {:?}", e.err));
        let after = balances(&env, &watched);
        let (r, event) = reported(&meta);

        assert_eq!(r.net_sent, spend, "{label}: net_sent = trader's real spend");
        assert_eq!(r.received, expected_out, "{label}: received");
        assert_eq!(r.received, after[1] - before[1], "{label}: Δtrader_output");
        assert_eq!(r.pipeline_fee, fees::pipeline_fee(r.net_sent), "{label}");
        assert_eq!(
            r.integrator_fee,
            fees::ceil_bps(r.net_sent, case.integrator_bps as u64).unwrap(),
            "{label}"
        );
        assert_eq!(
            after[2] - before[2],
            r.pipeline_fee - transfer_fee(r.pipeline_fee, case.tfee),
            "{label}: Δfee_account"
        );
        assert_eq!(
            after[3] - before[3],
            r.integrator_fee - transfer_fee(r.integrator_fee, case.tfee),
            "{label}: Δintegrator"
        );
        assert_eq!(
            before[0] - after[0],
            spend + r.pipeline_fee + r.integrator_fee,
            "{label}: input debit = trader_spent + fees"
        );

        assert_eq!(event.market, m.market, "{label}");
        assert_eq!(event.strategy, CP_PROGRAM_ID, "{label}");
        assert_eq!(event.trader, t.keypair.pubkey(), "{label}");
        assert_eq!((event.input_mint, event.output_mint), (mint_a, mint_b));
        assert_eq!(event.mode, case.mode as u8, "{label}");
        assert_eq!(event.integrator, accounts.integrator_fee_account, "{label}");
    }
}

/// INV-SWAP-12 / INV-SWAP-13 (adjusted, a31a5c5): neither fee account can be the
/// trader's input account, so no fee is ever reported that was not collected. Refused
/// with `FeeToTraderInput` (the integrator check applies even at 0 bps), nothing moves.
#[test]
fn bok_inv_swap_12_fee_accounts_equal_to_trader_input_are_refused() {
    let mut env = Env::new();
    let m = setup_cp_market(&mut env, TOKEN, TOKEN, RESERVE, 2 * RESERVE, 30);
    let t = cp_trader(&mut env, &m, true, 10_000_000);
    let fee_account = pda::fee_account(&m.market, &m.mint_a, &TOKEN);
    env.mint_to(&m.mint_a, &TOKEN, &fee_account, 5_000_000);
    let watched = [t.input, t.output, fee_account, m.vault_a, m.vault_b];

    for bps in [0u16, 10, 10_000, 65_535] {
        let mut accounts = cp_accounts(&m, &t, true);
        accounts.integrator_fee_account = Some(t.input);
        let mut args = exact_in(1_000_000, 0);
        args.integrator_fee_bps = bps;
        let before = balances(&env, &watched);
        assert_fails_with(
            send_swap(&mut env, client::swap(&accounts, &args), &t.keypair),
            "FeeToTraderInput",
        );
        assert_eq!(balances(&env, &watched), before, "integrator bps {bps}");
    }

    // trader_input = this market's fee account for the input mint (the fee-account
    // address check passes, so the aliasing check is what refuses it).
    let mut accounts = cp_accounts(&m, &t, true);
    accounts.trader_input = fee_account;
    let ix = client::swap(&accounts, &exact_in(1_000_000, 0));
    assert_eq!(ix.accounts[ix::FEE_ACCOUNT].pubkey, fee_account);
    let before = balances(&env, &watched);
    assert_fails_with(send_swap(&mut env, ix, &t.keypair), "FeeToTraderInput");
    assert_eq!(balances(&env, &watched), before);
}

// ------------------------------------------------------------------- INV-SWAP-13

/// INV-SWAP-13: `trader_input = trader_output` is refused (`SameMint` cannot apply as
/// the mints differ, so `checked_balance` refuses the output read with
/// `BadTokenAccount`); nothing moves.
#[test]
fn bok_inv_swap_13_input_equal_to_output_is_refused() {
    let mut env = Env::new();
    let m = setup_cp_market(&mut env, TOKEN, TOKEN, RESERVE, 2 * RESERVE, 30);
    let t = cp_trader(&mut env, &m, true, 10_000_000);
    let watched = [t.input, t.output, m.vault_a, m.vault_b];
    let before = balances(&env, &watched);
    let mut accounts = cp_accounts(&m, &t, true);
    accounts.trader_output = t.input;
    assert_fails_with(
        send_swap(
            &mut env,
            client::swap(&accounts, &exact_in(1_000, 0)),
            &t.keypair,
        ),
        "BadTokenAccount",
    );
    // Same account and the same mint on both sides.
    let mut ix = client::swap(&cp_accounts(&m, &t, true), &exact_in(1_000, 0));
    ix.accounts[ix::TRADER_OUTPUT].pubkey = t.input;
    ix.accounts[9].pubkey = m.mint_a;
    assert_fails_with(send_swap(&mut env, ix, &t.keypair), "SameMint");
    assert_eq!(balances(&env, &watched), before);
}

/// INV-SWAP-13: a market that declares the trader's own input account as its input
/// vault (`trader_input = input_vault`) cannot deliver output for input that never left
/// the trader: refused with `OutputWithoutInput`, nothing moves.
#[test]
fn bok_inv_swap_13_input_equal_to_vault_cannot_sell_nothing() {
    let mut env = Env::new();
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let victim = env.funded_keypair();
    let victim_a = env.create_ata(&victim.pubkey(), &mint_a, &TOKEN);
    let victim_b = env.create_ata(&victim.pubkey(), &mint_b, &TOKEN);
    env.mint_to(&mint_a, &TOKEN, &victim_a, 10_000_000);
    let m = evil_market(&mut env, mint_a, TOKEN, mint_b, Some(victim_a), RESERVE);
    let fee_a = pda::fee_account(&m.market, &mint_a, &TOKEN);
    let watched = [victim_a, victim_b, m.vault_b, fee_a];
    let before = balances(&env, &watched);
    let accounts = evil_swap_accounts(&m, &victim.pubkey(), &victim_a, &victim_b);
    for bps in [0u16, 100] {
        let mut accounts = accounts.clone();
        if bps > 0 {
            accounts.integrator_fee_account =
                Some(env.create_ata(&Pubkey::new_unique(), &mint_a, &TOKEN));
        }
        assert_fails_with(
            send_swap(
                &mut env,
                client::swap(&accounts, &evil_args(HONEST, 1_000, bps)),
                &victim,
            ),
            "OutputWithoutInput",
        );
        assert_eq!(balances(&env, &watched), before, "bps {bps}");
    }
}

/// INV-SWAP-13: an integrator account that aliases a market-controlled account (this
/// market's input vault or fee account, another market's input vault or fee account for
/// the same mint) cannot raise the trader's debit: it is exactly
/// `max_total_spend(amount_in, bps)` for an exact-in swap, the fee base stays
/// `amount_in` (an integrator fee landing in the vault comes after measurement), and
/// the target account rises by exactly the fee(s) it was named for.
#[test]
fn bok_inv_swap_13_integrator_aliasing_market_accounts_keeps_the_debit_bound() {
    for target in [
        "own input vault",
        "own fee account",
        "other vault",
        "other fee account",
    ] {
        for bps in [1u16, 500, 10_000, 65_535] {
            let mut env = Env::new();
            let mint_a = env.create_mint(&TOKEN, 6, None, None);
            let mint_b = env.create_mint(&TOKEN, 6, None, None);
            let mine = setup_cp_market_with_mints(
                &mut env,
                mint_a,
                TOKEN,
                mint_b,
                TOKEN,
                RESERVE,
                RESERVE,
                30,
                vec![],
            );
            let other = setup_cp_market_with_mints(
                &mut env,
                mint_a,
                TOKEN,
                mint_b,
                TOKEN,
                RESERVE,
                RESERVE,
                100,
                vec![],
            );
            let my_fee = pda::fee_account(&mine.market, &mint_a, &TOKEN);
            let other_fee = pda::fee_account(&other.market, &mint_a, &TOKEN);
            let integrator = match target {
                "own input vault" => mine.vault_a,
                "own fee account" => my_fee,
                "other vault" => other.vault_a,
                _ => other_fee,
            };
            let t = cp_trader(&mut env, &mine, true, 100_000_000);
            let amount = 1_000_000u64;
            let mut accounts = cp_accounts(&mine, &t, true);
            accounts.integrator_fee_account = Some(integrator);
            let mut args = exact_in(amount, 0);
            args.integrator_fee_bps = bps;
            let watched = [t.input, mine.vault_a, my_fee, other.vault_a, other_fee];
            let before = balances(&env, &watched);
            let meta = send_swap(&mut env, client::swap(&accounts, &args), &t.keypair)
                .unwrap_or_else(|e| panic!("{target} bps {bps}: {:?}", e.err));
            let after = balances(&env, &watched);
            let (r, _) = reported(&meta);
            let label = format!("{target} bps {bps}");
            let debit = before[0] - after[0];
            assert_eq!(r.net_sent, amount, "{label}");
            assert_eq!(
                debit,
                fees::max_total_spend(amount, bps).unwrap(),
                "{label}: debit"
            );
            assert_eq!(
                debit,
                r.net_sent + r.pipeline_fee + r.integrator_fee,
                "{label}"
            );
            let mut expected = [0u64; 5];
            expected[1] = amount; // the input itself
            expected[2] = r.pipeline_fee;
            let slot = watched.iter().position(|a| *a == integrator).unwrap();
            expected[slot] += r.integrator_fee;
            for i in 1..5 {
                assert_eq!(after[i] - before[i], expected[i], "{label}: account {i}");
            }
            assert_eq!(
                env.token_balance(&t.output),
                cp_out(RESERVE, RESERVE, amount, 30),
                "{label}: priced on amount_in, not on the integrator fee"
            );
        }
    }
}

/// INV-SWAP-13: a strategy may declare the market's own Pipeline fee account as its
/// input vault (`input_vault = fee_account`). The trader's input is then donated to
/// Pipeline, and Pipeline is not undercharged: `net_sent = amount_in`,
/// `pipeline_fee = pipeline_fee(amount_in)`, and the fee account rises by
/// `amount_in + pipeline_fee`, which is exactly the trader's debit.
#[test]
fn bok_inv_swap_13_vault_declared_as_fee_account_is_not_undercharged() {
    let mut env = Env::new();
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state = evil_state(&mint_a, &mint_b);
    let fee_a = pda::fee_account(&pda::market(&EVIL_ID, &state), &mint_a, &TOKEN);
    let m = evil_market(&mut env, mint_a, TOKEN, mint_b, Some(fee_a), RESERVE);
    assert_eq!(m.vault_a, fee_a);
    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let amount = 1_000_000u64;
    let before = balances(&env, &[trader_a, trader_b, fee_a]);
    let accounts = evil_swap_accounts(&m, &trader.pubkey(), &trader_a, &trader_b);
    let meta = send_swap(
        &mut env,
        client::swap(&accounts, &evil_args(HONEST, amount, 0)),
        &trader,
    )
    .expect("vault = fee account");
    let after = balances(&env, &[trader_a, trader_b, fee_a]);
    let (r, _) = reported(&meta);
    assert_eq!(r.net_sent, amount);
    assert_eq!(r.pipeline_fee, fees::pipeline_fee(amount));
    assert_eq!(before[0] - after[0], amount + r.pipeline_fee);
    assert_eq!(after[2] - before[2], amount + r.pipeline_fee);
    assert_eq!(after[1] - before[1], amount);
}

// ------------------------------------------------------------------- INV-SWAP-16

/// INV-SWAP-16 (router part): declaring more input-hook accounts than remaining accounts
/// were passed fails with the router's `BadHookAccounts` (not a panic), and nothing
/// moves. The boundary `k = remaining.len()` is accepted (all remaining accounts are
/// then also passed to the input transfer, which plain SPL ignores).
#[test]
fn bok_inv_swap_16_too_many_hook_accounts_is_bad_hook_accounts() {
    let mut env = Env::new();
    let m = setup_cp_market(&mut env, TOKEN, TOKEN, RESERVE, 2 * RESERVE, 30);
    let t = cp_trader(&mut env, &m, true, 10_000_000);
    let watched = [t.input, t.output, m.vault_a, m.vault_b];
    // The CP swap passes one remaining account (the output vault).
    let base = client::swap(&cp_accounts(&m, &t, true), &exact_in(1_000, 0));
    assert_eq!(base.data[HOOK_COUNT_OFFSET], 0);
    for k in [2u8, 3, 255] {
        let mut ix = base.clone();
        ix.data[HOOK_COUNT_OFFSET] = k;
        let before = balances(&env, &watched);
        assert_fails_with(send_swap(&mut env, ix, &t.keypair), "BadHookAccounts");
        assert_eq!(balances(&env, &watched), before, "k = {k}");
    }
    for k in [0u8, 1] {
        let mut ix = base.clone();
        ix.data[HOOK_COUNT_OFFSET] = k;
        let meta = send_swap(&mut env, ix, &t.keypair)
            .unwrap_or_else(|e| panic!("k = {k} should be accepted: {:?}", e.err));
        assert_eq!(reported(&meta).0.net_sent, 1_000);
    }
}

// ------------------------------------------------------------------- INV-SWAP-19

/// INV-SWAP-19 (adjusted): a swap where nothing moves is refused with `NothingSwapped`
/// (no zero event, no fee) and no balance changes, through the CP strategy and the
/// evil strategy's honest mode, with and without an integrator fee.
#[test]
fn bok_inv_swap_19_zero_swap_is_nothing_swapped() {
    let mut env = Env::new();
    let m = setup_cp_market(&mut env, TOKEN, TOKEN, RESERVE, 2 * RESERVE, 30);
    let t = cp_trader(&mut env, &m, true, 10_000_000);
    let integrator = env.create_ata(&Pubkey::new_unique(), &m.mint_a, &TOKEN);
    let fee_a = pda::fee_account(&m.market, &m.mint_a, &TOKEN);
    let watched = [t.input, t.output, m.vault_a, m.vault_b, fee_a, integrator];
    for bps in [0u16, 100] {
        let mut accounts = cp_accounts(&m, &t, true);
        if bps > 0 {
            accounts.integrator_fee_account = Some(integrator);
        }
        let mut args = exact_in(0, 0);
        args.integrator_fee_bps = bps;
        let before = balances(&env, &watched);
        assert_fails_with(
            send_swap(&mut env, client::swap(&accounts, &args), &t.keypair),
            "NothingSwapped",
        );
        assert_eq!(balances(&env, &watched), before, "cp bps {bps}");
    }

    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let e = evil_market(&mut env, mint_a, TOKEN, mint_b, None, RESERVE);
    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let watched = [trader_a, trader_b, e.vault_a, e.vault_b];
    let before = balances(&env, &watched);
    let accounts = evil_swap_accounts(&e, &trader.pubkey(), &trader_a, &trader_b);
    assert_fails_with(
        send_swap(
            &mut env,
            client::swap(&accounts, &evil_args(HONEST, 0, 0)),
            &trader,
        ),
        "NothingSwapped",
    );
    assert_eq!(balances(&env, &watched), before, "evil");
}

// ------------------------------------------------------------------- INV-SWAP-14

/// INV-SWAP-14: exact-out on a market that does not declare `supports_exact_out` is
/// refused with `ExactOutUnsupported` and nothing moves.
#[test]
fn bok_inv_swap_14_exact_out_unsupported_moves_nothing() {
    let mut env = Env::new();
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let e = evil_market(&mut env, mint_a, TOKEN, mint_b, None, RESERVE);
    assert!(!env.market(&e.market).supports_exact_out);
    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let watched = [trader_a, trader_b, e.vault_a, e.vault_b];
    let before = balances(&env, &watched);
    let accounts = evil_swap_accounts(&e, &trader.pubkey(), &trader_a, &trader_b);
    let mut args = exact_out(1_000, 1_000, 0);
    args.extra = vec![HONEST];
    assert_fails_with(
        send_swap(&mut env, client::swap(&accounts, &args), &trader),
        "ExactOutUnsupported",
    );
    assert_eq!(balances(&env, &watched), before);
}

/// INV-SWAP-14 (characterisation): the router checks exact-out swaps only against
/// `min_amount_out`, never against `amount_out`. With a transfer-fee output mint the
/// strategy sends `amount_out` but the trader receives `amount_out − tfee`:
/// `min_amount_out = 0` and `= amount_out − tfee` succeed with `received < amount_out`;
/// `min_amount_out = amount_out` fails with `InsufficientOutput`. The refund lowers the
/// fee base to what the trader really spent (`net_sent < amount_in`).
#[test]
fn bok_inv_swap_14_exact_out_checks_min_amount_out_not_amount_out() {
    let mut env = Env::new();
    let out_fee = Some((200u16, u64::MAX));
    // mint_a: Token-2022 with a transfer fee (the output); mint_b: SPL (the input).
    let mint_a = env.create_mint(&TOKEN_2022, 6, out_fee, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let m = setup_cp_market_with_mints(
        &mut env,
        mint_a,
        TOKEN_2022,
        mint_b,
        TOKEN,
        RESERVE,
        RESERVE,
        30,
        vec![],
    );
    let t = cp_trader(&mut env, &m, false, 100_000_000);
    let fee_b = pda::fee_account(&m.market, &mint_b, &TOKEN);
    let want = 1_000_000u64;
    let arrives = want - transfer_fee(want, out_fee);
    assert!(arrives < want);
    let max_in = 3 * want;

    let watched = [t.input, t.output, fee_b];
    let before = balances(&env, &watched);
    assert_fails_with(
        send_swap(
            &mut env,
            client::swap(&cp_accounts(&m, &t, false), &exact_out(max_in, want, want)),
            &t.keypair,
        ),
        "InsufficientOutput",
    );
    assert_eq!(balances(&env, &watched), before);

    for min_out in [0, arrives] {
        let before = balances(&env, &watched);
        let meta = send_swap(
            &mut env,
            client::swap(
                &cp_accounts(&m, &t, false),
                &exact_out(max_in, want, min_out),
            ),
            &t.keypair,
        )
        .unwrap_or_else(|e| panic!("min_out {min_out}: {:?}", e.err));
        let after = balances(&env, &watched);
        let (r, event) = reported(&meta);
        assert_eq!(event.mode, TradeMode::ExactOut as u8);
        assert_eq!(r.received, arrives, "received < amount_out is accepted");
        assert_eq!(after[1] - before[1], arrives);
        assert!(r.net_sent < max_in, "the refund lowers the fee base");
        assert_eq!(
            before[0] - after[0],
            r.net_sent + r.pipeline_fee,
            "fee base = trader's real spend"
        );
        assert_eq!(r.pipeline_fee, fees::pipeline_fee(r.net_sent));
        assert_eq!(after[2] - before[2], r.pipeline_fee);
    }
}

// ----------------------------------------------------------- INV-FEE-20 / FEE-7

/// INV-FEE-20 / INV-FEE-7: in a two-hop route (A→B on one market, B→C on another) each
/// hop pays its own fee on its own input, into its own market's fee account for its
/// input mint: Δfee(AB, A) = pipeline_fee(hop 1 input), Δfee(BC, B) =
/// pipeline_fee(hop 2 input), and no other fee account changes.
#[test]
fn bok_inv_fee_20_each_hop_pays_its_own_fee() {
    let mut env = Env::new();
    let ab = setup_cp_market(&mut env, TOKEN, TOKEN, RESERVE, RESERVE, 30);
    let mint_c = env.create_mint(&TOKEN, 6, None, None);
    let bc = setup_cp_market_with_mints(
        &mut env,
        ab.mint_b,
        TOKEN,
        mint_c,
        TOKEN,
        RESERVE,
        RESERVE,
        30,
        vec![],
    );
    let (trader, trader_a) = env.trader_with(&ab.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &ab.mint_b, &TOKEN);
    let trader_c = env.create_ata(&trader.pubkey(), &mint_c, &TOKEN);
    let first_in = 1_000_000u64;
    let first_out = cp_out(RESERVE, RESERVE, first_in, 30);
    let second_in = fees::amount_for_budget(first_out, 0);
    let fee_ab_a = pda::fee_account(&ab.market, &ab.mint_a, &TOKEN);
    let fee_ab_b = pda::fee_account(&ab.market, &ab.mint_b, &TOKEN);
    let fee_bc_b = pda::fee_account(&bc.market, &ab.mint_b, &TOKEN);
    let fee_bc_c = pda::fee_account(&bc.market, &mint_c, &TOKEN);
    let watched = [fee_ab_a, fee_ab_b, fee_bc_b, fee_bc_c, trader_a, trader_b];
    let before = balances(&env, &watched);
    let hop1 = client::swap(
        &cp_swap_accounts(
            &ab,
            &trader.pubkey(),
            &ab.mint_a,
            &trader_a,
            &ab.mint_b,
            &trader_b,
        ),
        &exact_in(first_in, first_out),
    );
    let hop2 = client::swap(
        &cp_swap_accounts(
            &bc,
            &trader.pubkey(),
            &ab.mint_b,
            &trader_b,
            &mint_c,
            &trader_c,
        ),
        &exact_in(second_in, 0),
    );
    let meta = env
        .send(&[compute(1_000_000), hop1, hop2], &[&trader])
        .unwrap();
    let after = balances(&env, &watched);
    let events = swap_events(&meta);
    assert_eq!(events.len(), 2);
    assert_eq!(
        (events[0].market, events[0].net_sent),
        (ab.market, first_in)
    );
    assert_eq!(
        (events[1].market, events[1].net_sent),
        (bc.market, second_in)
    );
    assert_eq!(events[0].pipeline_fee, fees::pipeline_fee(first_in));
    assert_eq!(events[1].pipeline_fee, fees::pipeline_fee(second_in));
    assert_eq!(after[0] - before[0], fees::pipeline_fee(first_in));
    assert_eq!(after[1], before[1]);
    assert_eq!(after[2] - before[2], fees::pipeline_fee(second_in));
    assert_eq!(after[3], before[3]);
    assert_eq!(
        before[4] - after[4],
        first_in + fees::pipeline_fee(first_in)
    );
    assert_eq!(
        after[5], 0,
        "hop 2 spent all of hop 1's output, fee included"
    );
    assert!(fees::pipeline_fee(first_in) + fees::pipeline_fee(second_in) >= 2);
}
