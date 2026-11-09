//! SOS audit #2, H039: a third party who sends the rent-exempt minimum to a fresh signer of a
//! pending `create_market` makes `is_fresh` false, so the router strips that signature
//! and the strategy's use of it fails. The attempt must fail atomically (no market, no
//! delegate, no lamports moved from the creator), the victim's keypair must still be able
//! to recover its lamports, and a retry with a new keypair must succeed.
//!
//! The strategy is `test-strategy-evil` mode 7 (`INIT_APPROVES_CREATOR_TOKENS`), whose
//! `Approve` needs the wallet at account 9 to sign. The victim is that wallet: a fresh
//! keypair that the creator also signs with, so the router is what decides whether it
//! keeps its signature.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

const INIT_APPROVES_CREATOR_TOKENS: u8 = 7;

struct Evil {
    mint_a: Pubkey,
    mint_b: Pubkey,
    state: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
    market: Pubkey,
}

fn registered_evil_env() -> Env {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    env
}

fn evil_accounts(env: &mut Env) -> Evil {
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    Evil {
        mint_a,
        mint_b,
        state,
        vault_a,
        vault_b,
        market: pda::market(&EVIL_ID, &state),
    }
}

/// `create_market` on the evil strategy, with the mode-7 tail: the wallet's token account
/// (writable), the wallet as authority (signer), the token program.
fn create_ix(
    creator: &Pubkey,
    evil: &Evil,
    wallet: &Pubkey,
    wallet_tokens: &Pubkey,
) -> Instruction {
    client::create_market(&client::CreateMarket {
        creator: *creator,
        strategy_program: EVIL_ID,
        market_state: evil.state,
        market_state_signs: false,
        init_data: vec![INIT_APPROVES_CREATOR_TOKENS],
        max_creator_lamports: SOL,
        strategy_accounts: vec![
            AccountMeta::new_readonly(evil.mint_a, false),
            AccountMeta::new_readonly(evil.mint_b, false),
            AccountMeta::new(evil.vault_a, false),
            AccountMeta::new(evil.vault_b, false),
            AccountMeta::new(*wallet_tokens, false),
            AccountMeta::new_readonly(*wallet, true),
            AccountMeta::new_readonly(TOKEN, false),
        ],
    })
}

fn system_transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction::new_with_bytes(
        client::SYSTEM_PROGRAM_ID,
        &data,
        vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
    )
}

fn delegate_of(env: &Env, token_account: &Pubkey) -> Option<Pubkey> {
    let data = env.account_data(token_account);
    (data[72] == 1).then(|| Pubkey::new_from_array(data[76..108].try_into().unwrap()))
}

/// A fresh wallet with zero lamports that owns a token account holding tokens.
fn fresh_wallet_with_tokens(env: &mut Env) -> (Keypair, Pubkey) {
    let wallet = Keypair::new();
    let mint = env.create_mint(&TOKEN, 6, None, None);
    let tokens = env.create_ata(&wallet.pubkey(), &mint, &TOKEN);
    env.mint_to(&mint, &TOKEN, &tokens, 1_000_000);
    assert_eq!(env.lamports(&wallet.pubkey()), 0);
    assert_eq!(delegate_of(env, &tokens), None);
    (wallet, tokens)
}

/// Control: the same fresh wallet with zero lamports keeps its signature, and the evil
/// setup approves its tokens. The grief test below differs only by the pre-funding.
#[test]
fn h039_control_fresh_wallet_signature_is_forwarded() {
    let mut env = registered_evil_env();
    let creator = env.funded_keypair();
    let (wallet, tokens) = fresh_wallet_with_tokens(&mut env);
    let evil = evil_accounts(&mut env);
    let ix = create_ix(&creator.pubkey(), &evil, &wallet.pubkey(), &tokens);
    env.send(&[ix], &[&creator, &wallet])
        .unwrap_or_else(|e| panic!("control create_market failed: {:?}", e.err));
    assert_eq!(delegate_of(&env, &tokens), Some(evil.state));
}

/// Grief: a third party sends the rent-exempt minimum for an empty account (the runtime
/// rejects any smaller non-zero balance) to the pending fresh wallet. The router strips its
/// signature, the strategy's `Approve` fails, and the whole `create_market` reverts.
#[test]
fn h039_one_lamport_prefund_strips_fresh_signer_and_the_attempt_reverts() {
    let mut env = registered_evil_env();
    let creator = env.funded_keypair();
    let attacker = env.funded_keypair();
    let (wallet, tokens) = fresh_wallet_with_tokens(&mut env);
    let evil = evil_accounts(&mut env);

    let grief = env.svm.minimum_balance_for_rent_exemption(0);
    env.send(
        &[system_transfer(&attacker.pubkey(), &wallet.pubkey(), grief)],
        &[&attacker],
    )
    .unwrap_or_else(|e| panic!("pre-fund failed: {:?}", e.err));
    assert_eq!(env.lamports(&wallet.pubkey()), grief);

    let creator_before = env.lamports(&creator.pubkey());
    let ix = create_ix(&creator.pubkey(), &evil, &wallet.pubkey(), &tokens);
    let result = env.send(&[ix], &[&creator, &wallet]);
    assert!(
        result.is_err(),
        "H039: create_market succeeded although the fresh wallet was pre-funded"
    );
    // Atomic: no market, no delegate, and the creator paid nothing beyond the fee the
    // admin pays in this helper.
    assert_eq!(delegate_of(&env, &tokens), None);
    assert_eq!(env.lamports(&evil.market), 0);
    assert_eq!(env.lamports(&creator.pubkey()), creator_before);

    // The victim keypair can recover its lamports, so the grief costs it only a retry.
    env.send(
        &[system_transfer(&wallet.pubkey(), &creator.pubkey(), grief)],
        &[&wallet],
    )
    .unwrap_or_else(|e| panic!("victim could not recover its lamports: {:?}", e.err));
    assert_eq!(env.lamports(&wallet.pubkey()), 0);

    // A retry with a new fresh keypair goes through.
    let (retry, retry_tokens) = fresh_wallet_with_tokens(&mut env);
    let retry_ix = create_ix(&creator.pubkey(), &evil, &retry.pubkey(), &retry_tokens);
    env.send(&[retry_ix], &[&creator, &retry])
        .unwrap_or_else(|e| panic!("retry with a new keypair failed: {:?}", e.err));
    assert_eq!(delegate_of(&env, &retry_tokens), Some(evil.state));
}
