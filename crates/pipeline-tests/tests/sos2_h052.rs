//! SOS2 H052: any strategy-owned account can be registered as a market.
//!
//! `register_existing_market` checks only that `market_state` is owned by the strategy
//! (`fetch_market_info`). The record is keyed by `(strategy, market_state)`, so a forged
//! record can't occupy a real market's PDA, and the strategy's own `pipeline_market_info`
//! decides mints and vaults. The router-side question is whether a third party can record
//! a non-market account (a vault, or a strategy account of another type) of a Pipeline
//! strategy. Pipeline's own strategies read their state through typed Anchor accounts
//! (`MarketInfoAccounts`), so the discriminator check in the strategy should refuse.
//!
//! These tests confirm the safe behaviour holds for the CP strategy:
//!
//! 1. A token account the strategy holds a vault in (owned by the token program) is
//!    refused by the router's owner check, and no record is written.
//! 2. A strategy-owned account with a pool's layout but a wrong discriminator is refused
//!    by the strategy, and no record is written.
//! 3. The real pool's record is untouched by either attempt.
//!
//! Third-party strategies that skip the discriminator check (the STRATEGY-GUIDE rule 4
//! warning) can record such accounts. That is a strategy-side issue; the evil fixture
//! covers it in `sos2_h019.rs`.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const DISCRIMINATOR_LEN: usize = 8;

/// A CP market with liquidity, plus the registration payer.
fn cp_env() -> (Env, CpMarket, Keypair) {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000, 1_000_000, 30);
    let payer = env.funded_keypair();
    (env, market, payer)
}

fn assert_no_record(env: &Env, strategy: &Pubkey, market_state: &Pubkey) {
    let address = pda::market(strategy, market_state);
    let exists = env
        .svm
        .get_account(&address)
        .is_some_and(|account| !account.data.is_empty());
    assert!(!exists, "H052: a market record was written at {address}");
}

/// Property 1 (test): the CP strategy's vault token account (owned by the token program)
/// can't be registered as a market, because the router requires strategy ownership.
#[test]
fn h052_cp_vault_token_account_cannot_be_registered() {
    let (mut env, cp, payer) = cp_env();
    let pool_record_before = env.account_data(&cp.market);

    let ix = client::register_existing_market(&payer.pubkey(), &CP_PROGRAM_ID, &cp.vault_a, vec![]);
    let result = env.send(&[ix], &[&payer]);
    assert_fails_with(result, "MarketStateNotOwnedByStrategy");

    assert_no_record(&env, &CP_PROGRAM_ID, &cp.vault_a);
    assert_eq!(
        env.account_data(&cp.market),
        pool_record_before,
        "H052: attempt altered the real pool's market record"
    );
}

/// Property 2 (test): a CP-owned account with a valid pool layout but a zeroed
/// discriminator (so it is not a pool) is refused by the strategy's typed account
/// check, and no record is written. The router records only what the strategy returns.
#[test]
fn h052_cp_non_pool_account_with_pool_layout_cannot_be_registered() {
    let (mut env, cp, payer) = cp_env();
    let pool_record_before = env.account_data(&cp.market);

    let mut data = env.account_data(&cp.pool);
    for byte in data.iter_mut().take(DISCRIMINATOR_LEN) {
        *byte = 0;
    }
    let forged = Pubkey::new_unique();
    let lamports = env.svm.minimum_balance_for_rent_exemption(data.len());
    env.svm
        .set_account(
            forged,
            Account {
                lamports,
                data,
                owner: CP_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let ix = client::register_existing_market(&payer.pubkey(), &CP_PROGRAM_ID, &forged, vec![]);
    let result = env.send(&[ix], &[&payer]);
    assert_fails_with(result, "AccountDiscriminatorMismatch");

    assert_no_record(&env, &CP_PROGRAM_ID, &forged);
    assert_eq!(
        env.account_data(&cp.market),
        pool_record_before,
        "H052: attempt altered the real pool's market record"
    );
}
