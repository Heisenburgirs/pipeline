//! SOS audit #2, H009: `settle` is fed the right balances.
//!
//! The settlement maths is proven for any six numbers; these tests check which accounts
//! produce those numbers when the router's input-side accounts alias each other. Anchor
//! gives `UncheckedAccount`s no duplicate protection and the router rejects only
//! `fee_account == trader_input` and `integrator == trader_input`, so every other pair
//! (`input_vault == fee_account`, `trader_input == input_vault`, `integrator ==
//! input_vault`, `integrator == fee_account`) is exercised here against the hostile
//! strategy's honest, short-output and over-refund behaviours.
//!
//! On every success the tests check, from balances measured directly in LiteSVM: the
//! reported `net_sent`, `received` and fees; that every watched account moved by exactly
//! the ledger those numbers imply (so a fee is never recorded without arriving in the
//! pinned fee account); the trader's debit is within `max_total_spend`; and the event
//! names the integrator account used. On every failure nothing moves.

#![allow(clippy::result_large_err)]

mod bok_swap_util;
mod common;

use std::collections::HashMap;

use bok_swap_util::*;
use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Pubkey};
use pipeline_strategy_interface::fees;
use solana_signer::Signer;

const RESERVE: u64 = 1_000_000_000;
const AMOUNT: u64 = 1_000_000;

const TAKE_MORE: u8 = 1;
const SHORT_OUTPUT: u8 = 2;
const REFUND_MORE: u8 = 3;
const STEAL_FEES: u8 = 6;

#[derive(Clone, Copy, Debug, PartialEq)]
enum VaultKind {
    /// The usual vault: the evil state PDA's ATA.
    StrategyOwned,
    /// The strategy declares the market's own Pipeline fee ATA as its input vault.
    FeeAccount,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum IntegratorKind {
    None,
    ThirdParty,
    InputVault,
    FeeAccount,
}

/// Expected per-account token deltas, accumulated by address so that aliased roles add up.
#[derive(Default)]
struct Ledger(HashMap<Pubkey, i128>);

impl Ledger {
    fn add(&mut self, account: Pubkey, delta: i128) {
        *self.0.entry(account).or_default() += delta;
    }

    fn get(&self, account: &Pubkey) -> i128 {
        self.0.get(account).copied().unwrap_or(0)
    }
}

fn deltas(before: &[u64], after: &[u64]) -> Vec<i128> {
    before
        .iter()
        .zip(after)
        .map(|(b, a)| *a as i128 - *b as i128)
        .collect()
}

/// Sends one swap and checks it against the ledger an honest measurement implies: the
/// router moved `AMOUNT` from `trader_input` to `input_vault`, the strategy paid `out`,
/// and both fees left `trader_input` for the fee account and the integrator account.
#[allow(clippy::too_many_arguments)]
fn swap_and_check(
    env: &mut Env,
    m: &EvilMarket,
    trader: &solana_keypair::Keypair,
    accounts: &client::SwapAccounts,
    behaviour: u8,
    bps: u16,
    fee_a: Pubkey,
    watched: &[Pubkey],
    label: &str,
) {
    let before = balances(env, watched);
    let result = send_swap(
        env,
        client::swap(accounts, &evil_args(behaviour, AMOUNT, bps)),
        trader,
    );
    let after = balances(env, watched);

    if behaviour == REFUND_MORE {
        // A strategy-owned vault refunds more than it got: neither side moved, so
        // `OutputWithoutInput`. A vault that is the fee ATA cannot be debited by the
        // strategy at all, so the refund itself fails. Either way, nothing moves.
        assert!(
            result.is_err(),
            "H009 {label}: an over-refund must not settle"
        );
        assert_eq!(before, after, "H009 {label}: a refused swap moved tokens");
        return;
    }

    let meta = result.unwrap_or_else(|e| {
        panic!(
            "{label}: setup swap must succeed: {:?}\n{}",
            e.err,
            e.meta.logs.join("\n")
        )
    });
    let (r, event) = reported(&meta);
    let out = if behaviour == SHORT_OUTPUT { 1 } else { AMOUNT };
    let pipeline_fee = fees::pipeline_fee(AMOUNT);
    let integrator_fee = fees::ceil_bps(AMOUNT, bps as u64).unwrap();

    assert_eq!(r.net_sent, AMOUNT, "H009 {label}: net_sent");
    assert_eq!(r.received, out, "H009 {label}: received");
    assert_eq!(r.pipeline_fee, pipeline_fee, "H009 {label}: pipeline fee");
    assert_eq!(
        r.integrator_fee, integrator_fee,
        "H009 {label}: integrator fee"
    );
    assert_eq!(
        event.integrator, accounts.integrator_fee_account,
        "H009 {label}: event integrator"
    );
    assert_eq!(event.market, m.market, "H009 {label}: event market");

    let mut ledger = Ledger::default();
    ledger.add(
        accounts.trader_input,
        -((AMOUNT + pipeline_fee + integrator_fee) as i128),
    );
    ledger.add(accounts.input_vault, AMOUNT as i128);
    ledger.add(fee_a, pipeline_fee as i128);
    if let Some(integrator) = accounts.integrator_fee_account {
        ledger.add(integrator, integrator_fee as i128);
    }
    ledger.add(accounts.trader_output, out as i128);
    ledger.add(m.vault_b, -(out as i128));
    let moved = deltas(&before, &after);
    for (i, account) in watched.iter().enumerate() {
        assert_eq!(
            moved[i],
            ledger.get(account),
            "H009 {label}: account {i} ({account}) moved differently from what was settled"
        );
    }

    let debit = (before[0] - after[0]) as u128;
    assert!(
        debit <= fees::max_total_spend(AMOUNT, bps).unwrap() as u128,
        "H009 {label}: trader debit {debit} over max_total_spend"
    );
    // The fee account received at least the recorded Pipeline fee, and Pipeline's fee is
    // at least 25 bps of what the input vault kept from this swap.
    let fee_index = watched.iter().position(|a| *a == fee_a).unwrap();
    assert!(
        moved[fee_index] >= r.pipeline_fee as i128,
        "H009 {label}: pipeline fee recorded but not collected"
    );
    let vault_index = watched
        .iter()
        .position(|a| *a == accounts.input_vault)
        .unwrap();
    let mut vault_kept = moved[vault_index];
    if accounts.input_vault == fee_a {
        vault_kept -= r.pipeline_fee as i128;
    }
    if accounts.integrator_fee_account == Some(accounts.input_vault) {
        vault_kept -= r.integrator_fee as i128;
    }
    assert!(
        r.pipeline_fee >= fees::pipeline_fee(vault_kept.clamp(0, AMOUNT as i128) as u64),
        "H009 {label}: pipeline fee below 25 bps of what the vault kept"
    );
}

fn alias_matrix(tp_a: Pubkey, vault_kind: VaultKind) {
    let mut env = Env::new();
    let mint_a = env.create_mint(&tp_a, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state = evil_state(&mint_a, &mint_b);
    let fee_a = pda::fee_account(&pda::market(&EVIL_ID, &state), &mint_a, &tp_a);
    let declared = match vault_kind {
        VaultKind::StrategyOwned => None,
        VaultKind::FeeAccount => Some(fee_a),
    };
    let m = evil_market(&mut env, mint_a, tp_a, mint_b, declared, 10 * RESERVE);
    if vault_kind == VaultKind::FeeAccount {
        assert_eq!(m.vault_a, fee_a, "setup: vault declared as the fee ATA");
    }
    let (trader, trader_a) = env.trader_with(&mint_a, &tp_a, 100 * RESERVE);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let third = env.create_ata(&Pubkey::new_unique(), &mint_a, &tp_a);
    let watched = [trader_a, trader_b, m.vault_a, fee_a, third, m.vault_b];
    let mut cases = 0;

    for integrator in [
        IntegratorKind::None,
        IntegratorKind::ThirdParty,
        IntegratorKind::InputVault,
        IntegratorKind::FeeAccount,
    ] {
        let integrator_account = match integrator {
            IntegratorKind::None => None,
            IntegratorKind::ThirdParty => Some(third),
            IntegratorKind::InputVault => Some(m.vault_a),
            IntegratorKind::FeeAccount => Some(fee_a),
        };
        let rates: &[u16] = match integrator {
            IntegratorKind::None => &[0],
            _ => &[0, 1, 250, 10_000],
        };
        for &bps in rates {
            for behaviour in [HONEST, SHORT_OUTPUT, REFUND_MORE] {
                let mut accounts = evil_swap_accounts(&m, &trader.pubkey(), &trader_a, &trader_b);
                accounts.integrator_fee_account = integrator_account;
                let label = format!(
                    "tp {tp_a} vault {vault_kind:?} integrator {integrator:?} bps {bps} \
                     behaviour {behaviour}"
                );
                swap_and_check(
                    &mut env, &m, &trader, &accounts, behaviour, bps, fee_a, &watched, &label,
                );
                cases += 1;
            }
        }
    }

    println!("tp {tp_a} vault {vault_kind:?}: {cases} alias cases checked");
    assert_eq!(cases, 39);

    // Everything in the fee ATA (for a vault declared as the fee ATA, that includes the
    // traders' input) goes to the treasury, and the market keeps trading afterwards with
    // a fresh `vault_before`.
    let treasury_owner = env.treasury_owner;
    let treasury = env.create_ata(&treasury_owner, &mint_a, &tp_a);
    let collected = env.token_balance(&fee_a);
    assert!(collected > 0, "setup: fees were collected");
    env.send(
        &[client::sweep_fees(
            &m.market,
            &mint_a,
            &tp_a,
            &treasury_owner,
            vec![],
        )],
        &[],
    )
    .expect("sweep");
    assert_eq!(
        env.token_balance(&treasury),
        collected,
        "H009: sweep moved everything"
    );
    assert_eq!(env.token_balance(&fee_a), 0);
    let accounts = evil_swap_accounts(&m, &trader.pubkey(), &trader_a, &trader_b);
    swap_and_check(
        &mut env,
        &m,
        &trader,
        &accounts,
        HONEST,
        0,
        fee_a,
        &watched,
        &format!("tp {tp_a} vault {vault_kind:?} after sweep"),
    );
}

/// `integrator` aliased to the input vault, to the fee ATA or to a third party, on a
/// normal hostile market (SPL input mint).
#[test]
fn h009_integrator_aliases_on_hostile_market_spl() {
    alias_matrix(TOKEN, VaultKind::StrategyOwned);
}

#[test]
fn h009_integrator_aliases_on_hostile_market_token_2022() {
    alias_matrix(TOKEN_2022, VaultKind::StrategyOwned);
}

/// `input_vault == fee_account`, alone and together with `integrator == input_vault ==
/// fee_account`; consecutive swaps also make `vault_before` include earlier fees.
#[test]
fn h009_vault_declared_as_fee_account_matrix_spl() {
    alias_matrix(TOKEN, VaultKind::FeeAccount);
}

#[test]
fn h009_vault_declared_as_fee_account_matrix_token_2022() {
    alias_matrix(TOKEN_2022, VaultKind::FeeAccount);
}

// ------------------------------------------------------------ trader_input == input_vault

/// Rewrites a token account so `delegate` may spend `delegated_amount` of it (crafted
/// state: the vault's real owner, a PDA of the hostile strategy, never signs an approve).
fn set_delegate(env: &mut Env, account: &Pubkey, delegate: &Pubkey, delegated_amount: u64) {
    let mut data = env.svm.get_account(account).expect("token account");
    data.data[72..76].copy_from_slice(&1u32.to_le_bytes());
    data.data[76..108].copy_from_slice(delegate.as_ref());
    data.data[121..129].copy_from_slice(&delegated_amount.to_le_bytes());
    env.svm.set_account(*account, data).unwrap();
}

/// `trader_input == input_vault`, in both ways the trader can get debit authority over
/// the market's vault: the strategy declared the trader's own account as its vault, or
/// (crafted) the strategy-owned vault names the trader as a delegate, so the strategy can
/// also move the vault's tokens. The router's transfer is then a self-transfer. Honest,
/// short-output, over-refund and take-more behaviours, with and without an integrator,
/// must all refuse and move nothing: the strategy kept nothing.
fn input_is_vault(tp_a: Pubkey, strategy_owned_vault: bool) {
    let mut env = Env::new();
    let mint_a = env.create_mint(&tp_a, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let trader = env.funded_keypair();
    let state = evil_state(&mint_a, &mint_b);
    let shared = if strategy_owned_vault {
        env.create_ata(&state, &mint_a, &tp_a)
    } else {
        env.create_ata(&trader.pubkey(), &mint_a, &tp_a)
    };
    env.mint_to(&mint_a, &tp_a, &shared, 10_000_000);
    if strategy_owned_vault {
        set_delegate(&mut env, &shared, &trader.pubkey(), u64::MAX);
    }
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let m = evil_market(&mut env, mint_a, tp_a, mint_b, Some(shared), RESERVE);
    let fee_a = pda::fee_account(&m.market, &mint_a, &tp_a);
    let third = env.create_ata(&Pubkey::new_unique(), &mint_a, &tp_a);
    let watched = [shared, trader_b, m.vault_b, fee_a, third];

    // The setup is what it claims: the trader can debit the shared account, which is the
    // market's vault.
    assert_eq!(env.market(&m.market).vaults[0], shared, "setup: vault[A]");

    for integrator in [None, Some(third), Some(fee_a)] {
        for behaviour in [HONEST, SHORT_OUTPUT, REFUND_MORE, TAKE_MORE] {
            let mut accounts = evil_swap_accounts(&m, &trader.pubkey(), &shared, &trader_b);
            accounts.integrator_fee_account = integrator;
            let bps = if integrator.is_some() { 500 } else { 0 };
            let label = format!(
                "tp {tp_a} strategy-owned {strategy_owned_vault} integrator {integrator:?} \
                 behaviour {behaviour}"
            );
            let before = balances(&env, &watched);
            let result = send_swap(
                &mut env,
                client::swap(&accounts, &evil_args(behaviour, AMOUNT, bps)),
                &trader,
            );
            if let Ok(meta) = &result {
                let (r, _) = reported(meta);
                panic!("H009 {label}: settled a swap whose input never left the trader: {r:?}");
            }
            assert_eq!(
                balances(&env, &watched),
                before,
                "H009 {label}: a refused swap moved tokens"
            );
            if behaviour == HONEST || behaviour == SHORT_OUTPUT {
                assert_fails_with(result, "OutputWithoutInput");
            } else if behaviour == REFUND_MORE && strategy_owned_vault {
                // The strategy can sign for its vault, so the over-refund is a
                // self-transfer too, and the router sees nothing kept.
                assert_fails_with(result, "OutputWithoutInput");
            }
        }
    }
}

#[test]
fn h009_trader_input_is_traders_own_vault_spl() {
    input_is_vault(TOKEN, false);
}

#[test]
fn h009_trader_input_is_traders_own_vault_token_2022() {
    input_is_vault(TOKEN_2022, false);
}

#[test]
fn h009_trader_input_is_strategy_vault_via_crafted_delegate_spl() {
    input_is_vault(TOKEN, true);
}

#[test]
fn h009_trader_input_is_strategy_vault_via_crafted_delegate_token_2022() {
    input_is_vault(TOKEN_2022, true);
}

/// `trader_input == input_vault` with the strategy depositing into that account during
/// the call (from a token account the market signer controls). The vault "kept" one
/// unit, so `net_sent = 1` and the fee is charged on it and actually arrives in the fee
/// ATA; the strategy chose to give its output away (accepted collusion, as with a direct
/// call). Checks that the alias never yields a fee that is recorded but not collected,
/// and that the trader's debit stays within `max_total_spend`.
#[test]
fn h009_trader_input_is_vault_with_strategy_deposit_charges_what_was_kept() {
    for bps in [0u16, 500] {
        let mut env = Env::new();
        let mint_a = env.create_mint(&TOKEN, 6, None, None);
        let mint_b = env.create_mint(&TOKEN, 6, None, None);
        let trader = env.funded_keypair();
        let shared = env.create_ata(&trader.pubkey(), &mint_a, &TOKEN);
        env.mint_to(&mint_a, &TOKEN, &shared, 10_000_000);
        let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
        let m = evil_market(&mut env, mint_a, TOKEN, mint_b, Some(shared), RESERVE);
        let fee_a = pda::fee_account(&m.market, &mint_a, &TOKEN);
        let market_owned = env.create_ata(&m.market, &mint_a, &TOKEN);
        env.mint_to(&mint_a, &TOKEN, &market_owned, 10);
        let third = env.create_ata(&Pubkey::new_unique(), &mint_a, &TOKEN);

        let mut accounts = evil_swap_accounts(&m, &trader.pubkey(), &shared, &trader_b);
        accounts
            .strategy_accounts
            .push(AccountMeta::new(market_owned, false));
        if bps > 0 {
            accounts.integrator_fee_account = Some(third);
        }
        let watched = [shared, trader_b, m.vault_b, fee_a, market_owned, third];
        let before = balances(&env, &watched);
        let meta = send_swap(
            &mut env,
            client::swap(&accounts, &evil_args(STEAL_FEES, AMOUNT, bps)),
            &trader,
        )
        .unwrap_or_else(|e| {
            panic!(
                "setup: deposit swap: {:?}\n{}",
                e.err,
                e.meta.logs.join("\n")
            )
        });
        let after = balances(&env, &watched);
        let moved = deltas(&before, &after);
        let (r, event) = reported(&meta);
        let integrator_fee = fees::ceil_bps(1, bps as u64).unwrap();

        assert_eq!(
            r.net_sent, 1,
            "H009 bps {bps}: net_sent is what the vault kept"
        );
        assert_eq!(r.received, AMOUNT);
        assert_eq!(r.pipeline_fee, fees::pipeline_fee(1));
        assert_eq!(r.integrator_fee, integrator_fee);
        assert_eq!(event.integrator, accounts.integrator_fee_account);
        // Ledger: +1 from the market-owned account, then both fees out.
        let expected = [
            1 - (r.pipeline_fee + integrator_fee) as i128,
            AMOUNT as i128,
            -(AMOUNT as i128),
            r.pipeline_fee as i128,
            -1,
            integrator_fee as i128,
        ];
        assert_eq!(moved, expected, "H009 bps {bps}: balances vs settlement");
        let debit = (before[0] as i128 - after[0] as i128).max(0) as u64;
        assert!(debit <= fees::max_total_spend(AMOUNT, bps).unwrap());
    }
}
