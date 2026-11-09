//! SOS audit #2, H006: `sweep_fees` signs a Token-2022 `transfer_checked` as the market's
//! fee authority and appends a caller-chosen transfer-hook tail. The same PDA owns the
//! market's fee accounts in every mint, including legacy SPL Token accounts. A hostile hook
//! on the swept mint must not be able to use the fee authority's signature on any of them.
//!
//! The hostile hook is `test-hook-refill`: on every transfer of its mint it moves the whole
//! balance of extra account 0 ("stash") to extra account 2 ("target") with an SPL Token
//! `Transfer` whose authority is extra account 1. Its `ExtraAccountMetaList` is written with
//! `set_account` so the authority entry can carry the signer flag.
//!
//! Asserted here:
//! - control: the hook runs inside `sweep_fees` and receives the caller's tail, so a refusal
//!   below is about the signer privilege and not about the hook never running;
//! - with the fee authority as the hook's authority (signer flag set or not), the hook's CPI
//!   fails with a privilege escalation and the legacy-SPL fee account keeps its balance,
//!   owner and (absent) delegate;
//! - the hostile hook on mint A does not stop mint B's fees from being swept to the
//!   treasury.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_account::Account;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
const REFILL_HOOK_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ4");
const EXECUTE: [u8; 8] = [105, 37, 101, 197, 75, 251, 102, 26];

/// Fees "accrued" in each mint's fee account.
const FEES_A: u64 = 1_000;
const FEES_B: u64 = 7_777;
/// The hook's own stash, used only by the control.
const STASH: u64 = 5_000;

fn compute(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction::new_with_bytes(
        client::pubkey!("ComputeBudget111111111111111111111111111111"),
        &data,
        vec![],
    )
}

fn validation_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], &REFILL_HOOK_ID).0
}

/// Writes the refill hook's `ExtraAccountMetaList` for `mint` with fixed-address entries
/// `(address, is_signer, is_writable)`.
fn set_validation(env: &mut Env, mint: &Pubkey, entries: &[(Pubkey, bool, bool)]) {
    let mut tlv = EXECUTE.to_vec();
    tlv.extend_from_slice(&(4 + 35 * entries.len() as u32).to_le_bytes());
    tlv.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (key, signer, writable) in entries {
        tlv.push(0);
        tlv.extend_from_slice(key.as_ref());
        tlv.push(*signer as u8);
        tlv.push(*writable as u8);
    }
    let lamports = env.svm.minimum_balance_for_rent_exemption(tlv.len());
    env.svm
        .set_account(
            validation_address(mint),
            Account {
                lamports,
                data: tlv,
                owner: REFILL_HOOK_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
}

struct World {
    env: Env,
    market: Pubkey,
    fee_authority: Pubkey,
    /// Token-2022 mint whose transfer hook is the hostile refill hook.
    mint_a: Pubkey,
    /// Legacy SPL Token mint.
    mint_b: Pubkey,
    fee_a: Pubkey,
    fee_b: Pubkey,
    treasury_a: Pubkey,
    treasury_b: Pubkey,
    /// The hook issuer's own legacy-SPL account in mint B.
    sink_b: Pubkey,
    /// A legacy-SPL account in mint B owned by the hook's `["stash"]` PDA (control only).
    stash: Pubkey,
    stash_authority: Pubkey,
}

/// An evil-strategy market with mints A (Token-2022, hostile hook) and B (legacy SPL),
/// both fee accounts created and holding fees, and the treasury's accounts created.
fn world() -> World {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    env.svm
        .add_program_from_file(REFILL_HOOK_ID, sbf_dir().join("test_hook_refill.so"))
        .unwrap();
    let mint_a = env.create_mint(&TOKEN_2022, 6, None, Some(REFILL_HOOK_ID));
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    set_validation(&mut env, &mint_a, &[]);

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
        max_creator_lamports: MAX_SETUP_LAMPORTS,
        strategy_accounts: vec![
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
        ],
    });
    env.send(&[ix], &[&creator]).expect("create market");
    let market = pda::market(&EVIL_ID, &state);
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &market, &mint_a, &TOKEN_2022),
            client::init_fee_account(&payer, &market, &mint_b, &TOKEN),
        ],
        &[],
    )
    .expect("init fee accounts");

    let fee_authority = pda::fee_authority(&market);
    let fee_a = pda::fee_account(&market, &mint_a, &TOKEN_2022);
    let fee_b = pda::fee_account(&market, &mint_b, &TOKEN);
    // Anyone can send tokens to a fee account; minting stands in for accrued fees.
    env.mint_to(&mint_a, &TOKEN_2022, &fee_a, FEES_A);
    env.mint_to(&mint_b, &TOKEN, &fee_b, FEES_B);

    let treasury_owner = env.treasury_owner;
    let treasury_a = env.create_ata(&treasury_owner, &mint_a, &TOKEN_2022);
    let treasury_b = env.create_ata(&treasury_owner, &mint_b, &TOKEN);
    let sink_b = env.create_ata(&Pubkey::new_unique(), &mint_b, &TOKEN);
    let stash_authority = Pubkey::find_program_address(&[b"stash"], &REFILL_HOOK_ID).0;
    let stash = env.create_ata(&stash_authority, &mint_b, &TOKEN);
    env.mint_to(&mint_b, &TOKEN, &stash, STASH);

    assert_eq!(env.token_balance(&fee_a), FEES_A);
    assert_eq!(env.token_balance(&fee_b), FEES_B);
    World {
        env,
        market,
        fee_authority,
        mint_a,
        mint_b,
        fee_a,
        fee_b,
        treasury_a,
        treasury_b,
        sink_b,
        stash,
        stash_authority,
    }
}

/// `sweep_fees` for mint A with the hook program, its validation account and `extras`
/// as the hook tail.
fn sweep_a(w: &mut World, extras: Vec<AccountMeta>) -> TxResult {
    let mut tail = vec![
        AccountMeta::new_readonly(REFILL_HOOK_ID, false),
        AccountMeta::new_readonly(validation_address(&w.mint_a), false),
    ];
    tail.extend(extras);
    let ix = client::sweep_fees(
        &w.market,
        &w.mint_a,
        &TOKEN_2022,
        &w.env.treasury_owner,
        tail,
    );
    w.env.send(&[compute(400_000), ix], &[])
}

/// Owner (32..64), delegate option tag (72..76) and amount of a legacy SPL account.
fn spl_owner_delegate_amount(env: &Env, account: &Pubkey) -> (Pubkey, u32, u64) {
    let data = env.account_data(account);
    let owner = Pubkey::try_from(&data[32..64]).unwrap();
    let delegate_tag = u32::from_le_bytes(data[72..76].try_into().unwrap());
    let amount = u64::from_le_bytes(data[64..72].try_into().unwrap());
    (owner, delegate_tag, amount)
}

/// Control: with the hook's own PDA as the authority, the hook runs during `sweep_fees`,
/// gets the caller's tail and moves its stash, and the sweep of A still completes.
#[test]
fn control_hook_runs_inside_sweep_fees_with_the_callers_tail() {
    let mut w = world();
    let mint_a = w.mint_a;
    set_validation(
        &mut w.env,
        &mint_a,
        &[
            (w.stash, false, true),
            (w.stash_authority, false, false),
            (w.sink_b, false, true),
            (TOKEN, false, false),
        ],
    );
    let extras = vec![
        AccountMeta::new(w.stash, false),
        AccountMeta::new_readonly(w.stash_authority, false),
        AccountMeta::new(w.sink_b, false),
        AccountMeta::new_readonly(TOKEN, false),
    ];
    let result = sweep_a(&mut w, extras);
    if let Err(e) = &result {
        println!("control failed: {:?}\n{}", e.err, e.meta.logs.join("\n"));
    }
    result.expect("control sweep");
    assert_eq!(w.env.token_balance(&w.sink_b), STASH, "hook did not run");
    assert_eq!(w.env.token_balance(&w.treasury_a), FEES_A);
    assert_eq!(w.env.token_balance(&w.fee_a), 0);
    assert_eq!(w.env.token_balance(&w.fee_b), FEES_B);
}

/// The probe: mint A's hook names the market's legacy-SPL fee account as its stash and
/// the fee authority as the transfer authority (with and without the signer flag in the
/// meta list), then tries to move the fees to its own account during `sweep_fees(A)`.
/// Token-2022 passes the authority to the hook without signer privilege, so the hook's
/// CPI is refused and the fee account is untouched.
#[test]
fn hook_cannot_use_the_fee_authority_signature_on_the_legacy_fee_account() {
    for signer_flag in [true, false] {
        let mut w = world();
        let mint_a = w.mint_a;
        set_validation(
            &mut w.env,
            &mint_a,
            &[
                (w.fee_b, false, true),
                (w.fee_authority, signer_flag, false),
                (w.sink_b, false, true),
                (TOKEN, false, false),
            ],
        );
        let extras = vec![
            AccountMeta::new(w.fee_b, false),
            AccountMeta::new_readonly(w.fee_authority, false),
            AccountMeta::new(w.sink_b, false),
            AccountMeta::new_readonly(TOKEN, false),
        ];
        let before = spl_owner_delegate_amount(&w.env, &w.fee_b);
        assert_eq!(before, (w.fee_authority, 0, FEES_B));

        let result = sweep_a(&mut w, extras);
        match &result {
            Ok(meta) => println!(
                "[signer flag {signer_flag}] sweep SUCCEEDED\n{}",
                meta.logs.join("\n")
            ),
            Err(e) => println!(
                "[signer flag {signer_flag}] sweep failed: {:?}\n{}",
                e.err,
                e.meta.logs.join("\n")
            ),
        }
        assert_eq!(
            spl_owner_delegate_amount(&w.env, &w.fee_b),
            before,
            "H006: a transfer hook on mint A used the fee authority's signature on the \
             market's legacy-SPL fee account (signer flag {signer_flag})"
        );
        assert_eq!(
            w.env.token_balance(&w.sink_b),
            0,
            "H006: fees reached the hook issuer's account"
        );
        // The refusal is specifically about the fee authority's signer privilege, raised
        // at the hook's CPI (so the hook received the authority as a non-signer).
        let needle = format!("{}'s signer privilege escalated", w.fee_authority);
        assert_fails_with(result, &needle);
        // The failed sweep reverted as a whole.
        assert_eq!(w.env.token_balance(&w.fee_a), FEES_A);
        assert_eq!(w.env.token_balance(&w.treasury_a), 0);
    }
}

/// Same probe with the fee authority also placed a second time in the caller's tail,
/// writable, so it appears twice in Token-2022's instruction. Duplicated metas still give
/// the hook no signature.
#[test]
fn duplicated_fee_authority_in_the_tail_gains_no_signature() {
    let mut w = world();
    let mint_a = w.mint_a;
    set_validation(
        &mut w.env,
        &mint_a,
        &[
            (w.fee_b, false, true),
            (w.fee_authority, true, true),
            (w.sink_b, false, true),
            (TOKEN, false, false),
        ],
    );
    let extras = vec![
        AccountMeta::new(w.fee_b, false),
        AccountMeta::new(w.fee_authority, false),
        AccountMeta::new(w.sink_b, false),
        AccountMeta::new_readonly(TOKEN, false),
        AccountMeta::new(w.fee_authority, false),
    ];
    let result = sweep_a(&mut w, extras);
    if let Err(e) = &result {
        println!("duplicate probe: {:?}\n{}", e.err, e.meta.logs.join("\n"));
    }
    assert_eq!(
        w.env.token_balance(&w.fee_b),
        FEES_B,
        "H006: duplicated fee authority let the hook move legacy-SPL fees"
    );
    assert_eq!(w.env.token_balance(&w.sink_b), 0);
    let needle = format!("{}'s signer privilege escalated", w.fee_authority);
    assert_fails_with(result, &needle);
}

/// A hostile hook on mint A blocks only A's sweep; B's fees still reach the treasury.
#[test]
fn hostile_hook_on_one_mint_does_not_block_the_other_mints_sweep() {
    let mut w = world();
    let mint_a = w.mint_a;
    set_validation(
        &mut w.env,
        &mint_a,
        &[
            (w.fee_b, false, true),
            (w.fee_authority, true, false),
            (w.sink_b, false, true),
            (TOKEN, false, false),
        ],
    );
    let extras = vec![
        AccountMeta::new(w.fee_b, false),
        AccountMeta::new_readonly(w.fee_authority, false),
        AccountMeta::new(w.sink_b, false),
        AccountMeta::new_readonly(TOKEN, false),
    ];
    assert!(sweep_a(&mut w, extras).is_err());

    let treasury_owner = w.env.treasury_owner;
    let ix = client::sweep_fees(&w.market, &w.mint_b, &TOKEN, &treasury_owner, vec![]);
    w.env.send(&[ix], &[]).expect("sweep B");
    assert_eq!(w.env.token_balance(&w.treasury_b), FEES_B);
    assert_eq!(w.env.token_balance(&w.fee_b), 0);
    assert_eq!(w.env.token_balance(&w.sink_b), 0);
}
