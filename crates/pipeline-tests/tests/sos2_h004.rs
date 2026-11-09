//! SOS audit #2, H004: `create_market` judges freshness per instruction, from live
//! lamports. A wallet that signed the transaction and holds 0 lamports when the router looks
//! (emptied by the fee, by its own earlier instruction, or by an earlier router instruction
//! in the same transaction) looks fresh, so its signature reaches the strategy when its key
//! is one of `create_market`'s strategy accounts.
//!
//! Status: accepted and documented (the router docs' known limitations, "`create_market` forwards
//! fresh signers"): the router can't tell a new keypair from a zero-SOL wallet, so callers
//! must sign `create_market` only with new keypairs. These tests pin that behaviour: every
//! zero-lamport case below is forwarded, and a funded wallet is not.
//!
//! Signer flags are transaction-wide: a key that signs the transaction is a signer in every
//! instruction that references it, whatever `AccountMeta::is_signer` the client wrote for
//! that instruction. Several tests below list the victim with `is_signer = false`.
//!
//! The hostile setup is `test-strategy-evil` mode 7 (`INIT_APPROVES_CREATOR_TOKENS`): it
//! calls SPL `Approve(u64::MAX)` on strategy account 8 (a token account) to its state PDA,
//! with strategy account 9 as the authority. A delegate on the victim's token account means
//! the router forwarded the victim's signature.
//!
//! Cases: (a) fee payer emptied by the fee, (d) fee payer emptied by its own earlier
//! transfer, (b) creator of an earlier `create_market`, (c) payer of an earlier
//! `register_strategy`. In (b) and (c) the router's own earlier instruction produced the
//! zero balance; the documented rule covers these too.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

const HONEST: u8 = 0;
const INIT_DRAINS_PAYER: u8 = 4;
const INIT_APPROVES_CREATOR_TOKENS: u8 = 7;
/// `8 + Market::INIT_SPACE`.
const MARKET_SPACE: usize = 8 + 32 + 32 + 1 + 4 * 32 + 4 * 32 + 1 + 32 + 8 + 1 + 1;
/// The evil strategy's market state (`STATE_LEN`), which its setup pays rent for.
const STATE_LEN: usize = 32 * 4 + 2;
/// LiteSVM's default fee per signature.
const FEE_PER_SIGNATURE: u64 = 5_000;

struct Evil {
    mint_a: Pubkey,
    mint_b: Pubkey,
    state: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
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
    }
}

fn create_ix(
    creator: &Pubkey,
    evil: &Evil,
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
        market_state: evil.state,
        market_state_signs: false,
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

/// A token account owned by `owner`, holding tokens, with no delegate.
fn tokens_for(env: &mut Env, owner: &Pubkey) -> Pubkey {
    let mint = env.create_mint(&TOKEN, 6, None, None);
    let tokens = env.create_ata(owner, &mint, &TOKEN);
    env.mint_to(&mint, &TOKEN, &tokens, 1_000_000);
    assert_eq!(env.token_balance(&tokens), 1_000_000);
    assert_eq!(delegate_of(env, &tokens), None);
    tokens
}

/// A wallet with exactly `balance` lamports (airdropped) and a token account.
fn wallet_with(env: &mut Env, balance: u64) -> (Keypair, Pubkey) {
    let wallet = Keypair::new();
    env.svm.airdrop(&wallet.pubkey(), balance).unwrap();
    assert_eq!(env.lamports(&wallet.pubkey()), balance);
    let tokens = tokens_for(env, &wallet.pubkey());
    (wallet, tokens)
}

/// The mode-7 tail: the victim's token account (writable), the victim as the authority
/// (with the given client-side signer flag), the token program.
fn approve_tail(victim: &Pubkey, tokens: &Pubkey, marked_signer: bool) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(*tokens, false),
        AccountMeta::new_readonly(*victim, marked_signer),
        AccountMeta::new_readonly(TOKEN, false),
    ]
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

/// Sends a transaction paid for by `payer` (not the admin).
fn send_paid_by(
    env: &mut Env,
    payer: &Keypair,
    instructions: &[Instruction],
    signers: &[&Keypair],
) -> TxResult {
    env.svm.expire_blockhash();
    let mut all: Vec<&Keypair> = vec![payer];
    all.extend_from_slice(signers);
    let tx = Transaction::new_signed_with_payer(
        instructions,
        Some(&payer.pubkey()),
        &all,
        env.svm.latest_blockhash(),
    );
    let size = bincode::serialize(&tx).unwrap().len();
    assert!(size <= MAX_TRANSACTION_SIZE, "transaction is {size} bytes");
    env.svm.send_transaction(tx)
}

fn expect_ok(result: TxResult, what: &str) {
    if let Err(e) = result {
        panic!(
            "setup step failed ({what}): {:?}\n{}",
            e.err,
            e.meta.logs.join("\n")
        );
    }
}

/// The documented behaviour (the router docs' known limitations) for a wallet that holds 0 lamports
/// when `create_market` looks: it is treated as fresh, its signature reaches the strategy and
/// the strategy's `Approve` sets itself as delegate on the wallet's tokens.
fn assert_forwarded(
    env: &Env,
    result: TxResult,
    victim_tokens: &Pubkey,
    strategy_state: &Pubkey,
    case: &str,
) {
    expect_ok(result, case);
    assert_eq!(
        delegate_of(env, victim_tokens),
        Some(*strategy_state),
        "H004 {case}: a zero-lamport signer is forwarded to the strategy (documented caveat)"
    );
}

// ------------------------------------------------------------------------- controls

/// Control: a funded fee payer listed (unmarked) as the mode-7 authority is not forwarded.
/// This is the safe baseline for (a) and (d): a wallet holding SOL keeps its signature
/// away from the strategy, even though it is a transaction signer.
#[test]
fn h004_control_funded_fee_payer_is_not_forwarded() {
    let mut env = registered_evil_env();
    let (payer, tokens) = wallet_with(&mut env, SOL);
    let creator = env.funded_keypair();
    let evil = evil_accounts(&mut env);
    let ix = create_ix(
        &creator.pubkey(),
        &evil,
        INIT_APPROVES_CREATOR_TOKENS,
        SOL / 10,
        approve_tail(&payer.pubkey(), &tokens, false),
    );
    let result = send_paid_by(&mut env, &payer, &[ix], &[&creator]);
    assert_eq!(delegate_of(&env, &tokens), None);
    assert_fails_with(result, "signer privilege escalated");
}

// ------------------------------------------------- (a), (d): accepted design, pinned

/// (a) A separate fee payer holding exactly the transaction fee (crafted with
/// `set_account`, since an airdrop below the rent-exempt minimum is refused). After the fee
/// it has 0 lamports, so the router sees it as fresh. Listed with `is_signer = false`, it is
/// still a signer (transaction-wide flag), and its tokens are approved. Accepted design:
/// the wallet holds no SOL when the router looks, and the router didn't empty it.
#[test]
fn h004_a_fee_payer_emptied_by_the_fee_looks_fresh_accepted() {
    let mut env = registered_evil_env();
    let fee = 2 * FEE_PER_SIGNATURE;
    let craft = |env: &mut Env| -> (Keypair, Pubkey) {
        let wallet = Keypair::new();
        env.svm
            .set_account(
                wallet.pubkey(),
                Account {
                    lamports: fee,
                    data: vec![],
                    owner: client::SYSTEM_PROGRAM_ID,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        let tokens = tokens_for(env, &wallet.pubkey());
        (wallet, tokens)
    };

    // Setup check: the runtime lets this fee payer pay exactly the fee and reach 0.
    let (probe, probe_tokens) = craft(&mut env);
    let probe_creator = env.funded_keypair();
    let probe_evil = evil_accounts(&mut env);
    let ix = create_ix(
        &probe_creator.pubkey(),
        &probe_evil,
        HONEST,
        SOL / 10,
        approve_tail(&probe.pubkey(), &probe_tokens, false),
    );
    expect_ok(
        send_paid_by(&mut env, &probe, &[ix], &[&probe_creator]),
        "honest create_market paid by a fee payer holding exactly the fee",
    );
    assert_eq!(env.lamports(&probe.pubkey()), 0);

    let (payer, tokens) = craft(&mut env);
    let creator = env.funded_keypair();
    let evil = evil_accounts(&mut env);
    let ix = create_ix(
        &creator.pubkey(),
        &evil,
        INIT_APPROVES_CREATOR_TOKENS,
        SOL / 10,
        approve_tail(&payer.pubkey(), &tokens, false),
    );
    expect_ok(
        send_paid_by(&mut env, &payer, &[ix], &[&creator]),
        "mode-7 create_market with the fee-drained payer as authority",
    );
    assert_eq!(
        delegate_of(&env, &tokens),
        Some(evil.state),
        "the fee-drained fee payer's signature was forwarded (documented caveat)"
    );
}

/// (d) The natural form of (a): the fee payer W funds a new creator keypair with a
/// "send max" transfer (balance minus the fee) in the same transaction, which leaves W at 0.
/// The strategy's account list names W (unmarked). W's signature is forwarded. No crafted
/// state. Accepted design for the same reason as (a), even though the client never marked W
/// as a signer of `create_market` (signer flags are transaction-wide; §8 covers this).
#[test]
fn h004_d_fee_payer_emptied_by_send_max_looks_fresh_accepted() {
    let mut env = registered_evil_env();
    let fee = 2 * FEE_PER_SIGNATURE;
    let send_max_tx = |env: &mut Env, mode: u8| -> (TxResult, Pubkey, Pubkey, Pubkey) {
        let (wallet, tokens) = wallet_with(env, SOL);
        let creator = Keypair::new();
        let evil = evil_accounts(env);
        let fund = system_transfer(&wallet.pubkey(), &creator.pubkey(), SOL - fee);
        let create = create_ix(
            &creator.pubkey(),
            &evil,
            mode,
            SOL / 10,
            approve_tail(&wallet.pubkey(), &tokens, false),
        );
        let result = send_paid_by(env, &wallet, &[fund, create], &[&creator]);
        (result, wallet.pubkey(), tokens, evil.state)
    };

    let (result, probe, _, _) = send_max_tx(&mut env, HONEST);
    expect_ok(result, "send-max funding + honest create_market");
    assert_eq!(env.lamports(&probe), 0, "send max left the fee payer at 0");

    let (result, _, tokens, state) = send_max_tx(&mut env, INIT_APPROVES_CREATOR_TOKENS);
    expect_ok(result, "send-max funding + mode-7 create_market");
    assert_eq!(
        delegate_of(&env, &tokens),
        Some(state),
        "the send-max fee payer's signature was forwarded (documented caveat)"
    );
}

// ------------------------- (b), (c): zero left by an earlier router instruction, pinned

/// (b) Two `create_market` instructions in one transaction. C1 holds exactly Market rent
/// plus the allowance its (honest) setup spends, so the router leaves C1 at 0 after ix1.
/// ix2 has a different creator and lists C1 as the mode-7 authority. Documented behaviour
/// (§8, "a wallet emptied by an earlier instruction in the same transaction"): C1 looks
/// fresh and its signature reaches ix2's strategy.
#[test]
fn h004_b_creator_of_earlier_create_market_is_forwarded_documented() {
    let mut env = registered_evil_env();
    let market_rent = rent(&env, MARKET_SPACE);
    let state_rent = rent(&env, STATE_LEN);
    let two_markets = |env: &mut Env, mode: u8| -> (TxResult, Pubkey, Pubkey, Pubkey) {
        let (c1, tokens) = wallet_with(env, market_rent + state_rent);
        let c2 = env.funded_keypair();
        let first = evil_accounts(env);
        let second = evil_accounts(env);
        let ix1 = create_ix(&c1.pubkey(), &first, HONEST, state_rent, vec![]);
        let ix2 = create_ix(
            &c2.pubkey(),
            &second,
            mode,
            SOL / 10,
            approve_tail(&c1.pubkey(), &tokens, true),
        );
        let result = env.send(&[ix1, ix2], &[&c1, &c2]);
        (result, c1.pubkey(), tokens, second.state)
    };

    // Setup check: the same transaction with an honest second setup succeeds and C1 ends
    // at exactly 0, so C1 held 0 lamports when ix2's freshness was judged.
    let (result, probe, _, _) = two_markets(&mut env, HONEST);
    expect_ok(result, "two honest create_market instructions");
    assert_eq!(env.lamports(&probe), 0, "ix1 left its creator at 0");

    let (result, _, tokens, state) = two_markets(&mut env, INIT_APPROVES_CREATOR_TOKENS);
    assert_forwarded(
        &env,
        result,
        &tokens,
        &state,
        "(b) earlier create_market creator",
    );
}

/// (b') As (b), but ix1's setup is hostile (`INIT_DRAINS_PAYER` moves 1 SOL of the
/// allowance to its state) and C1 granted an allowance covering its whole spare balance.
/// The strategy keeps the allowance (at most `max_creator_lamports`), and C1, now at 0, also
/// looks fresh in ix2, so its signature is forwarded (documented in §8).
#[test]
fn h004_b_creator_drained_by_hostile_setup_is_forwarded_documented() {
    let mut env = registered_evil_env();
    let market_rent = rent(&env, MARKET_SPACE);
    let state_rent = rent(&env, STATE_LEN);
    let allowance = state_rent + SOL;
    let two_markets = |env: &mut Env, mode: u8| -> (TxResult, Pubkey, Pubkey, Pubkey) {
        let (c1, tokens) = wallet_with(env, market_rent + allowance);
        let c2 = env.funded_keypair();
        let first = evil_accounts(env);
        let second = evil_accounts(env);
        let ix1 = create_ix(&c1.pubkey(), &first, INIT_DRAINS_PAYER, allowance, vec![]);
        let ix2 = create_ix(
            &c2.pubkey(),
            &second,
            mode,
            SOL / 10,
            approve_tail(&c1.pubkey(), &tokens, false),
        );
        let result = env.send(&[ix1, ix2], &[&c1, &c2]);
        (result, c1.pubkey(), tokens, second.state)
    };

    let (result, probe, _, _) = two_markets(&mut env, HONEST);
    expect_ok(result, "draining create_market + honest create_market");
    assert_eq!(env.lamports(&probe), 0, "the hostile setup left C1 at 0");

    let (result, _, tokens, state) = two_markets(&mut env, INIT_APPROVES_CREATOR_TOKENS);
    assert_forwarded(
        &env,
        result,
        &tokens,
        &state,
        "(b') creator drained by hostile setup",
    );
}

/// (c) `register_strategy` then `create_market` in one transaction. The registration payer
/// P holds exactly the listing fee plus the Strategy account's rent, so the router leaves P
/// at 0. The `create_market` (another creator, the already registered evil strategy) lists
/// P as the mode-7 authority. Documented behaviour (§8): P looks fresh and its signature
/// reaches the strategy.
#[test]
fn h004_c_register_strategy_payer_is_forwarded_documented() {
    let mut env = registered_evil_env();
    let strategy_rent = env.lamports(&pda::strategy(&EVIL_ID));
    assert!(strategy_rent > 0);
    let register_then_create = |env: &mut Env, mode: u8| -> (TxResult, Pubkey, Pubkey, Pubkey) {
        let program = Keypair::new().pubkey();
        let registrar = env.funded_keypair();
        env.deploy_strategy(&program, "test_strategy_evil", &registrar.pubkey());
        let (payer, tokens) = wallet_with(env, LISTING_FEE + strategy_rent);
        let creator = env.funded_keypair();
        let evil = evil_accounts(env);
        let register = client::register_strategy(
            &payer.pubkey(),
            &registrar.pubkey(),
            &program,
            &env.listing_fee_recipient,
            "https://example.com/new.json",
            LISTING_FEE,
        );
        let create = create_ix(
            &creator.pubkey(),
            &evil,
            mode,
            SOL / 10,
            approve_tail(&payer.pubkey(), &tokens, false),
        );
        let result = env.send(&[register, create], &[&payer, &registrar, &creator]);
        (result, payer.pubkey(), tokens, evil.state)
    };

    let (result, probe, _, _) = register_then_create(&mut env, HONEST);
    expect_ok(result, "register_strategy + honest create_market");
    assert_eq!(env.lamports(&probe), 0, "registration left its payer at 0");

    let (result, _, tokens, state) = register_then_create(&mut env, INIT_APPROVES_CREATOR_TOKENS);
    assert_forwarded(
        &env,
        result,
        &tokens,
        &state,
        "(c) earlier register_strategy payer",
    );
}
