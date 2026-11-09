//! SOS audit #2, H003 (RECHECK of audit #1 H017): `create_market` must never forward the
//! creator's signature to the strategy, whatever aliases or duplicates of the creator, the
//! setup payer or the market PDA the caller passes, and however the creator was emptied
//! (Market rent from Anchor's `init`, the setup allowance, the transaction fee).
//!
//! The hostile setup is `test-strategy-evil` mode 7 (`INIT_APPROVES_CREATOR_TOKENS`): it
//! calls SPL `Approve(u64::MAX)` on strategy account 8 (a token account) to its state PDA,
//! with strategy account 9 as the authority/signer. If the router forwarded the creator's
//! signature, the creator's token account would get a delegate.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

const HONEST: u8 = 0;
const INIT_APPROVES_CREATOR_TOKENS: u8 = 7;
/// `8 + Market::INIT_SPACE`.
const MARKET_SPACE: usize = 8 + 32 + 32 + 1 + 4 * 32 + 4 * 32 + 1 + 32 + 8 + 1 + 1;
/// The evil strategy's market state (`STATE_LEN`), which its setup pays rent for.
const STATE_LEN: usize = 32 * 4 + 2;

struct Evil {
    mint_a: Pubkey,
    mint_b: Pubkey,
    state: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
    market: Pubkey,
}

impl Evil {
    fn setup_payer(&self) -> Pubkey {
        pda::setup_payer(&self.market)
    }
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

/// `create_market` on the evil strategy with `market_state` chosen by the caller.
fn create_ix(
    creator: &Pubkey,
    evil: &Evil,
    market_state: Pubkey,
    mode: u8,
    max_creator_lamports: u64,
    extra: Vec<AccountMeta>,
) -> Instruction {
    let mut strategy_accounts = vec![
        AccountMeta::new_readonly(evil.mint_a, false),
        AccountMeta::new_readonly(evil.mint_b, false),
        AccountMeta::new(evil.vault_a, false),
        AccountMeta::new(evil.vault_b, false),
    ];
    strategy_accounts.extend(extra);
    client::create_market(&client::CreateMarket {
        creator: *creator,
        strategy_program: EVIL_ID,
        market_state,
        market_state_signs: market_state == *creator,
        init_data: vec![mode],
        max_creator_lamports,
        strategy_accounts,
    })
}

fn rent(env: &Env, len: usize) -> u64 {
    env.svm.minimum_balance_for_rent_exemption(len)
}

fn delegate_of(env: &Env, token_account: &Pubkey) -> Option<Pubkey> {
    let data = env.account_data(token_account);
    (data[72] == 1).then(|| Pubkey::new_from_array(data[76..108].try_into().unwrap()))
}

/// A creator with exactly `balance` lamports and a token account holding tokens.
fn creator_with(env: &mut Env, balance: u64) -> (Keypair, Pubkey) {
    let creator = Keypair::new();
    if balance > 0 {
        env.svm.airdrop(&creator.pubkey(), balance).unwrap();
    }
    assert_eq!(env.lamports(&creator.pubkey()), balance);
    let mint = env.create_mint(&TOKEN, 6, None, None);
    let tokens = env.create_ata(&creator.pubkey(), &mint, &TOKEN);
    env.mint_to(&mint, &TOKEN, &tokens, 1_000_000);
    assert_eq!(env.token_balance(&tokens), 1_000_000);
    assert_eq!(delegate_of(env, &tokens), None);
    (creator, tokens)
}

/// The mode-7 tail: the creator's token account (writable), the creator as the
/// authority (signer), the token program.
fn approve_tail(creator: &Pubkey, tokens: &Pubkey) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(*tokens, false),
        AccountMeta::new_readonly(*creator, true),
        AccountMeta::new_readonly(TOKEN, false),
    ]
}

/// Asserts the attack was refused by the runtime: the creator's signature was not in the
/// CPI, no delegate was set, and the creator lost nothing (the admin pays the fee).
fn assert_refused(env: &Env, result: TxResult, creator: &Pubkey, tokens: &Pubkey, before: u64) {
    if let Ok(meta) = &result {
        panic!(
            "H003: create_market succeeded, so the creator's signature reached the strategy:\n{}",
            meta.logs.join("\n")
        );
    }
    assert_eq!(
        delegate_of(env, tokens),
        None,
        "H003: the strategy approved a delegate on the creator's token account"
    );
    assert_fails_with(result, "signer privilege escalated");
    assert_eq!(env.lamports(creator), before, "creator lost lamports");
}

/// Control: the test fixture can detect forwarding. A genuinely fresh signer that is not
/// the creator (zero lamports, owns a token account) keeps its signature (accepted
/// design), and the evil setup approves its tokens. Every negative result below is
/// therefore meaningful on this router binary.
#[test]
fn h003_control_fresh_non_creator_signer_is_forwarded() {
    let mut env = registered_evil_env();
    let creator = env.funded_keypair();
    let (fresh, fresh_tokens) = creator_with(&mut env, 0);
    let evil = evil_accounts(&mut env);
    let ix = create_ix(
        &creator.pubkey(),
        &evil,
        evil.state,
        INIT_APPROVES_CREATOR_TOKENS,
        SOL,
        approve_tail(&fresh.pubkey(), &fresh_tokens),
    );
    env.send(&[ix], &[&creator, &fresh])
        .unwrap_or_else(|e| panic!("control create_market failed: {:?}", e.err));
    assert_eq!(delegate_of(&env, &fresh_tokens), Some(evil.state));
}

/// The exact-balance arithmetic of the drain cases: a creator holding exactly
/// `market rent + allowance` can create an honest market, and ends with
/// `allowance - state rent`. So at the strategy CPI it held exactly 0 lamports.
#[test]
fn h003_exact_balance_arithmetic_drains_the_creator_to_zero() {
    let mut env = registered_evil_env();
    let market_rent = rent(&env, MARKET_SPACE);
    let allowance = SOL;
    let (creator, _) = creator_with(&mut env, market_rent + allowance);
    let evil = evil_accounts(&mut env);
    let ix = create_ix(
        &creator.pubkey(),
        &evil,
        evil.state,
        HONEST,
        allowance,
        vec![],
    );
    env.send(&[ix], &[&creator])
        .unwrap_or_else(|e| panic!("honest create_market failed: {:?}", e.err));
    assert_eq!(env.lamports(&evil.market), market_rent);
    assert_eq!(
        env.lamports(&creator.pubkey()),
        allowance - rent(&env, STATE_LEN)
    );
}

/// (b) Audit #1 H017 reproduction: a creator emptied to 0 by Market rent plus the
/// allowance, listed once as a signer strategy account. Before the fix this succeeded.
#[test]
fn h003_drained_creator_in_remaining_is_not_forwarded() {
    let mut env = registered_evil_env();
    let market_rent = rent(&env, MARKET_SPACE);
    let allowance = SOL;
    let (creator, tokens) = creator_with(&mut env, market_rent + allowance);
    let evil = evil_accounts(&mut env);
    let ix = create_ix(
        &creator.pubkey(),
        &evil,
        evil.state,
        INIT_APPROVES_CREATOR_TOKENS,
        allowance,
        approve_tail(&creator.pubkey(), &tokens),
    );
    let before = env.lamports(&creator.pubkey());
    let result = env.send(&[ix], &[&creator]);
    assert_refused(&env, result, &creator.pubkey(), &tokens, before);
}

/// (b) The drained creator listed three times: read-only signer (the authority slot),
/// writable signer, and read-only non-signer. Every copy is excluded by key, so no meta
/// for the creator carries `is_signer` and the runtime's OR-merge has nothing to merge.
#[test]
fn h003_creator_repeated_with_mixed_flags_is_not_forwarded() {
    let mut env = registered_evil_env();
    let market_rent = rent(&env, MARKET_SPACE);
    let allowance = SOL;
    for balance in [market_rent + allowance, 10 * SOL] {
        let (creator, tokens) = creator_with(&mut env, balance);
        let evil = evil_accounts(&mut env);
        let mut extra = approve_tail(&creator.pubkey(), &tokens);
        extra.push(AccountMeta::new(creator.pubkey(), true));
        extra.push(AccountMeta::new_readonly(creator.pubkey(), false));
        let ix = create_ix(
            &creator.pubkey(),
            &evil,
            evil.state,
            INIT_APPROVES_CREATOR_TOKENS,
            allowance,
            extra,
        );
        let before = env.lamports(&creator.pubkey());
        let result = env.send(&[ix], &[&creator]);
        assert_refused(&env, result, &creator.pubkey(), &tokens, before);
    }
}

/// (d) The setup payer (fresh before funding) and the market PDA duplicated in the
/// remaining accounts, writable, next to the drained creator. Neither duplicate can make
/// the creator a signer: they are different keys, and neither is a transaction signer.
#[test]
fn h003_setup_payer_and_market_duplicates_do_not_forward_the_creator() {
    let mut env = registered_evil_env();
    let market_rent = rent(&env, MARKET_SPACE);
    let allowance = SOL;
    let (creator, tokens) = creator_with(&mut env, market_rent + allowance);
    let evil = evil_accounts(&mut env);
    assert!(env
        .svm
        .get_account(&evil.setup_payer())
        .is_none_or(|a| a.lamports == 0));
    let mut extra = approve_tail(&creator.pubkey(), &tokens);
    extra.push(AccountMeta::new(evil.setup_payer(), false));
    extra.push(AccountMeta::new(evil.market, false));
    extra.push(AccountMeta::new(creator.pubkey(), true));
    let ix = create_ix(
        &creator.pubkey(),
        &evil,
        evil.state,
        INIT_APPROVES_CREATOR_TOKENS,
        allowance,
        extra,
    );
    let before = env.lamports(&creator.pubkey());
    let result = env.send(&[ix], &[&creator]);
    assert_refused(&env, result, &creator.pubkey(), &tokens, before);
}

/// (d) Control: the same duplicates with an honest setup are harmless. The market is
/// recorded correctly, the setup payer ends empty and the creator pays only Market rent
/// plus the state's rent.
#[test]
fn h003_setup_payer_and_market_duplicates_are_harmless_when_honest() {
    let mut env = registered_evil_env();
    let creator = env.funded_keypair();
    let evil = evil_accounts(&mut env);
    let extra = vec![
        AccountMeta::new(evil.setup_payer(), false),
        AccountMeta::new(evil.market, false),
        AccountMeta::new_readonly(evil.setup_payer(), false),
    ];
    let ix = create_ix(&creator.pubkey(), &evil, evil.state, HONEST, SOL, extra);
    let before = env.lamports(&creator.pubkey());
    env.send(&[ix], &[&creator])
        .unwrap_or_else(|e| panic!("honest create_market with duplicates: {:?}", e.err));
    let market = env.market(&evil.market);
    assert_eq!(market.created_by, creator.pubkey());
    assert_eq!(market.market_state, evil.state);
    assert_eq!(market.strategy_program, EVIL_ID);
    assert_eq!(env.lamports(&evil.setup_payer()), 0);
    assert_eq!(
        before - env.lamports(&creator.pubkey()),
        rent(&env, MARKET_SPACE) + rent(&env, STATE_LEN)
    );
}

/// Anchor's `init` debit alone (before the handler) empties the creator: allowance 0, the
/// creator holds exactly the Market rent, and a third party pre-funded the setup payer so
/// the evil setup can still pay its state rent. Judging freshness "before the creator
/// pays" is not enough here; the key exclusion is what protects the creator.
#[test]
fn h003_creator_emptied_by_market_rent_alone_is_not_forwarded() {
    let mut env = registered_evil_env();
    let market_rent = rent(&env, MARKET_SPACE);
    let (creator, tokens) = creator_with(&mut env, market_rent);
    let evil = evil_accounts(&mut env);
    env.svm.airdrop(&evil.setup_payer(), SOL / 10).unwrap();
    assert_eq!(env.lamports(&evil.setup_payer()), SOL / 10);

    // Setup check: with an honest setup the pre-funded payer is enough and allowance 0
    // works for a creator holding exactly the Market rent.
    {
        let (probe, _) = creator_with(&mut env, market_rent);
        let probe_evil = evil_accounts(&mut env);
        env.svm
            .airdrop(&probe_evil.setup_payer(), SOL / 10)
            .unwrap();
        let ix = create_ix(
            &probe.pubkey(),
            &probe_evil,
            probe_evil.state,
            HONEST,
            0,
            vec![],
        );
        env.send(&[ix], &[&probe])
            .unwrap_or_else(|e| panic!("honest create_market, allowance 0: {:?}", e.err));
        assert_eq!(
            env.lamports(&probe.pubkey()),
            SOL / 10 - rent(&env, STATE_LEN),
            "the creator was emptied by init and then refunded the pre-funding"
        );
    }

    let ix = create_ix(
        &creator.pubkey(),
        &evil,
        evil.state,
        INIT_APPROVES_CREATOR_TOKENS,
        0,
        approve_tail(&creator.pubkey(), &tokens),
    );
    let before = env.lamports(&creator.pubkey());
    let result = env.send(&[ix], &[&creator]);
    assert_refused(&env, result, &creator.pubkey(), &tokens, before);
}

/// The creator is also the transaction fee payer, holding exactly fee + Market rent +
/// allowance: the runtime's fee deduction and the router's debits together empty it.
#[test]
fn h003_creator_as_fee_payer_emptied_by_fee_and_router_is_not_forwarded() {
    let mut env = registered_evil_env();
    let market_rent = rent(&env, MARKET_SPACE);
    let allowance = SOL;
    let fee = 5_000;
    let (creator, tokens) = creator_with(&mut env, fee + market_rent + allowance);
    let evil = evil_accounts(&mut env);
    let ix = create_ix(
        &creator.pubkey(),
        &evil,
        evil.state,
        INIT_APPROVES_CREATOR_TOKENS,
        allowance,
        approve_tail(&creator.pubkey(), &tokens),
    );
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&creator.pubkey()),
        &[&creator],
        env.svm.latest_blockhash(),
    );
    let result = env.svm.send_transaction(tx);
    if let Ok(meta) = &result {
        panic!(
            "H003: creator-as-fee-payer create_market succeeded:\n{}",
            meta.logs.join("\n")
        );
    }
    assert_eq!(
        delegate_of(&env, &tokens),
        None,
        "H003: the strategy approved a delegate on the fee-paying creator's tokens"
    );
    assert_fails_with(result, "signer privilege escalated");
    // Only the fee was charged, which confirms the drain arithmetic was exact.
    assert_eq!(env.lamports(&creator.pubkey()), market_rent + allowance);
}

/// (a)/(c) `market_state == creator` (drained to 0), with the creator also as the mode-7
/// authority. The router excludes it by key; the evil setup's own seeds check rejects the
/// state address first, and nothing about the creator changes. Whether the router would
/// have forwarded the signature through the `market_state` slot is settled by code
/// reading (no fixture uses a signature at account 1 before validating it).
#[test]
fn h003_creator_as_market_state_cannot_become_a_market() {
    let mut env = registered_evil_env();
    let market_rent = rent(&env, MARKET_SPACE);
    let allowance = SOL;
    let (creator, tokens) = creator_with(&mut env, market_rent + allowance);
    let evil = evil_accounts(&mut env);
    let market = pda::market(&EVIL_ID, &creator.pubkey());
    let ix = create_ix(
        &creator.pubkey(),
        &evil,
        creator.pubkey(),
        INIT_APPROVES_CREATOR_TOKENS,
        allowance,
        approve_tail(&creator.pubkey(), &tokens),
    );
    let before = env.lamports(&creator.pubkey());
    let result = env.send(&[ix], &[&creator]);
    if let Err(e) = &result {
        println!("creator as market state: {:?}", e.err);
        let logs = e.meta.logs.join("\n");
        println!("{logs}");
    }
    assert!(result.is_err(), "H003: creator accepted as market state");
    let account = env.svm.get_account(&creator.pubkey()).unwrap();
    assert_eq!(account.owner, client::SYSTEM_PROGRAM_ID);
    assert!(account.data.is_empty());
    assert_eq!(account.lamports, before);
    assert_eq!(delegate_of(&env, &tokens), None);
    assert!(env
        .svm
        .get_account(&market)
        .is_none_or(|a| a.data.is_empty()));
}
