//! SOS H017: can `create_market` make the creator's own wallet "fresh" so that its
//! signature reaches the strategy, although docs say the creator's signature never does?
//!
//! The router debits the creator (Anchor `init` rent for the Market account, then the
//! setup-payer funding of `max_creator_lamports`) BEFORE it evaluates `is_fresh` on the
//! remaining accounts. If the creator also passes its own key as a remaining account and
//! its balance is exactly what the router takes, it reads as 0 lamports / empty /
//! system-owned, and its signer flag is forwarded to the untrusted strategy.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

/// evil `INIT_APPROVES_CREATOR_TOKENS` (mode 7): approve account 8 to the state PDA with
/// account 9 as the authority/signer. The creator wallet is passed as account 9.
fn approve_creator_tokens_ix(
    env: &mut Env,
    creator: &Pubkey,
    creator_tokens: &Pubkey,
    max_creator_lamports: u64,
) -> Instruction {
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    client::create_market(&client::CreateMarket {
        creator: *creator,
        strategy_program: EVIL_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![7],
        max_creator_lamports,
        strategy_accounts: vec![
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
            AccountMeta::new(*creator_tokens, false),
            // The creator passes ITSELF (a funded wallet, not a new keypair).
            AccountMeta::new_readonly(*creator, true),
            AccountMeta::new_readonly(TOKEN, false),
        ],
    })
}

/// Learns the rent the router's `init` of the Market account takes from the creator.
fn market_rent(env: &mut Env) -> u64 {
    let probe = env.funded_keypair();
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    let ix = client::create_market(&client::CreateMarket {
        creator: probe.pubkey(),
        strategy_program: EVIL_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![0],
        max_creator_lamports: SOL,
        strategy_accounts: vec![
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
        ],
    });
    env.send(&[ix], &[&probe]).unwrap();
    env.lamports(&pda::market(&EVIL_ID, &state))
}

/// A funded creator holding tokens, with exactly `market_rent + allowance + extra` lamports.
fn creator_with_balance(env: &mut Env, balance: u64) -> (Keypair, Pubkey) {
    let creator = Keypair::new();
    env.svm.airdrop(&creator.pubkey(), balance).unwrap();
    let mint = env.create_mint(&TOKEN, 6, None, None);
    let tokens = env.create_ata(&creator.pubkey(), &mint, &TOKEN);
    env.mint_to(&mint, &TOKEN, &tokens, 1_000_000);
    (creator, tokens)
}

fn delegate_of(env: &Env, token_account: &Pubkey) -> Option<Pubkey> {
    let data = env.account_data(token_account);
    (data[72] == 1).then(|| Pubkey::new_from_array(data[76..108].try_into().unwrap()))
}

/// The bug: a FUNDED wallet that is not a new keypair has its signature forwarded,
/// because the router itself drains it to zero before the freshness check.
/// Safe behaviour (asserted): the creator's signature never reaches the strategy.
#[test]
fn router_drained_creator_wallet_keeps_its_signature_in_setup() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let rent = market_rent(&mut env);
    let allowance = SOL;
    let (creator, tokens) = creator_with_balance(&mut env, rent + allowance);
    println!(
        "market rent = {rent}, creator balance = {}",
        rent + allowance
    );
    assert_eq!(env.lamports(&creator.pubkey()), rent + allowance);

    let ix = approve_creator_tokens_ix(&mut env, &creator.pubkey(), &tokens, allowance);
    let result = env.send(&[ix], &[&creator]);
    let delegate = delegate_of(&env, &tokens);
    println!(
        "tx ok: {}, delegate on creator's token account: {:?}",
        result.is_ok(),
        delegate
    );
    if let Err(e) = &result {
        println!("error: {:?}", e.err);
    }
    // Safe behaviour: the strategy must not be able to approve the creator's tokens.
    assert!(
        delegate.is_none(),
        "the strategy approved a delegate on a funded creator's token account"
    );
}

/// Control: with one lamport more than the router takes the wallet is not fresh, its
/// signature is stripped and the same attack fails. Shows the exact threshold.
#[test]
fn creator_with_one_spare_lamport_is_not_forwarded() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let rent = market_rent(&mut env);
    let allowance = SOL;
    let (creator, tokens) = creator_with_balance(&mut env, rent + allowance + 1);
    let ix = approve_creator_tokens_ix(&mut env, &creator.pubkey(), &tokens, allowance);
    let result = env.send(&[ix], &[&creator]);
    println!(
        "one spare lamport: {:?}",
        result.as_ref().err().map(|e| e.err.clone())
    );
    assert_fails_with(result, "privilege escalated");
    assert!(delegate_of(&env, &tokens).is_none());
}
