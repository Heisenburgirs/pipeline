//! SOS audit #2, H010: the fee base when input leaves the declared vault during the call
//! and the trader's input account is refilled from outside.
//!
//! The router charges `min(amount_in, max(trader_spent, vault_kept))`, measured on the
//! trader's input account and on the one vault the market declared for the input mint.
//! A refill of the trader's input account from outside the strategy (an output-mint
//! Token-2022 hook, or lamports plus `SyncNative` on wSOL) lowers `trader_spent`; input
//! that the strategy moves out of its declared vault during the call lowers
//! `vault_kept`. Either alone leaves the other term at `amount_in`.
//!
//! No prebuilt fixture can do both in one swap: the evil strategy never moves input
//! anywhere but back to the trader, and the refill hook makes one transfer per firing.
//! So each half is run separately against the honest CP strategy and an SPL input mint
//! with a refill-hooked Token-2022 output mint:
//!
//! - refill only: the hook empties a stash of the trader's own input tokens into the
//!   trader's input account; the fee base is `vault_kept = amount_in`;
//! - forwarding only: the CP input vault's delegate is set (crafted with `set_account`)
//!   to the hook's stash authority, and the hook empties the vault into a side account
//!   during the output payment, standing in for a strategy that forwards its input; the
//!   fee base is `trader_spent = amount_in`.
//!
//! Both tests assert the safe behaviour and pass. They also print what the router would
//! charge if both happened in the same swap (`settle` on the combined balances), which is
//! the H010 limitation: `amount_in - min(refill, forwarded)`.

#![allow(clippy::result_large_err)]

mod bok_swap_util;
mod common;

use bok_swap_util::*;
use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_strategy_interface::{self as psi, fees};
use solana_signer::Signer;

const RESERVE: u64 = 1_000_000_000;
const AMOUNT: u64 = 1_000_000;
const REFILL: u64 = 900_000;
const POOL_FEE_BPS: u16 = 30;

fn stash_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"stash"], &REFILL_HOOK_ID).0
}

/// A refill-hooked Token-2022 mint whose hook empties `stash` (any SPL account the
/// hook's stash PDA may move, as owner or delegate) into `target` on every transfer.
fn refill_mint(env: &mut Env, stash_of: impl FnOnce(&Pubkey) -> (Pubkey, Pubkey)) -> RefillHook {
    if env.svm.get_account(&REFILL_HOOK_ID).is_none() {
        env.svm
            .add_program_from_file(REFILL_HOOK_ID, sbf_dir().join("test_hook_refill.so"))
            .unwrap();
    }
    let mint = env.create_mint(&TOKEN_2022, 6, None, Some(REFILL_HOOK_ID));
    let (stash, target) = stash_of(&mint);
    let validation =
        Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], &REFILL_HOOK_ID).0;
    let mut init_data = b"refilini".to_vec();
    for (key, writable) in [
        (stash, 1u8),
        (stash_authority(), 0),
        (target, 1),
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
            AccountMeta::new(validation, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    );
    env.send(&[init], &[]).expect("refill hook init");
    RefillHook {
        mint,
        stash,
        accounts: vec![
            AccountMeta::new_readonly(REFILL_HOOK_ID, false),
            AccountMeta::new_readonly(validation, false),
            AccountMeta::new(stash, false),
            AccountMeta::new_readonly(stash_authority(), false),
            AccountMeta::new(target, false),
            AccountMeta::new_readonly(TOKEN, false),
        ],
    }
}

/// Crafted state: makes `delegate` the SPL token account's delegate for `u64::MAX`.
fn set_delegate(env: &mut Env, account: &Pubkey, delegate: &Pubkey) {
    let mut acc = env.svm.get_account(account).expect("token account");
    assert_eq!(acc.owner, TOKEN, "setup: an SPL Token account");
    acc.data[72..76].copy_from_slice(&1u32.to_le_bytes());
    acc.data[76..108].copy_from_slice(delegate.as_ref());
    acc.data[121..129].copy_from_slice(&u64::MAX.to_le_bytes());
    env.svm.set_account(*account, acc).unwrap();
}

/// What `settle` would charge if the refill and the forwarding had happened in the same
/// swap: the trader side from the refill run, the vault side from the forwarding run.
fn combined_net_sent(
    input_before: u64,
    trader_spent: u64,
    vault_before: u64,
    vault_after: u64,
) -> u64 {
    let measured = psi::settlement::Measured {
        input_before,
        input_after: input_before - trader_spent,
        output_before: 0,
        output_after: 1,
        vault_before,
        vault_after,
    };
    psi::settlement::settle(&measured, AMOUNT, 0, 0)
        .expect("settles")
        .net_sent
}

/// Refill only. SPL input, refill-hooked Token-2022 output; the hook empties `REFILL` of
/// the trader's own input tokens (parked in the hook's stash) into the trader's input
/// account during the strategy's output payment. The vault keeps `AMOUNT`, so the
/// router still charges 25 bps of `AMOUNT`.
#[test]
fn h010_refill_alone_is_charged_on_what_the_vault_kept() {
    let mut env = Env::new();
    let mint_in = env.create_mint(&TOKEN, 6, None, None);
    let (trader, trader_in) = env.trader_with(&mint_in, &TOKEN, 100 * AMOUNT);
    let hook = refill_mint(&mut env, |_| {
        (
            pda::associated_token(&stash_authority(), &mint_in, &TOKEN),
            trader_in,
        )
    });
    env.create_ata(&stash_authority(), &mint_in, &TOKEN);
    let m = setup_cp_market_with_mints(
        &mut env,
        mint_in,
        TOKEN,
        hook.mint,
        TOKEN_2022,
        RESERVE,
        RESERVE,
        POOL_FEE_BPS,
        hook.accounts.clone(),
    );
    let trader_out = env.create_ata(&trader.pubkey(), &hook.mint, &TOKEN_2022);
    // The trader parks part of their own input in the stash before the swap.
    let park = spl_token_interface::instruction::transfer(
        &TOKEN,
        &trader_in,
        &hook.stash,
        &trader.pubkey(),
        &[],
        REFILL,
    )
    .unwrap();
    env.send(&[park], &[&trader])
        .expect("setup: park the refill");
    assert_eq!(
        env.token_balance(&hook.stash),
        REFILL,
        "setup: stash funded"
    );

    let mut accounts = cp_swap_accounts(
        &m,
        &trader.pubkey(),
        &mint_in,
        &trader_in,
        &hook.mint,
        &trader_out,
    );
    accounts.strategy_accounts.extend(hook.accounts.clone());
    let fee_account = pda::fee_account(&m.market, &mint_in, &TOKEN);
    let watched = [trader_in, m.vault_a, fee_account, hook.stash];
    let before = balances(&env, &watched);
    let meta = send_swap(
        &mut env,
        client::swap(&accounts, &exact_in(AMOUNT, 1)),
        &trader,
    )
    .unwrap_or_else(|e| panic!("setup: swap must succeed: {:?}", e.err));
    let after = balances(&env, &watched);
    let (r, _) = reported(&meta);

    assert_eq!(
        after[3], 0,
        "setup: the hook emptied the stash during the call"
    );
    let trader_spent = AMOUNT - REFILL;
    assert_eq!(
        before[0] - after[0],
        trader_spent + r.pipeline_fee,
        "the trader's input account fell by AMOUNT - REFILL + fee"
    );
    let vault_kept = after[1] - before[1];
    assert_eq!(vault_kept, AMOUNT, "the declared vault kept AMOUNT");
    println!(
        "[refill only] trader_spent {trader_spent}, vault_kept {vault_kept}, net_sent {}, \
         pipeline_fee {}",
        r.net_sent, r.pipeline_fee
    );
    assert_eq!(
        r.net_sent, AMOUNT,
        "H010: a refill alone must not lower the fee base"
    );
    assert_eq!(r.pipeline_fee, fees::pipeline_fee(AMOUNT), "H010");
    assert_eq!(after[2] - before[2], fees::pipeline_fee(AMOUNT), "H010");
}

/// Forwarding only. Same market shape, but the CP input vault's delegate is the hook's
/// stash authority (crafted), and the hook empties the vault (reserve plus this swap's
/// input) into a side account during the output payment, as a strategy that forwards
/// its input would. The vault ends below where it started, so `vault_kept = 0`; the
/// trader really spent `AMOUNT`, so the router still charges 25 bps of `AMOUNT`.
///
/// The hooked mint is the market's mint A here, so the CP deposit (which moves A first,
/// while the SPL vault is still empty) does not trip the hook.
#[test]
fn h010_forwarding_alone_is_charged_on_the_trader_spend() {
    let mut env = Env::new();
    let mint_in = env.create_mint(&TOKEN, 6, None, None);
    let (trader, trader_in) = env.trader_with(&mint_in, &TOKEN, 100 * AMOUNT);
    let side = env.create_ata(&Pubkey::new_unique(), &mint_in, &TOKEN);
    let hook = refill_mint(&mut env, |hook_mint| {
        let pool = cp_pool(hook_mint, &mint_in, POOL_FEE_BPS);
        (pda::associated_token(&pool, &mint_in, &TOKEN), side)
    });
    let m = setup_cp_market_with_mints(
        &mut env,
        hook.mint,
        TOKEN_2022,
        mint_in,
        TOKEN,
        RESERVE,
        RESERVE,
        POOL_FEE_BPS,
        hook.accounts.clone(),
    );
    assert_eq!(
        m.vault_b, hook.stash,
        "setup: the hook's stash is the input vault"
    );
    assert_eq!(
        env.token_balance(&m.vault_b),
        RESERVE,
        "setup: vault funded"
    );
    assert_eq!(env.token_balance(&side), 0, "setup: side account empty");
    set_delegate(&mut env, &m.vault_b, &stash_authority());

    let trader_out = env.create_ata(&trader.pubkey(), &hook.mint, &TOKEN_2022);
    let mut accounts = cp_swap_accounts(
        &m,
        &trader.pubkey(),
        &mint_in,
        &trader_in,
        &hook.mint,
        &trader_out,
    );
    accounts.strategy_accounts.extend(hook.accounts.clone());
    let fee_account = pda::fee_account(&m.market, &mint_in, &TOKEN);
    let watched = [trader_in, m.vault_b, fee_account, side];
    let before = balances(&env, &watched);
    let meta = send_swap(
        &mut env,
        client::swap(&accounts, &exact_in(AMOUNT, 1)),
        &trader,
    )
    .unwrap_or_else(|e| panic!("setup: swap must succeed: {:?}", e.err));
    let after = balances(&env, &watched);
    let (r, _) = reported(&meta);

    assert_eq!(after[1], 0, "setup: the hook forwarded the whole vault");
    assert_eq!(
        after[3] - before[3],
        RESERVE + AMOUNT,
        "setup: the side account received the reserve plus this swap's input"
    );
    let trader_spent = before[0] - after[0] - r.pipeline_fee;
    assert_eq!(trader_spent, AMOUNT, "the trader really spent AMOUNT");
    println!(
        "[forwarding only] trader_spent {trader_spent}, vault {} -> {} (vault_kept 0), \
         net_sent {}, pipeline_fee {}",
        before[1], after[1], r.net_sent, r.pipeline_fee
    );
    assert_eq!(
        r.net_sent, AMOUNT,
        "H010: forwarding alone must not lower the fee base"
    );
    assert_eq!(r.pipeline_fee, fees::pipeline_fee(AMOUNT), "H010");
    assert_eq!(after[2] - before[2], fees::pipeline_fee(AMOUNT), "H010");

    // Not run on chain (no fixture does both in one swap): the same vault movement plus
    // the refill from the other test. A strategy that forwards only a fee F instead of
    // its whole input caps the shortfall at min(REFILL, F).
    let combined = combined_net_sent(before[0], AMOUNT - REFILL, before[1], after[1]);
    println!(
        "[forwarding + refill, settle on combined balances] net_sent {combined} \
         (pipeline_fee {}) instead of {AMOUNT} (pipeline_fee {})",
        fees::pipeline_fee(combined),
        fees::pipeline_fee(AMOUNT)
    );
}
