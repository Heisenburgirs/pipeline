//! SOS2 H076 — Token-2022 transfer-fee input mint: Pipeline's fee leg and sweep leg.
//!
//! The input mint's issuer sets a transfer fee of 100% capped at CAP. Pipeline's fee is
//! moved trader -> fee ATA (taxed by the mint) and later fee ATA -> treasury (taxed again).
//! Properties checked:
//!   * fee leg: when the fee is at most CAP, the fee ATA's balance is unchanged and the
//!     whole fee sits as withheld tokens (only the mint's issuer can take them);
//!     `SwapEvent.pipeline_fee` still reports the full fee; a sweep then moves nothing.
//!   * sweep leg: when the fee exceeds CAP, the sweep taxes the treasury's receipt again,
//!     so the treasury gets fee - CAP (fee leg) - CAP (sweep leg) at most.
//!
//! Both legs are the mint's own transfer fee, so this is an issuer power over its own
//! mint's revenue (accepted by design); the test documents the amounts.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Pubkey};
use pipeline_strategy_interface as psi;
use solana_signer::Signer;
use spl_token_2022_interface::extension::transfer_fee::{TransferFeeAmount, TransferFeeConfig};
use spl_token_2022_interface::extension::{BaseStateWithExtensions, StateWithExtensions};

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

/// Issuer takes the whole transfer, up to CAP per transfer.
const BPS: u16 = 10_000;
const CAP: u64 = 50_000;

struct World {
    env: Env,
    market: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
    fee_account: Pubkey,
    treasury: Pubkey,
}

fn world() -> World {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let mint_a = env.create_mint(&TOKEN_2022, 6, Some((BPS, CAP)), None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN_2022);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    let creator = env.funded_keypair();
    let ix = client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
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
    env.send(&[ix], &[&creator]).expect("create evil market");
    let market = pda::market(&EVIL_ID, &state);
    let payer = env.admin.pubkey();
    for (mint, program) in [(&mint_a, &TOKEN_2022), (&mint_b, &TOKEN)] {
        let ix = client::init_fee_account(&payer, &market, mint, program);
        env.send(&[ix], &[]).expect("init fee account");
    }
    env.mint_to(&mint_b, &TOKEN, &vault_b, 1_000_000_000);
    let treasury_owner = env.treasury_owner;
    let treasury = env.create_ata(&treasury_owner, &mint_a, &TOKEN_2022);
    let fee_account = pda::fee_account(&market, &mint_a, &TOKEN_2022);
    World {
        env,
        market: state,
        mint_a,
        mint_b,
        vault_a,
        vault_b,
        fee_account,
        treasury,
    }
}

/// Withheld transfer fees on a Token-2022 account (0 without the extension).
fn withheld(env: &Env, account: &Pubkey) -> u64 {
    let data = env.account_data(account);
    if data.len() <= 165 {
        return 0;
    }
    let state = StateWithExtensions::<spl_token_2022_interface::state::Account>::unpack(&data)
        .expect("token-2022 account");
    state
        .get_extension::<TransferFeeAmount>()
        .map(|fee| u64::from(fee.withheld_amount))
        .unwrap_or(0)
}

/// The mint's transfer fee on `amount` this epoch, per Token-2022 itself.
fn mint_transfer_fee(env: &Env, mint: &Pubkey, amount: u64) -> u64 {
    let data = env.account_data(mint);
    let state = StateWithExtensions::<spl_token_2022_interface::state::Mint>::unpack(&data)
        .expect("token-2022 mint");
    let epoch = env.svm.get_sysvar::<solana_clock::Clock>().epoch;
    state
        .get_extension::<TransferFeeConfig>()
        .map(|config| config.calculate_epoch_fee(epoch, amount).unwrap())
        .unwrap_or(0)
}

/// One honest exact-in swap A -> B of `amount`, returns the transaction metadata.
fn swap_a_to_b(w: &mut World, amount: u64) -> litesvm::types::TransactionMetadata {
    let (trader, trader_a) = w.env.trader_with(&w.mint_a, &TOKEN_2022, amount * 2);
    let trader_b = w.env.create_ata(&trader.pubkey(), &w.mint_b, &TOKEN);
    let accounts = client::SwapAccounts {
        trader: trader.pubkey(),
        strategy_program: EVIL_ID,
        market_state: w.market,
        trader_input: trader_a,
        trader_output: trader_b,
        input_vault: w.vault_a,
        input_mint: w.mint_a,
        output_mint: w.mint_b,
        input_token_program: TOKEN_2022,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts: vec![AccountMeta::new(w.vault_b, false)],
        input_hook_accounts: vec![],
    };
    w.env
        .send(&[client::swap(&accounts, &exact_in(amount, 1))], &[&trader])
        .expect("honest swap")
}

fn swept_count(meta: &litesvm::types::TransactionMetadata) -> usize {
    events(meta)
        .into_iter()
        .filter(|e| matches!(e, pipeline_events::PipelineEvent::FeesSwept(_)))
        .count()
}

fn sweep(w: &mut World) -> litesvm::types::TransactionMetadata {
    let treasury_owner = w.env.treasury_owner;
    let ix = client::sweep_fees(
        &pda::market(&EVIL_ID, &w.market),
        &w.mint_a,
        &TOKEN_2022,
        &treasury_owner,
        vec![],
    );
    w.env.send(&[ix], &[]).expect("sweep")
}

/// Fee at most CAP: the mint takes all of it on the fee leg, so the fee ATA holds nothing
/// spendable, the event still reports the fee, and the sweep is a no-op.
#[test]
fn h076_fee_leg_is_withheld_to_the_issuer_and_sweep_sees_nothing() {
    let mut w = world();
    let amount = 1_000_000u64;
    let fee = psi::fees::pipeline_fee(amount);
    assert!(fee > 0 && fee <= CAP, "setup: fee within the cap");
    let meta = swap_a_to_b(&mut w, amount);

    assert_eq!(
        swap_events(&meta)[0].pipeline_fee,
        fee,
        "event reports the full fee"
    );
    assert_eq!(
        mint_transfer_fee(&w.env, &w.mint_a, fee),
        fee,
        "setup: the mint takes all of the fee"
    );
    assert_eq!(
        withheld(&w.env, &w.fee_account),
        fee,
        "fee sits as withheld"
    );
    assert_eq!(
        w.env.token_balance(&w.fee_account),
        0,
        "fee ATA amount unchanged"
    );

    let treasury_before = w.env.token_balance(&w.treasury);
    let sweep_meta = sweep(&mut w);
    assert_eq!(
        swept_count(&sweep_meta),
        0,
        "sweep sees 0 and emits no event"
    );
    assert_eq!(w.env.token_balance(&w.treasury), treasury_before);
}

/// Fee above CAP: the fee leg withholds CAP, the fee ATA receives fee - CAP, and the sweep
/// withholds CAP again on the treasury leg. Pipeline's treasury gets fee - 2 * CAP at most.
#[test]
fn h076_sweep_taxes_the_treasury_leg_again() {
    let mut w = world();
    let amount = 100_000_000u64;
    let fee = psi::fees::pipeline_fee(amount);
    assert!(fee > CAP, "setup: fee above the cap");
    let meta = swap_a_to_b(&mut w, amount);
    assert_eq!(swap_events(&meta)[0].pipeline_fee, fee);

    let fee_ata_amount = w.env.token_balance(&w.fee_account);
    assert_eq!(
        withheld(&w.env, &w.fee_account),
        CAP,
        "fee leg withholds CAP"
    );
    assert_eq!(fee_ata_amount, fee - CAP, "fee ATA gets fee - CAP");

    let treasury_before = w.env.token_balance(&w.treasury);
    let treasury_withheld_before = withheld(&w.env, &w.treasury);
    let sweep_meta = sweep(&mut w);
    assert_eq!(swept_events_amount(&sweep_meta), fee_ata_amount);
    let received = w.env.token_balance(&w.treasury) - treasury_before;
    let kept_back = withheld(&w.env, &w.treasury) - treasury_withheld_before;
    assert_eq!(
        kept_back,
        CAP.min(fee_ata_amount),
        "sweep leg withholds again"
    );
    assert_eq!(received, fee_ata_amount - kept_back);
    assert_eq!(
        received,
        fee - 2 * CAP,
        "treasury receives fee minus one cap per leg"
    );
}

fn swept_events_amount(meta: &litesvm::types::TransactionMetadata) -> u64 {
    events(meta)
        .into_iter()
        .find_map(|e| match e {
            pipeline_events::PipelineEvent::FeesSwept(s) => Some(s.amount),
            _ => None,
        })
        .expect("a FeesSwept event")
}
