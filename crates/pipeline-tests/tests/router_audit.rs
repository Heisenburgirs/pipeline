//! Router audit (2026-11-08): proof-of-concept tests, kept as regression tests.
//!
//! `hook_refill_underpays_the_pipeline_fee` uses the hostile hook in
//! `programs/test-hook-refill`.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_strategy_interface::fees;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
const REFILL_HOOK_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ4");

fn compute(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction::new_with_bytes(
        client::pubkey!("ComputeBudget111111111111111111111111111111"),
        &data,
        vec![],
    )
}

/// Documented limitation: a wallet with zero lamports looks exactly like a new keypair,
/// so if it signs `create_market` its signature is forwarded to the strategy, which can
/// then approve or move the tokens it owns. The router can't tell the two apart; callers
/// must sign `create_market` only with new keypairs (see `create_market` and the
/// strategy guide). This test pins the behaviour so a change to it is deliberate.
#[test]
fn zero_lamport_wallet_signature_reaches_strategy_setup() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let creator = env.funded_keypair();

    // The victim holds tokens but no SOL (its fees are paid by someone else).
    let victim = Keypair::new();
    let token_mint = env.create_mint(&TOKEN, 6, None, None);
    let victim_tokens = env.create_ata(&victim.pubkey(), &token_mint, &TOKEN);
    env.mint_to(&token_mint, &TOKEN, &victim_tokens, 1_000_000);
    assert_eq!(env.lamports(&victim.pubkey()), 0);

    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    let ix = client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
        strategy_program: EVIL_ID,
        market_state: state,
        market_state_signs: false,
        // INIT_APPROVES_CREATOR_TOKENS: approve account 8 to the state PDA with 9 signing.
        init_data: vec![7],
        max_creator_lamports: SOL,
        strategy_accounts: vec![
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
            AccountMeta::new(victim_tokens, false),
            AccountMeta::new_readonly(victim.pubkey(), true),
            AccountMeta::new_readonly(TOKEN, false),
        ],
    });
    let result = env.send(&[ix], &[&creator, &victim]);
    let data = env.account_data(&victim_tokens);
    let delegate_set = data[72] == 1;
    let delegate = Pubkey::new_from_array(data[76..108].try_into().unwrap());
    println!(
        "create_market ok: {}, delegate set: {delegate_set}, delegate == evil state: {}",
        result.is_ok(),
        delegate == state
    );
    assert!(result.is_ok() && delegate_set && delegate == state);
}

fn refill_validation(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], &REFILL_HOOK_ID).0
}

/// Finding: the router measures `net_sent` as the trader's input-account balance change
/// over the whole strategy call. Code that runs inside that window but is not the
/// strategy (here, the output mint's Token-2022 transfer hook, called by an honest,
/// router-only strategy when it pays the trader) can credit the trader's input account
/// from elsewhere. The strategy credits the full `amount_in` that reached its vault, but
/// the router charges 0.25% only on what is left after the refill.
///
/// Fixed: the router also measures what the market's input vault kept and charges on the
/// larger amount, so the fee is 0.25% of the input the pool kept.
#[test]
fn hook_refill_underpays_the_pipeline_fee() {
    let mut env = Env::new();
    env.svm
        .add_program_from_file(REFILL_HOOK_ID, sbf_dir().join("test_hook_refill.so"))
        .unwrap();

    // Input: a classic SPL token (like wSOL or USDC). Output: the issuer's Token-2022
    // token whose hook refills the buyer's input account from a stash.
    let mint_in = env.create_mint(&TOKEN, 6, None, None);
    let mint_out = env.create_mint(&TOKEN_2022, 6, None, Some(REFILL_HOOK_ID));

    let trader = env.funded_keypair();
    let trader_in = env.create_ata(&trader.pubkey(), &mint_in, &TOKEN);
    let trader_out = env.create_ata(&trader.pubkey(), &mint_out, &TOKEN_2022);
    let stash_authority = Pubkey::find_program_address(&[b"stash"], &REFILL_HOOK_ID).0;
    let stash = env.create_ata(&stash_authority, &mint_in, &TOKEN);

    // Hook extra accounts: stash (w), stash authority, refill target (w), SPL Token.
    let mut init_data = b"refilini".to_vec();
    for (key, writable) in [
        (stash, 1u8),
        (stash_authority, 0),
        (trader_in, 1),
        (TOKEN, 0),
    ] {
        init_data.extend_from_slice(key.as_ref());
        init_data.push(writable);
    }
    let init = Instruction::new_with_bytes(
        REFILL_HOOK_ID,
        &init_data,
        vec![
            AccountMeta::new(env.admin.pubkey(), true),
            AccountMeta::new(refill_validation(&mint_out), false),
            AccountMeta::new_readonly(mint_out, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    );
    env.send(&[init], &[]).expect("hook init");
    let hook_accounts = vec![
        AccountMeta::new_readonly(REFILL_HOOK_ID, false),
        AccountMeta::new_readonly(refill_validation(&mint_out), false),
        AccountMeta::new(stash, false),
        AccountMeta::new_readonly(stash_authority, false),
        AccountMeta::new(trader_in, false),
        AccountMeta::new_readonly(TOKEN, false),
    ];

    // An honest, router-only constant-product pool (the reference strategy).
    let market = setup_cp_market_with_mints(
        &mut env,
        mint_in,
        TOKEN,
        mint_out,
        TOKEN_2022,
        1_000_000_000_000,
        1_000_000_000_000,
        30,
        hook_accounts.clone(),
    );

    let amount_in: u64 = 1_000_000_000;
    let refill = amount_in - 1;
    env.mint_to(
        &mint_in,
        &TOKEN,
        &trader_in,
        amount_in + refill + 10_000_000,
    );
    // The trader parks `refill` of their own input in the hook's stash beforehand.
    let park = spl_token_interface::instruction::transfer(
        &TOKEN,
        &trader_in,
        &stash,
        &trader.pubkey(),
        &[],
        refill,
    )
    .unwrap();
    env.send(&[park], &[&trader]).expect("park");

    let vault_in_before = env.token_balance(&market.vault(&mint_in));
    let fee_account = pda::associated_token(&pda::fee_authority(&market.market), &mint_in, &TOKEN);
    let fee_before = env.token_balance(&fee_account);
    let wealth_before = env.token_balance(&trader_in) + env.token_balance(&stash);

    let mut accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &mint_in,
        &trader_in,
        &mint_out,
        &trader_out,
    );
    accounts.strategy_accounts.extend(hook_accounts);
    let meta = env
        .send(
            &[
                compute(400_000),
                client::swap(&accounts, &exact_in(amount_in, 1)),
            ],
            &[&trader],
        )
        .expect("swap");

    let pool_kept = env.token_balance(&market.vault(&mint_in)) - vault_in_before;
    let fee_paid = env.token_balance(&fee_account) - fee_before;
    let wealth_after = env.token_balance(&trader_in) + env.token_balance(&stash);
    let event = swap_events(&meta).pop().unwrap();
    println!(
        "pool kept {pool_kept}, trader's input wealth fell by {}, output {}, \
         event net_sent {} received {} pipeline_fee {}, fee account got {fee_paid}, \
         honest fee {}",
        wealth_before - wealth_after,
        env.token_balance(&trader_out),
        event.net_sent,
        event.received,
        event.pipeline_fee,
        fees::pipeline_fee(pool_kept),
    );
    assert_eq!(
        fee_paid,
        fees::pipeline_fee(pool_kept),
        "BUG: Pipeline charged {fee_paid} on a trade where the pool kept {pool_kept}"
    );
}
