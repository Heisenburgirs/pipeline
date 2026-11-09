//! SOS audit #2, H077: a trader who is the delegate of someone else's `trader_input`.
//!
//! The router only requires the trader to be the signer; the token program decides
//! whether the trader may move the owner's tokens. With a delegation, the amount, the
//! pipeline fee and the integrator fee all come out of the same delegated allowance. The
//! checks here are: a delegated allowance equal to `amount_in` alone makes the swap fail
//! at the fee transfer with nothing moved; an allowance that covers `amount_in` plus the
//! fee succeeds, and the owner loses exactly that much, no more; the allowance is spent
//! down to zero; and the event names the signing delegate as `trader`.

mod common;

use common::*;
use pipeline_client::{self as client, AccountMeta, Instruction, Pubkey};
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const AMOUNT_IN: u64 = 1_000_000;
/// 25 bps of `AMOUNT_IN` (the swap's net spend), rounded up.
const PIPELINE_FEE: u64 = 2_500;
/// SPL token account layout: `delegated_amount` is a u64 at bytes 121..129.
const DELEGATED_AMOUNT: std::ops::Range<usize> = 121..129;

/// SPL Token `Approve` (instruction 4): `owner` lets `delegate` move up to `amount`.
fn approve_ix(source: &Pubkey, delegate: &Pubkey, owner: &Pubkey, amount: u64) -> Instruction {
    let mut data = vec![4u8];
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction::new_with_bytes(
        TOKEN,
        &data,
        vec![
            AccountMeta::new(*source, false),
            AccountMeta::new_readonly(*delegate, false),
            AccountMeta::new_readonly(*owner, true),
        ],
    )
}

struct Fixture {
    market: CpMarket,
    owner_a: Pubkey,
    delegate: Keypair,
    delegate_b: Pubkey,
}

/// A CP market with liquidity, an owner holding `mint_a`, and a delegate (the trader)
/// approved for `allowance` of the owner's `mint_a`.
fn fixture(env: &mut Env, allowance: u64) -> Fixture {
    let market = setup_cp_market(env, TOKEN, TOKEN, 1_000_000_000, 2_000_000_000, 30);
    let (owner, owner_a) = env.trader_with(&market.mint_a, &TOKEN, 100_000_000);
    let delegate = env.funded_keypair();
    let delegate_b = env.create_ata(&delegate.pubkey(), &market.mint_b, &TOKEN);
    let approve = approve_ix(&owner_a, &delegate.pubkey(), &owner.pubkey(), allowance);
    env.send(&[approve], &[&owner]).expect("approve");
    Fixture {
        market,
        owner_a,
        delegate,
        delegate_b,
    }
}

fn swap_ix(fixture: &Fixture) -> Instruction {
    let accounts = cp_swap_accounts(
        &fixture.market,
        &fixture.delegate.pubkey(),
        &fixture.market.mint_a,
        &fixture.owner_a,
        &fixture.market.mint_b,
        &fixture.delegate_b,
    );
    client::swap(&accounts, &exact_in(AMOUNT_IN, 0))
}

fn delegated_amount(env: &Env, account: &Pubkey) -> u64 {
    let data = env.account_data(account);
    u64::from_le_bytes(data[DELEGATED_AMOUNT].try_into().unwrap())
}

#[test]
fn allowance_equal_to_amount_in_fails_at_the_fee_and_moves_nothing() {
    let mut env = Env::new();
    let fixture = fixture(&mut env, AMOUNT_IN);
    let owner_before = env.token_balance(&fixture.owner_a);
    let vault_before = env.token_balance(&fixture.market.vault(&fixture.market.mint_a));

    let result = env.send(&[swap_ix(&fixture)], &[&fixture.delegate]);

    assert!(result.is_err(), "swap spent the fee beyond the allowance");
    assert_eq!(env.token_balance(&fixture.owner_a), owner_before);
    assert_eq!(
        env.token_balance(&fixture.market.vault(&fixture.market.mint_a)),
        vault_before
    );
    assert_eq!(env.token_balance(&fixture.delegate_b), 0);
    assert_eq!(delegated_amount(&env, &fixture.owner_a), AMOUNT_IN);
}

#[test]
fn allowance_covering_fee_debits_owner_exactly_and_names_delegate_as_trader() {
    let mut env = Env::new();
    let fixture = fixture(&mut env, AMOUNT_IN + PIPELINE_FEE);
    let owner_before = env.token_balance(&fixture.owner_a);

    let meta = env
        .send(&[swap_ix(&fixture)], &[&fixture.delegate])
        .expect("swap with a fee-covering allowance");

    let debited = owner_before - env.token_balance(&fixture.owner_a);
    assert_eq!(debited, AMOUNT_IN + PIPELINE_FEE);
    assert_eq!(delegated_amount(&env, &fixture.owner_a), 0);
    assert!(env.token_balance(&fixture.delegate_b) > 0);

    let events = swap_events(&meta);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].pipeline_fee, PIPELINE_FEE);
    assert_eq!(events[0].trader, fixture.delegate.pubkey());
}

#[test]
fn spent_allowance_cannot_fund_a_second_swap() {
    let mut env = Env::new();
    let fixture = fixture(&mut env, AMOUNT_IN + PIPELINE_FEE);
    env.send(&[swap_ix(&fixture)], &[&fixture.delegate])
        .expect("first swap");
    let owner_before = env.token_balance(&fixture.owner_a);

    let result = env.send(&[swap_ix(&fixture)], &[&fixture.delegate]);

    assert!(result.is_err(), "second swap drew on a spent allowance");
    assert_eq!(env.token_balance(&fixture.owner_a), owner_before);
}
