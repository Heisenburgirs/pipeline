//! BOK runtime evidence for the router's token-account checks in `swap`
//! (`token::checked_balance`): INV-ADM-11 (LiteSVM part, `08-parsing.md`) and
//! INV-SWAP-17 / INV-ADM-12 (`04-swap-handler.md`).
//!
//! Crafted accounts are injected with `svm.set_account`. Each crafted variant is derived
//! from a real, working account, and an unmodified copy is shown to swap successfully,
//! so a rejection is caused by the one property the variant breaks.

#![allow(clippy::result_large_err)]

mod bok_swap_util;
mod common;

use bok_swap_util::*;
use common::*;
use pipeline_client::{self as client, Pubkey};
use solana_account::Account;
use solana_signer::Signer;

const RESERVE: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Slot {
    TraderInput,
    TraderOutput,
    InputVault,
}

/// Variants of a real token account `base` (owned by `token_program`, in `mint`).
/// Every one of them must be refused by `checked_balance`.
fn bad_variants(
    base: &Account,
    token_program: &Pubkey,
    other_mint: &Pubkey,
    mint_account: &Account,
) -> Vec<(&'static str, Account)> {
    let mut variants = vec![];
    let other_program = if *token_program == TOKEN {
        TOKEN_2022
    } else {
        TOKEN
    };
    let mut a = base.clone();
    a.owner = client::SYSTEM_PROGRAM_ID;
    variants.push(("owned by the system program", a));
    let mut a = base.clone();
    a.owner = other_program;
    variants.push(("owned by the other token program", a));
    let mut a = base.clone();
    a.owner = Pubkey::new_unique();
    variants.push(("owned by an arbitrary program", a));
    let mut a = base.clone();
    a.data[..32].copy_from_slice(other_mint.as_ref());
    variants.push(("wrong mint", a));
    variants.push(("the mint account itself", mint_account.clone()));
    let mut a = base.clone();
    a.data.resize(355, 0);
    a.data[165] = 2;
    variants.push(("355 bytes (multisig length), byte 165 = 2", a));
    let mut a = base.clone();
    a.data.resize(355, 0);
    a.data[165] = 0;
    variants.push(("355 bytes (multisig length), byte 165 = 0", a));
    let mut a = base.clone();
    a.data.truncate(164);
    variants.push(("164 bytes (short)", a));
    let mut a = base.clone();
    a.data.truncate(72);
    variants.push(("72 bytes (short, amount still readable)", a));
    let mut a = base.clone();
    a.data.clear();
    variants.push(("empty", a));
    let mut a = base.clone();
    a.data.resize(170, 0);
    a.data[165] = 1;
    variants.push(("170 bytes, byte 165 = 1 (Token-2022 mint type)", a));
    let mut a = base.clone();
    a.data.resize(170, 0);
    a.data[165] = 0;
    variants.push(("170 bytes, byte 165 = 0 (uninitialised type)", a));
    // Keep every variant rent-exempt, so a refusal can only come from the router.
    for (_, account) in &mut variants {
        account.lamports = account.lamports.max(SOL);
    }
    variants
}

struct World {
    env: Env,
    m: CpMarket,
    trader: solana_keypair::Keypair,
    trader_a: Pubkey,
    trader_b: Pubkey,
    other_mint: Pubkey,
}

fn world(tp_a: Pubkey, tp_b: Pubkey) -> World {
    let mut env = Env::new();
    let m = setup_cp_market(&mut env, tp_a, tp_b, RESERVE, RESERVE, 30);
    let (trader, trader_a) = env.trader_with(&m.mint_a, &tp_a, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &m.mint_b, &tp_b);
    let other_mint = env.create_mint(&tp_a, 6, None, None);
    World {
        env,
        m,
        trader,
        trader_a,
        trader_b,
        other_mint,
    }
}

/// Swap A → B with the account in `slot` replaced by `replacement` (the input vault is
/// overwritten in place, since its address is pinned by the market).
fn swap_with(w: &mut World, slot: Slot, replacement: Option<Account>) -> TxResult {
    let mut accounts = cp_swap_accounts(
        &w.m,
        &w.trader.pubkey(),
        &w.m.mint_a,
        &w.trader_a,
        &w.m.mint_b,
        &w.trader_b,
    );
    let saved_vault = w.env.svm.get_account(&w.m.vault_a).unwrap();
    if let Some(account) = replacement {
        match slot {
            Slot::TraderInput | Slot::TraderOutput => {
                let address = Pubkey::new_unique();
                w.env.svm.set_account(address, account).unwrap();
                if slot == Slot::TraderInput {
                    accounts.trader_input = address;
                } else {
                    accounts.trader_output = address;
                }
            }
            Slot::InputVault => {
                w.env.svm.set_account(w.m.vault_a, account).unwrap();
            }
        }
    }
    let ix = client::swap(&accounts, &exact_in(1_000, 0));
    let result = send_swap(&mut w.env, ix, &w.trader);
    if slot == Slot::InputVault && result.is_err() {
        w.env.svm.set_account(w.m.vault_a, saved_vault).unwrap();
    }
    result
}

fn run_matrix(tp_a: Pubkey, tp_b: Pubkey) {
    let mut w = world(tp_a, tp_b);
    for slot in [Slot::TraderInput, Slot::TraderOutput, Slot::InputVault] {
        let (address, mint, token_program) = match slot {
            Slot::TraderInput => (w.trader_a, w.m.mint_a, tp_a),
            Slot::TraderOutput => (w.trader_b, w.m.mint_b, tp_b),
            Slot::InputVault => (w.m.vault_a, w.m.mint_a, tp_a),
        };
        let base = w.env.svm.get_account(&address).unwrap();
        let mint_account = w.env.svm.get_account(&mint).unwrap();
        let other_mint = if slot == Slot::TraderOutput {
            w.m.mint_a
        } else {
            w.other_mint
        };

        // Control: an unmodified copy of the real account swaps.
        let meta = swap_with(&mut w, slot, Some(base.clone()))
            .unwrap_or_else(|e| panic!("{slot:?} control copy must swap: {:?}", e.err));
        assert_eq!(reported(&meta).0.net_sent, 1_000);
        let base = w.env.svm.get_account(&address).unwrap();

        for (label, account) in bad_variants(&base, &token_program, &other_mint, &mint_account) {
            let watched = [w.trader_a, w.trader_b, w.m.vault_a, w.m.vault_b];
            let before = balances(&w.env, &watched);
            let result = swap_with(&mut w, slot, Some(account));
            match &result {
                Ok(meta) => panic!(
                    "BUG ({tp_a}/{tp_b}): {slot:?} {label} accepted as a token account:\n{}",
                    meta.logs.join("\n")
                ),
                Err(_) => assert_fails_with(result, "BadTokenAccount"),
            }
            assert_eq!(
                balances(&w.env, &watched),
                before,
                "{slot:?} {label}: balances moved"
            );
        }
    }
}

/// INV-ADM-11 (LiteSVM part) and INV-SWAP-17 / INV-ADM-12 (what can be exercised): a
/// swap naming, as `trader_input`, `trader_output` or `input_vault`, an account with the
/// wrong owner, the wrong mint, a mint, the 355-byte multisig length (whatever byte 165
/// says), a short or empty account, or a longer account whose type byte is not
/// "account" fails with `BadTokenAccount` and moves nothing. SPL Token market.
#[test]
fn bok_inv_adm_11_checked_balance_rejects_non_token_accounts_spl() {
    run_matrix(TOKEN, TOKEN);
}

/// As above, on a Token-2022 market (so "the mint account itself" is a Token-2022
/// mint, and the other-program variant is SPL Token).
#[test]
fn bok_inv_adm_11_checked_balance_rejects_non_token_accounts_token_2022() {
    run_matrix(TOKEN_2022, TOKEN_2022);
}

/// INV-ADM-11: a real Token-2022 mint with an extension (length > 165, byte 165 = 1)
/// is refused as `trader_input` / `trader_output`, even if its first 32 bytes were the
/// market mint's key.
#[test]
fn bok_inv_adm_11_token_2022_mint_with_extensions_is_not_a_token_account() {
    let mut env = Env::new();
    let mint_a = env.create_mint(&TOKEN_2022, 6, Some((10, 1_000)), None);
    let mint_b = env.create_mint(&TOKEN_2022, 6, Some((10, 1_000)), None);
    let m = setup_cp_market_with_mints(
        &mut env,
        mint_a,
        TOKEN_2022,
        mint_b,
        TOKEN_2022,
        RESERVE,
        RESERVE,
        30,
        vec![],
    );
    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN_2022, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN_2022);
    for (slot, mint, mint_key) in [(0, mint_a, mint_a), (1, mint_b, mint_b)] {
        let mut real = env.svm.get_account(&mint).unwrap();
        assert!(
            real.data.len() > 165 && real.data[165] == 1,
            "mint type byte"
        );
        for disguise in [false, true] {
            let address = if disguise {
                real.data[..32].copy_from_slice(mint_key.as_ref());
                let address = Pubkey::new_unique();
                env.svm.set_account(address, real.clone()).unwrap();
                address
            } else {
                mint
            };
            let mut accounts =
                cp_swap_accounts(&m, &trader.pubkey(), &mint_a, &trader_a, &mint_b, &trader_b);
            if slot == 0 {
                accounts.trader_input = address;
            } else {
                accounts.trader_output = address;
            }
            let before = balances(&env, &[trader_a, trader_b, m.vault_a, m.vault_b]);
            assert_fails_with(
                send_swap(
                    &mut env,
                    client::swap(&accounts, &exact_in(1_000, 0)),
                    &trader,
                ),
                "BadTokenAccount",
            );
            assert_eq!(
                balances(&env, &[trader_a, trader_b, m.vault_a, m.vault_b]),
                before
            );
        }
    }
}

/// INV-SWAP-17 / INV-ADM-12 (postcondition): after every successful swap, the three
/// accounts the router measured are still real token accounts of the right program
/// and mint (owner = token program, mint bytes, length ≥ 165 and ≠ 355, type byte
/// "account" when longer), so the post-call reads described the same accounts.
#[test]
fn bok_inv_swap_17_successful_swaps_leave_real_token_accounts() {
    for (tp_a, tp_b) in [
        (TOKEN, TOKEN),
        (TOKEN_2022, TOKEN),
        (TOKEN_2022, TOKEN_2022),
    ] {
        let mut w = world(tp_a, tp_b);
        let meta = swap_with(&mut w, Slot::TraderInput, None).expect("swap");
        reported(&meta);
        for (address, mint, program) in [
            (w.trader_a, w.m.mint_a, tp_a),
            (w.trader_b, w.m.mint_b, tp_b),
            (w.m.vault_a, w.m.mint_a, tp_a),
        ] {
            let account = w.env.svm.get_account(&address).unwrap();
            assert_eq!(account.owner, program);
            assert!(account.data.len() >= 165 && account.data.len() != 355);
            assert!(account.data.len() == 165 || account.data[165] == 2);
            assert_eq!(&account.data[..32], mint.as_ref());
        }
    }
}
