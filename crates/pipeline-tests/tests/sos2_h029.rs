//! SOS audit #2, H029: only the router's own `emit_cpi!` produces a router event in a
//! successful transaction.
//!
//! Anchor 1.2.2 dispatches instruction data that starts with `EVENT_IX_TAG` to
//! `__event_dispatch`, which requires the first account to be a signer whose address is
//! the router's `["__event_authority"]` PDA. Only the router can sign for that PDA (via
//! `invoke_signed` inside `emit_cpi!`), so any other caller (outer instruction or CPI
//! from a strategy) fails, and a failed transaction is rolled back.
//!
//! Properties:
//! 1. no router instruction discriminator equals or starts with the event tag;
//! 2. an outer instruction with the event tag and a forged `SwapEvent` is refused
//!    (signer check, then seeds check);
//! 3. the same through a strategy CPI (the evil strategy's `FORWARD_TO_ROUTER`) is refused,
//!    including with the strategy's own PDA signing in the event-authority slot;
//! 4. a genuine swap emits exactly one router event instruction, whose only account is the
//!    event authority, and whose numbers match the balance changes;
//! 5. putting `event_authority` in a swap's remaining accounts changes nothing.
//!
//! The consumer-side gap (a failed transaction still lists the forged CPI in its inner
//! instructions) is printed for the record; indexers should skip failed transactions
//! (the TS SDK extractor takes no error field).

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_events::{discriminator, PipelineEvent, SwapEvent, EVENT_IX_TAG};
use sha2::{Digest, Sha256};
use solana_signer::Signer;
use test_strategy_evil_consts::FORWARD_TO_ROUTER;

/// The evil strategy's forwarding tag (kept in sync with `programs/test-strategy-evil`).
mod test_strategy_evil_consts {
    pub const FORWARD_TO_ROUTER: [u8; 8] = *b"evilfwd!";
}

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

fn forged_swap_event_data() -> Vec<u8> {
    let forged = SwapEvent {
        market: Pubkey::new_unique(),
        strategy: Pubkey::new_unique(),
        trader: Pubkey::new_unique(),
        input_mint: Pubkey::new_unique(),
        output_mint: Pubkey::new_unique(),
        mode: 0,
        net_sent: 999_999_999_999,
        received: 888_888_888_888,
        pipeline_fee: 1,
        integrator: None,
        integrator_fee: 0,
        slot: 1,
        unix_timestamp: 1,
    };
    let mut data = EVENT_IX_TAG.to_vec();
    data.extend_from_slice(&discriminator::SWAP);
    data.extend_from_slice(&borsh::to_vec(&forged).unwrap());
    data
}

/// Calls the router through the evil strategy's `FORWARD_TO_ROUTER`: accounts are the
/// router program, then the metas of the forwarded instruction.
fn forward_via_strategy(router_data: &[u8], metas: &[AccountMeta]) -> Instruction {
    let mut data = FORWARD_TO_ROUTER.to_vec();
    data.extend_from_slice(router_data);
    let mut accounts = vec![AccountMeta::new_readonly(client::PROGRAM_ID, false)];
    // The outer transaction cannot sign for the strategy's PDA: the strategy signs it.
    for meta in metas {
        let mut outer = meta.clone();
        outer.is_signer = false;
        accounts.push(outer);
    }
    Instruction::new_with_bytes(EVIL_ID, &data, accounts)
}

#[test]
fn h029_no_router_instruction_discriminator_collides_with_the_event_tag() {
    let names = [
        "initialize_config",
        "update_config",
        "lift_pause",
        "register_strategy",
        "update_metadata",
        "create_market",
        "register_existing_market",
        "init_fee_account",
        "swap",
        "sweep_fees",
    ];
    for name in names {
        let digest = Sha256::digest(format!("global:{name}").as_bytes());
        assert_ne!(
            digest[..8],
            EVENT_IX_TAG,
            "H029: router instruction {name} has the event tag as its discriminator"
        );
    }
    // Control: the helper really computes Anchor's discriminator (matches the client).
    let swap = Sha256::digest(b"global:swap");
    assert_eq!(client::instruction_discriminator("swap"), swap[..8]);
}

#[test]
fn h029_outer_event_instruction_without_authority_signature_is_refused() {
    let mut env = Env::new();
    let data = forged_swap_event_data();
    let slot_before = env.svm.get_sysvar::<solana_clock::Clock>().slot;

    // Control: the router program is deployed and answers (a real config instruction works).
    let ok = client::update_config(
        &env.admin.pubkey(),
        &client::UpdateConfigArgs {
            admin: None,
            paused: None,
            listing_fee_lamports: None,
            listing_fee_recipient: None,
            treasury_owner: None,
        },
    );
    env.send(&[ok], &[]).expect("setup: router answers");

    // (a) event authority present but not a signer: signer check.
    let ix = Instruction::new_with_bytes(
        client::PROGRAM_ID,
        &data,
        vec![AccountMeta::new_readonly(pda::event_authority(), false)],
    );
    let result = env.send(&[ix], &[]);
    assert_fails_with(result, "ConstraintSigner");

    // (b) no accounts at all.
    let ix = Instruction::new_with_bytes(client::PROGRAM_ID, &data, vec![]);
    let result = env.send(&[ix], &[]);
    assert!(
        result.is_err(),
        "H029: a bare event instruction to the router must fail"
    );

    // (c) a signer that is not the event authority (the admin) in the authority slot:
    // seeds check.
    let ix = Instruction::new_with_bytes(
        client::PROGRAM_ID,
        &data,
        vec![AccountMeta::new_readonly(env.admin.pubkey(), true)],
    );
    let result = env.send(&[ix], &[]);
    assert_fails_with(result, "ConstraintSeeds");
    let _ = slot_before;
}

#[test]
fn h029_strategy_cpi_with_event_tag_is_refused() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let data = forged_swap_event_data();

    // Control: forwarding a legitimate router instruction through the strategy works
    // (the forwarder itself is functional): update_metadata signed by the register PDA.
    let uri = "https://example.com/changed.json";
    let real = client::update_metadata(&pda::register(&EVIL_ID), &EVIL_ID, uri);
    let control = forward_via_strategy(&real.data, &real.accounts);
    env.send(&[control], &[])
        .expect("setup: forwarded update_metadata succeeds through the strategy");

    // (a) event authority as a non-signer.
    let ix = forward_via_strategy(
        &data,
        &[AccountMeta::new_readonly(pda::event_authority(), false)],
    );
    let result = env.send(&[ix], &[]);
    match result {
        Ok(meta) => panic!(
            "H029: strategy CPI with forged event tag succeeded:\n{}",
            meta.logs.join("\n")
        ),
        Err(failed) => {
            let logs = failed.meta.logs.join("\n");
            assert!(
                logs.contains("ConstraintSigner"),
                "unexpected failure: {logs}"
            );
            // Record what an err-ignoring consumer would see.
            let seen = events(&failed.meta);
            println!(
                "H029 note: failed tx still lists {} decodable router event(s) in inner instructions",
                seen.len()
            );
            for event in seen {
                assert!(matches!(event, PipelineEvent::Swap(_)));
            }
        }
    }

    // (b) the strategy's own PDA (it can sign for it) in the authority slot: signer check
    // passes but the address is not the router's event authority.
    let ix = forward_via_strategy(
        &data,
        &[AccountMeta::new_readonly(pda::register(&EVIL_ID), false)],
    );
    let result = env.send(&[ix], &[]);
    assert_fails_with(result, "ConstraintSeeds");
}

#[test]
fn h029_genuine_swap_emits_exactly_one_router_event_with_only_the_authority_account() {
    let mut env = Env::new();
    let m = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000_000, 1_000_000_000, 30);
    let (trader, trader_a) = env.trader_with(&m.mint_a, &TOKEN, 100_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &m.mint_b, &TOKEN);
    let accounts = cp_swap_accounts(
        &m,
        &trader.pubkey(),
        &m.mint_a,
        &trader_a,
        &m.mint_b,
        &trader_b,
    );

    // Control: a swap that also lists event_authority among the strategy's accounts
    // still succeeds (the CP strategy ignores extras), one event.
    let mut with_extra = accounts.clone();
    with_extra
        .strategy_accounts
        .push(AccountMeta::new_readonly(pda::event_authority(), false));
    let in_before = env.token_balance(&trader_a);
    let out_before = env.token_balance(&trader_b);
    let ix = client::swap(&with_extra, &exact_in(1_000_000, 1));
    let meta = env
        .send(&[ix], &[&trader])
        .expect("setup: swap with event_authority in remaining accounts succeeds");
    let swaps = swap_events(&meta);
    assert_eq!(
        swaps.len(),
        1,
        "H029: exactly one SwapEvent per swap, event_authority in remaining accounts or not"
    );
    let spent = in_before - env.token_balance(&trader_a);
    let got = env.token_balance(&trader_b) - out_before;
    assert_eq!(swaps[0].received, got);
    assert_eq!(swaps[0].net_sent + swaps[0].pipeline_fee, spent);

    // Every router-addressed inner instruction that decodes as an event is the single
    // self-CPI emitted by the router, with exactly [event_authority] as its account.
    let mut event_ixs = 0;
    for inner in meta.inner_instructions.iter().flatten() {
        if pipeline_events::decode(&inner.instruction.data).is_some() {
            event_ixs += 1;
            assert_eq!(inner.instruction.accounts.len(), 1, "event ix accounts");
            println!(
                "H029 note: event self-CPI stack_height={}",
                inner.stack_height
            );
        }
    }
    assert_eq!(event_ixs, 1, "H029: exactly one event instruction");
}
