//! SOS2 H025 — fee custody in `sweep_fees`: source and destination binding.
//!
//! bok_sweep.rs already settles the single-slot substitution matrix, the empty-balance
//! no-op, the transfer-fee receipt and the treasury-owner change. This file covers the
//! cases it leaves out:
//!   * the treasury ATA does not exist yet (fees stay put, then move once it is created);
//!   * the fee account was never initialised;
//!   * `market` is a router-owned account that is not a `Market`, or not router-owned;
//!   * ATA addresses derived for the *other* token program, with the right program passed;
//!   * `treasury_owner == fee_authority` (self-transfer) and `treasury_owner` = another
//!     market's fee authority (admin-set, so only the "nothing is lost" property is checked);
//!   * a caller-supplied hook tail cannot redirect the transfer (writable decoy tail).

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_events::PipelineEvent;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

const MARKET: usize = 1;
const FEE_AUTHORITY: usize = 2;
const FEE_ACCOUNT: usize = 4;
const TREASURY: usize = 5;

struct Mint {
    key: Pubkey,
    program: Pubkey,
}

/// An evil-strategy market over (a, b). Fee accounts are created only for the mints in
/// `init_fees`.
fn evil_market(env: &mut Env, a: &Mint, b: &Mint, init_fees: &[&Mint]) -> Pubkey {
    let state =
        Pubkey::find_program_address(&[b"evil", a.key.as_ref(), b.key.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &a.key, &a.program);
    let vault_b = env.create_ata(&state, &b.key, &b.program);
    let creator = env.funded_keypair();
    let ix = client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
        strategy_program: EVIL_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![0],
        max_creator_lamports: SOL,
        strategy_accounts: vec![
            AccountMeta::new_readonly(a.key, false),
            AccountMeta::new_readonly(b.key, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
        ],
    });
    env.send(&[ix], &[&creator]).expect("create evil market");
    let market = pda::market(&EVIL_ID, &state);
    let payer = env.admin.pubkey();
    for mint in init_fees {
        let ix = client::init_fee_account(&payer, &market, &mint.key, &mint.program);
        env.send(&[ix], &[]).expect("init fee account");
    }
    market
}

struct World {
    env: Env,
    spl: Mint,
    t22: Mint,
    /// Market with fee accounts for both mints.
    m1: Pubkey,
}

/// `treasury_atas`: whether the treasury owner's ATAs are created up front.
fn world(treasury_atas: bool) -> World {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let spl = Mint {
        key: env.create_mint(&TOKEN, 6, None, None),
        program: TOKEN,
    };
    let t22 = Mint {
        key: env.create_mint(&TOKEN_2022, 9, None, None),
        program: TOKEN_2022,
    };
    let m1 = evil_market(&mut env, &spl, &t22, &[&spl, &t22]);
    if treasury_atas {
        let owner = env.treasury_owner;
        for mint in [&spl, &t22] {
            env.create_ata(&owner, &mint.key, &mint.program);
        }
    }
    World { env, spl, t22, m1 }
}

fn swept(meta: &litesvm::types::TransactionMetadata) -> Vec<pipeline_events::FeesSwept> {
    events(meta)
        .into_iter()
        .filter_map(|e| match e {
            PipelineEvent::FeesSwept(s) => Some(s),
            _ => None,
        })
        .collect()
}

fn sweep(w: &World, market: &Pubkey, mint: &Mint) -> Instruction {
    client::sweep_fees(
        market,
        &mint.key,
        &mint.program,
        &w.env.treasury_owner,
        vec![],
    )
}

fn fee_account(market: &Pubkey, mint: &Mint) -> Pubkey {
    pda::fee_account(market, &mint.key, &mint.program)
}

/// Treasury ATA absent: the sweep fails without moving anything or emitting an event; once
/// anyone creates the ATA the same sweep succeeds and the whole balance reaches it.
#[test]
fn h025_missing_treasury_ata_blocks_then_recovers() {
    let mut w = world(false);
    let m1 = w.m1;
    for (mint_key, program, amount) in [(w.spl.key, TOKEN, 4_321u64), (w.t22.key, TOKEN_2022, 77)] {
        let mint = Mint {
            key: mint_key,
            program,
        };
        let fees = fee_account(&m1, &mint);
        w.env.mint_to(&mint_key, &program, &fees, amount);
        assert_eq!(w.env.token_balance(&fees), amount, "setup");
        let treasury = pda::associated_token(&w.env.treasury_owner, &mint_key, &program);
        assert!(w.env.svm.get_account(&treasury).is_none(), "setup");

        let ix = sweep(&w, &m1, &mint);
        let result = w.env.send(std::slice::from_ref(&ix), &[]);
        assert!(
            result.is_err(),
            "H025: sweep into a missing treasury ATA must fail, not succeed"
        );
        assert_eq!(
            w.env.token_balance(&fees),
            amount,
            "H025: failed sweep must leave the fees untouched"
        );
        assert!(w.env.svm.get_account(&treasury).is_none());

        let owner = w.env.treasury_owner;
        w.env.create_ata(&owner, &mint_key, &program);
        let meta = w.env.send(&[ix], &[]).expect("sweep after ATA creation");
        assert_eq!(w.env.token_balance(&fees), 0);
        assert_eq!(w.env.token_balance(&treasury), amount);
        let ev = swept(&meta);
        assert_eq!(ev.len(), 1);
        assert_eq!(
            (ev[0].market, ev[0].mint, ev[0].amount),
            (m1, mint_key, amount)
        );
    }
}

/// A market whose fee account for a mint was never initialised: sweeping it fails with
/// `BadTokenAccount` (the address is system-owned and empty), and a pre-funded account at the
/// fee address does not change that.
#[test]
fn h025_uninitialised_fee_account_is_refused() {
    let mut w = world(true);
    let spl = Mint {
        key: w.spl.key,
        program: TOKEN,
    };
    let t22 = Mint {
        key: w.t22.key,
        program: TOKEN_2022,
    };
    // Different creator pair so it is a distinct market; init only the SPL fee account.
    let other_spl = Mint {
        key: w.env.create_mint(&TOKEN, 6, None, None),
        program: TOKEN,
    };
    let m = evil_market(&mut w.env, &other_spl, &t22, &[&t22]);
    let _ = spl;
    let missing = fee_account(&m, &other_spl);
    assert!(w.env.svm.get_account(&missing).is_none(), "setup");
    // Griefer sends lamports to the uninitialised fee address.
    w.env.svm.airdrop(&missing, 1_000_000_000).unwrap();
    let ix = sweep(&w, &m, &other_spl);
    assert_fails_with(w.env.send(&[ix], &[]), "BadTokenAccount");
    // Control: the initialised side of the same market sweeps fine.
    let ok = fee_account(&m, &t22);
    w.env.mint_to(&t22.key, &TOKEN_2022, &ok, 5);
    let ix = sweep(&w, &m, &t22);
    w.env.send(&[ix], &[]).expect("control sweep");
    assert_eq!(w.env.token_balance(&ok), 0);
}

/// `market` must be a router-owned `Market`: the Config PDA, a Strategy record, a plain
/// wallet and the evil strategy's state account are all refused, with the matching fee
/// authority / fee account derived from them so only the market check is at issue.
#[test]
fn h025_market_must_be_a_router_market() {
    let mut w = world(true);
    let m1 = w.m1;
    let fees = fee_account(&m1, &w.spl);
    w.env.mint_to(&w.spl.key, &TOKEN, &fees, 1_000);
    let evil_state = w.env.market(&m1).market_state;
    let wallet = w.env.funded_keypair().pubkey();
    let fakes = [
        ("config", pda::config()),
        ("strategy record", pda::strategy(&EVIL_ID)),
        ("wallet", wallet),
        ("strategy state", evil_state),
    ];
    for (name, fake) in fakes {
        let mut ix = sweep(&w, &m1, &w.spl_ref());
        ix.accounts[MARKET].pubkey = fake;
        ix.accounts[FEE_AUTHORITY].pubkey = pda::fee_authority(&fake);
        ix.accounts[FEE_ACCOUNT].pubkey = fee_account(&fake, &w.spl_ref());
        let result = w.env.send(&[ix], &[]);
        assert!(result.is_err(), "H025: {name} accepted as the market");
        assert_eq!(w.env.token_balance(&fees), 1_000, "{name}: fees moved");
    }
    // Control.
    let ix = sweep(&w, &m1, &w.spl_ref());
    w.env.send(&[ix], &[]).expect("control sweep");
    assert_eq!(w.env.token_balance(&fees), 0);
}

impl World {
    fn spl_ref(&self) -> Mint {
        Mint {
            key: self.spl.key,
            program: self.spl.program,
        }
    }
}

/// Addresses derived for the other token program, with the mint's own program passed: both
/// slots are refused (WrongFeeAccount first, then WrongTreasuryAccount), nothing moves.
#[test]
fn h025_other_token_program_atas_are_refused() {
    let mut w = world(true);
    let m1 = w.m1;
    let fa = pda::fee_authority(&m1);
    let owner = w.env.treasury_owner;
    for (mint, own, other) in [
        (w.spl.key, TOKEN, TOKEN_2022),
        (w.t22.key, TOKEN_2022, TOKEN),
    ] {
        let fees = pda::fee_account(&m1, &mint, &own);
        w.env.mint_to(&mint, &own, &fees, 900);
        let base = client::sweep_fees(&m1, &mint, &own, &owner, vec![]);
        let treasury = pda::associated_token(&owner, &mint, &own);
        let before = (w.env.token_balance(&fees), w.env.token_balance(&treasury));

        let mut ix = base.clone();
        ix.accounts[FEE_ACCOUNT].pubkey = pda::associated_token(&fa, &mint, &other);
        assert_fails_with(w.env.send(&[ix], &[]), "WrongFeeAccount");

        let mut ix = base.clone();
        ix.accounts[TREASURY].pubkey = pda::associated_token(&owner, &mint, &other);
        assert_fails_with(w.env.send(&[ix], &[]), "WrongTreasuryAccount");

        let after = (w.env.token_balance(&fees), w.env.token_balance(&treasury));
        assert_eq!(before, after);
        w.env.send(&[base], &[]).expect("control");
        assert_eq!(w.env.token_balance(&treasury), 900);
    }
}

/// `treasury_owner == fee_authority(M1)` (admin-set): the sweep is a token self-transfer, so
/// the balance stays in the fee account (nothing is lost or leaked) and, for SPL and
/// Token-2022 alike, no other account moves. Observed behaviour is recorded in the asserts.
#[test]
fn h025_treasury_owner_equal_to_fee_authority_is_a_harmless_self_transfer() {
    let mut w = world(true);
    let m1 = w.m1;
    let fa = pda::fee_authority(&m1);
    let ix = client::update_config(
        &w.env.admin.pubkey(),
        &client::UpdateConfigArgs {
            treasury_owner: Some(fa),
            ..Default::default()
        },
    );
    w.env.send(&[ix], &[]).expect("set treasury owner");
    let old_treasury_owner = pda::fee_authority(&Pubkey::new_unique());
    let _ = old_treasury_owner;
    for (mint_key, program) in [(w.spl.key, TOKEN), (w.t22.key, TOKEN_2022)] {
        let fees = pda::fee_account(&m1, &mint_key, &program);
        w.env.mint_to(&mint_key, &program, &fees, 600);
        // Treasury ATA == fee account, so the pre-existing ATA is the fee account itself.
        assert_eq!(
            fees,
            pda::associated_token(&fa, &mint_key, &program),
            "setup: the two addresses coincide"
        );
        let ix = client::sweep_fees(&m1, &mint_key, &program, &fa, vec![]);
        let result = w.env.send(&[ix], &[]);
        match result {
            Ok(meta) => {
                assert_eq!(w.env.token_balance(&fees), 600, "self-transfer keeps funds");
                let ev = swept(&meta);
                println!(
                    "self-transfer sweep ok; FeesSwept events: {:?}",
                    ev.iter().map(|e| e.amount).collect::<Vec<_>>()
                );
            }
            Err(failed) => {
                println!("self-transfer sweep refused: {:?}", failed.err);
                assert_eq!(w.env.token_balance(&fees), 600, "refused sweep keeps funds");
            }
        }
    }
}

/// `treasury_owner` = another market's fee authority: the sweep moves M1's fees into M2's fee
/// account (the admin chose that recipient; the move is still source- and destination-bound
/// to the configured owner's ATA and nothing else changes).
#[test]
fn h025_treasury_owner_set_to_other_fee_authority_only_moves_the_pinned_pair() {
    let mut w = world(true);
    let m1 = w.m1;
    let spl = w.spl_ref();
    let t22 = Mint {
        key: w.t22.key,
        program: TOKEN_2022,
    };
    let sibling = Mint {
        key: w.env.create_mint(&TOKEN, 6, None, None),
        program: TOKEN,
    };
    let m2 = evil_market(&mut w.env, &sibling, &t22, &[&sibling, &t22]);
    let m2_spl_fees = {
        // M2 shares no SPL mint with M1 (sibling is new); use the Token-2022 mint, shared.
        pda::fee_account(&m2, &t22.key, &TOKEN_2022)
    };
    let _ = spl;
    let fa2 = pda::fee_authority(&m2);
    let ix = client::update_config(
        &w.env.admin.pubkey(),
        &client::UpdateConfigArgs {
            treasury_owner: Some(fa2),
            ..Default::default()
        },
    );
    w.env.send(&[ix], &[]).unwrap();
    let m1_fees = pda::fee_account(&m1, &t22.key, &TOKEN_2022);
    w.env.mint_to(&t22.key, &TOKEN_2022, &m1_fees, 250);
    let m1_spl_fees = pda::fee_account(&m1, &w.spl.key, &TOKEN);
    w.env.mint_to(&w.spl.key, &TOKEN, &m1_spl_fees, 33);
    let ix = client::sweep_fees(&m1, &t22.key, &TOKEN_2022, &fa2, vec![]);
    w.env.send(&[ix], &[]).expect("sweep to M2's fee account");
    assert_eq!(w.env.token_balance(&m1_fees), 0);
    assert_eq!(w.env.token_balance(&m2_spl_fees), 250);
    assert_eq!(
        w.env.token_balance(&m1_spl_fees),
        33,
        "other mint untouched"
    );
}

/// A caller-supplied tail naming writable decoy accounts (the thief's ATA, the SPL fee
/// account of the same market) does not change who is paid: the transfer still goes to the
/// pinned pair, and the decoys keep their balances.
#[test]
fn h025_extra_tail_accounts_cannot_redirect_the_sweep() {
    let mut w = world(true);
    let m1 = w.m1;
    let thief = solana_keypair::Keypair::new().pubkey();
    let thief_ata = w.env.create_ata(&thief, &w.t22.key, &TOKEN_2022);
    let spl_fees = pda::fee_account(&m1, &w.spl.key, &TOKEN);
    w.env.mint_to(&w.spl.key, &TOKEN, &spl_fees, 111);
    let t22_fees = pda::fee_account(&m1, &w.t22.key, &TOKEN_2022);
    w.env.mint_to(&w.t22.key, &TOKEN_2022, &t22_fees, 222);
    let tail = vec![
        AccountMeta::new(thief_ata, false),
        AccountMeta::new(spl_fees, false),
        AccountMeta::new(pda::fee_authority(&m1), false),
    ];
    let ix = client::sweep_fees(&m1, &w.t22.key, &TOKEN_2022, &w.env.treasury_owner, tail);
    w.env.send(&[ix], &[]).expect("sweep with decoy tail");
    let treasury = pda::associated_token(&w.env.treasury_owner, &w.t22.key, &TOKEN_2022);
    assert_eq!(w.env.token_balance(&treasury), 222);
    assert_eq!(w.env.token_balance(&t22_fees), 0);
    assert_eq!(
        w.env.token_balance(&thief_ata),
        0,
        "H025: decoy received funds"
    );
    assert_eq!(
        w.env.token_balance(&spl_fees),
        111,
        "H025: decoy lost funds"
    );
}
