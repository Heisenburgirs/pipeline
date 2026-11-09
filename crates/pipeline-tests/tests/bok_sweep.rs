//! BOK LiteSVM tests — fee custody in `sweep_fees` (INV-ADM-8, INV-ADM-9), run against
//! the real router with SPL Token, Token-2022 and a Token-2022 transfer-fee mint.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_events::PipelineEvent;
use proptest::prelude::*;
use proptest::test_runner::{Config as RunnerConfig, TestRunner};
use solana_keypair::Keypair;
use solana_signer::Signer;
use spl_token_2022_interface::extension::transfer_fee::{TransferFeeAmount, TransferFeeConfig};
use spl_token_2022_interface::extension::{BaseStateWithExtensions, StateWithExtensions};

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

/// Account positions in `client::sweep_fees`.
const MARKET: usize = 1;
const FEE_AUTHORITY: usize = 2;
const FEE_ACCOUNT: usize = 4;
const TREASURY: usize = 5;
const TOKEN_PROGRAM: usize = 6;

struct Mint {
    key: Pubkey,
    program: Pubkey,
}

/// Two evil-strategy markets sharing one SPL mint: M1 = (spl, transfer-fee 2022),
/// M2 = (spl, plain 2022). Every market/mint pair has a fee account, and the treasury
/// owner has an associated token account for every mint.
struct World {
    env: Env,
    spl: Mint,
    fee_mint: Mint,
    plain_2022: Mint,
    stranger: Mint,
    m1: Pubkey,
    m2: Pubkey,
}

fn evil_market(env: &mut Env, a: &Mint, b: &Mint) -> Pubkey {
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
    for mint in [a, b] {
        let ix = client::init_fee_account(&payer, &market, &mint.key, &mint.program);
        env.send(&[ix], &[]).expect("init fee account");
    }
    market
}

fn world() -> World {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let spl = Mint {
        key: env.create_mint(&TOKEN, 6, None, None),
        program: TOKEN,
    };
    // 1% transfer fee, capped high enough never to bind in these tests.
    let fee_mint = Mint {
        key: env.create_mint(&TOKEN_2022, 6, Some((100, u64::MAX / 2)), None),
        program: TOKEN_2022,
    };
    let plain_2022 = Mint {
        key: env.create_mint(&TOKEN_2022, 9, None, None),
        program: TOKEN_2022,
    };
    let stranger = Mint {
        key: env.create_mint(&TOKEN, 6, None, None),
        program: TOKEN,
    };
    let m1 = evil_market(&mut env, &spl, &fee_mint);
    let m2 = evil_market(&mut env, &spl, &plain_2022);
    let treasury_owner = env.treasury_owner;
    for mint in [&spl, &fee_mint, &plain_2022, &stranger] {
        env.create_ata(&treasury_owner, &mint.key, &mint.program);
    }
    World {
        env,
        spl,
        fee_mint,
        plain_2022,
        stranger,
        m1,
        m2,
    }
}

impl World {
    fn fee_account(&self, market: &Pubkey, mint: &Mint) -> Pubkey {
        pda::fee_account(market, &mint.key, &mint.program)
    }

    fn treasury(&self, mint: &Mint) -> Pubkey {
        pda::associated_token(&self.env.treasury_owner, &mint.key, &mint.program)
    }

    fn sweep_ix(&self, market: &Pubkey, mint: &Mint) -> Instruction {
        client::sweep_fees(
            market,
            &mint.key,
            &mint.program,
            &self.env.treasury_owner,
            vec![],
        )
    }

    /// Mints `amount` straight into a fee account (the mint authority can credit any
    /// token account), standing in for collected swap fees.
    fn credit(&mut self, account: &Pubkey, mint_key: &Pubkey, program: &Pubkey, amount: u64) {
        self.env.mint_to(mint_key, program, account, amount);
    }

    /// Every token account the tests care about, with its full data.
    fn tracked(&self) -> Vec<Pubkey> {
        let mut keys = vec![];
        for market in [self.m1, self.m2] {
            for mint in [&self.spl, &self.fee_mint, &self.plain_2022] {
                keys.push(self.fee_account(&market, mint));
            }
        }
        for mint in [&self.spl, &self.fee_mint, &self.plain_2022, &self.stranger] {
            keys.push(self.treasury(mint));
        }
        keys
    }

    fn snapshot(&self, extra: &[Pubkey]) -> Vec<(Pubkey, Vec<u8>)> {
        self.tracked()
            .into_iter()
            .chain(extra.iter().copied())
            .map(|k| (k, self.env.account_data(&k)))
            .collect()
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
fn transfer_fee(env: &Env, mint: &Pubkey, amount: u64) -> u64 {
    let data = env.account_data(mint);
    if data.len() <= 82 {
        return 0;
    }
    let state = StateWithExtensions::<spl_token_2022_interface::state::Mint>::unpack(&data)
        .expect("token-2022 mint");
    let epoch = env.svm.get_sysvar::<solana_clock::Clock>().epoch;
    state
        .get_extension::<TransferFeeConfig>()
        .map(|config| config.calculate_epoch_fee(epoch, amount).unwrap())
        .unwrap_or(0)
}

fn swept_events(meta: &litesvm::types::TransactionMetadata) -> Vec<pipeline_events::FeesSwept> {
    events(meta)
        .into_iter()
        .filter_map(|e| match e {
            PipelineEvent::FeesSwept(s) => Some(s),
            _ => None,
        })
        .collect()
}

/// Asserts that only `changed` accounts differ between two snapshots.
fn assert_only_changed(
    before: &[(Pubkey, Vec<u8>)],
    after: &[(Pubkey, Vec<u8>)],
    changed: &[Pubkey],
) {
    for ((key, old), (_, new)) in before.iter().zip(after) {
        if !changed.contains(key) {
            assert_eq!(old, new, "token account {key} changed but should not have");
        }
    }
}

// ---------------------------------------------------------------------------------------
// INV-ADM-8 — a sweep moves the whole balance and nothing else
// ---------------------------------------------------------------------------------------

/// INV-ADM-8: sweeping an empty fee account succeeds and does nothing: no token CPI, no
/// `FeesSwept` event, no account changes.
#[test]
fn bok_inv_adm_8_empty_fee_account_sweep_is_a_no_op() {
    let mut w = world();
    for (market, mint) in [(w.m1, &w.spl), (w.m1, &w.fee_mint), (w.m2, &w.plain_2022)] {
        assert_eq!(w.env.token_balance(&w.fee_account(&market, mint)), 0);
        let before = w.snapshot(&[]);
        let ix = w.sweep_ix(&market, mint);
        let meta = w.env.send(&[ix], &[]).expect("empty sweep succeeds");
        assert!(
            meta.inner_instructions.iter().all(|inner| inner.is_empty()),
            "no CPI at all for an empty sweep"
        );
        assert!(swept_events(&meta).is_empty());
        assert_eq!(w.snapshot(&[]), before);
    }
}

/// INV-ADM-8: for SPL Token and plain Token-2022, a sweep empties the fee account, the
/// treasury gains exactly the swept amount, the event reports it, and no other tracked
/// account changes, including the other market's fee account in the shared mint.
#[test]
fn bok_inv_adm_8_sweep_moves_the_whole_balance_and_nothing_else() {
    let mut w = world();
    // Fill every fee account so a stray movement anywhere would show.
    let (m1, m2) = (w.m1, w.m2);
    let fills = [
        (m1, w.spl.key, TOKEN, 11),
        (m2, w.spl.key, TOKEN, 22),
        (m1, w.fee_mint.key, TOKEN_2022, 33),
        (m2, w.plain_2022.key, TOKEN_2022, 44),
    ];
    for (market, mint, program, amount) in fills {
        let account = pda::fee_account(&market, &mint, &program);
        w.credit(&account, &mint, &program, amount);
    }
    for (market, which, amount) in [
        (m1, 0, 1u64),
        (m2, 0, 987_654_321),
        (m2, 1, 1),
        (m2, 1, u64::MAX / 4),
    ] {
        let mint = if which == 0 { &w.spl } else { &w.plain_2022 };
        let (mint_key, program) = (mint.key, mint.program);
        let fee_account = w.fee_account(&market, mint);
        let treasury = w.treasury(mint);
        w.credit(&fee_account, &mint_key, &program, amount);
        let total = w.env.token_balance(&fee_account);
        let treasury_before = w.env.token_balance(&treasury);
        let before = w.snapshot(&[]);
        let mint = if which == 0 { &w.spl } else { &w.plain_2022 };
        let ix = w.sweep_ix(&market, mint);
        let meta = w.env.send(&[ix], &[]).unwrap();
        assert_eq!(w.env.token_balance(&fee_account), 0);
        assert_eq!(w.env.token_balance(&treasury) - treasury_before, total);
        let swept = swept_events(&meta);
        assert_eq!(swept.len(), 1);
        assert_eq!(
            (swept[0].market, swept[0].mint, swept[0].amount),
            (market, mint_key, total)
        );
        assert_only_changed(&before, &w.snapshot(&[]), &[fee_account, treasury]);
    }
}

/// INV-ADM-8: with a Token-2022 transfer-fee mint the fee account is still emptied; the
/// treasury receives A − tf(A) and the mint's cut tf(A) is withheld on the treasury
/// account, so amount + withheld rises by exactly A.
#[test]
fn bok_inv_adm_8_transfer_fee_mint_treasury_bears_the_cut() {
    let mut w = world();
    let m1 = w.m1;
    let fee_account = w.fee_account(&m1, &w.fee_mint);
    let treasury = w.treasury(&w.fee_mint);
    let (mint_key, program) = (w.fee_mint.key, w.fee_mint.program);
    for amount in [1u64, 99, 100, 10_000, 1_234_567, 1_000_000_000_000] {
        w.credit(&fee_account, &mint_key, &program, amount);
        let a = w.env.token_balance(&fee_account);
        assert_eq!(a, amount);
        let cut = transfer_fee(&w.env, &mint_key, a);
        assert!(amount < 100 || cut > 0, "the mint really charges a fee");
        let (t0, h0) = (w.env.token_balance(&treasury), withheld(&w.env, &treasury));
        let before = w.snapshot(&[]);
        let ix = w.sweep_ix(&m1, &w.fee_mint);
        let meta = w.env.send(&[ix], &[]).unwrap();
        assert_eq!(w.env.token_balance(&fee_account), 0);
        assert_eq!(w.env.token_balance(&treasury) - t0, a - cut, "amount {a}");
        assert_eq!(withheld(&w.env, &treasury) - h0, cut, "amount {a}");
        assert_eq!(swept_events(&meta)[0].amount, a);
        assert_only_changed(&before, &w.snapshot(&[]), &[fee_account, treasury]);
    }
}

// ---------------------------------------------------------------------------------------
// INV-ADM-9 — only market M's fees in mint X, only to the treasury's ATA for X
// ---------------------------------------------------------------------------------------

/// A token account for `mint` owned by `owner` that is not the associated one.
fn plain_token_account(env: &mut Env, mint: &Pubkey, owner: &Pubkey) -> Pubkey {
    let account = Keypair::new();
    let lamports = env.svm.minimum_balance_for_rent_exemption(165);
    let payer = env.admin.pubkey();
    let ixs = [
        solana_system_interface::instruction::create_account(
            &payer,
            &account.pubkey(),
            lamports,
            165,
            &TOKEN,
        ),
        spl_token_interface::instruction::initialize_account3(
            &TOKEN,
            &account.pubkey(),
            mint,
            owner,
        )
        .unwrap(),
    ];
    env.send(&ixs, &[&account]).expect("plain token account");
    account.pubkey()
}

/// INV-ADM-9: each single substitution in an otherwise valid sweep of M1's SPL fees is
/// refused with its specific error, and no tracked account (including the decoys)
/// changes.
#[test]
fn bok_inv_adm_9_substituted_accounts_are_refused() {
    let mut w = world();
    let m1 = w.m1;
    let m2 = w.m2;
    let fee_account = w.fee_account(&m1, &w.spl);
    let (spl_key, fee_mint_key) = (w.spl.key, w.fee_mint.key);
    w.credit(&fee_account, &spl_key, &TOKEN, 5_000);
    let other_market_fees = w.fee_account(&m2, &w.spl);
    w.credit(&other_market_fees, &spl_key, &TOKEN, 7_000);

    let fee_authority = pda::fee_authority(&m1);
    let non_ata_fees = plain_token_account(&mut w.env, &spl_key, &fee_authority);
    w.credit(&non_ata_fees, &spl_key, &TOKEN, 3_000);
    let thief = Keypair::new().pubkey();
    let thief_ata = w.env.create_ata(&thief, &spl_key, &TOKEN);
    let treasury_owner = w.env.treasury_owner;
    let treasury_non_ata = plain_token_account(&mut w.env, &spl_key, &treasury_owner);
    let decoys = [non_ata_fees, thief_ata, treasury_non_ata];

    let base = w.sweep_ix(&m1, &w.spl);
    let with = |slot: usize, key: Pubkey| {
        let mut ix = base.clone();
        ix.accounts[slot].pubkey = key;
        ix
    };
    let stranger_key = w.stranger.key;
    let cases: Vec<(&str, Instruction, &str)> = vec![
        (
            "other market's fee account",
            with(FEE_ACCOUNT, other_market_fees),
            "WrongFeeAccount",
        ),
        (
            "non-ATA account of the fee authority",
            with(FEE_ACCOUNT, non_ata_fees),
            "WrongFeeAccount",
        ),
        (
            "thief treasury",
            with(TREASURY, thief_ata),
            "WrongTreasuryAccount",
        ),
        (
            "treasury owner's non-ATA account",
            with(TREASURY, treasury_non_ata),
            "WrongTreasuryAccount",
        ),
        (
            "market B with market A's fee account",
            {
                let mut ix = with(MARKET, m2);
                ix.accounts[FEE_AUTHORITY].pubkey = pda::fee_authority(&m2);
                ix
            },
            "WrongFeeAccount",
        ),
        (
            "another market's fee authority",
            with(FEE_AUTHORITY, pda::fee_authority(&m2)),
            "ConstraintSeeds",
        ),
        (
            "mint not in the market",
            client::sweep_fees(&m1, &stranger_key, &TOKEN, &treasury_owner, vec![]),
            "MintNotInMarket",
        ),
        (
            "M2's mint on M1",
            client::sweep_fees(&m1, &w.plain_2022.key, &TOKEN_2022, &treasury_owner, vec![]),
            "MintNotInMarket",
        ),
        (
            "SPL mint with the Token-2022 program",
            client::sweep_fees(&m1, &spl_key, &TOKEN_2022, &treasury_owner, vec![]),
            "WrongTokenProgram",
        ),
        (
            "Token-2022 mint with the SPL program",
            client::sweep_fees(&m1, &fee_mint_key, &TOKEN, &treasury_owner, vec![]),
            "WrongTokenProgram",
        ),
        (
            "a non-token program",
            with(TOKEN_PROGRAM, client::SYSTEM_PROGRAM_ID),
            "WrongTokenProgram",
        ),
    ];
    for (name, ix, error) in cases {
        let before = w.snapshot(&decoys);
        assert_fails_with(w.env.send(&[ix], &[]), error);
        assert_eq!(w.snapshot(&decoys), before, "{name}: balances moved");
    }

    // Control: the unmodified sweep works.
    w.env.send(&[base], &[]).unwrap();
    assert_eq!(w.env.token_balance(&fee_account), 0);
}

/// INV-ADM-9: after `treasury_owner := T2`, sweeps to the old treasury's account are
/// refused and sweeps go to T2's account.
#[test]
fn bok_inv_adm_9_treasury_owner_change_redirects_sweeps() {
    let mut w = world();
    let m1 = w.m1;
    let spl_key = w.spl.key;
    let fee_account = w.fee_account(&m1, &w.spl);
    w.credit(&fee_account, &spl_key, &TOKEN, 8_888);
    let old_owner = w.env.treasury_owner;
    let old_treasury = w.treasury(&w.spl);
    let new_owner = Pubkey::new_unique();
    let new_treasury = w.env.create_ata(&new_owner, &spl_key, &TOKEN);
    let ix = client::update_config(
        &w.env.admin.pubkey(),
        &client::UpdateConfigArgs {
            treasury_owner: Some(new_owner),
            ..Default::default()
        },
    );
    w.env.send(&[ix], &[]).unwrap();

    let old = client::sweep_fees(&m1, &spl_key, &TOKEN, &old_owner, vec![]);
    assert_fails_with(w.env.send(&[old], &[]), "WrongTreasuryAccount");
    assert_eq!(w.env.token_balance(&fee_account), 8_888);

    let new = client::sweep_fees(&m1, &spl_key, &TOKEN, &new_owner, vec![]);
    w.env.send(&[new], &[]).unwrap();
    assert_eq!(w.env.token_balance(&new_treasury), 8_888);
    assert_eq!(w.env.token_balance(&old_treasury), 0);
    assert_eq!(w.env.token_balance(&fee_account), 0);
}

/// INV-ADM-9 (randomised): each of the six address slots is filled with the value derived
/// from the others, or a decoy. The sweep succeeds exactly when every slot is what the
/// invariant requires, moving amount + withheld = A into that treasury; otherwise it
/// fails with the first violated check and nothing changes.
#[test]
fn bok_inv_adm_9_random_substitutions_succeed_only_when_bound() {
    let mut w = world();
    let treasury_owner = w.env.treasury_owner;
    let fee_authority_1 = pda::fee_authority(&w.m1);
    let non_ata_fees = plain_token_account(&mut w.env, &w.spl.key, &fee_authority_1);
    let thief_ata = w
        .env
        .create_ata(&Keypair::new().pubkey(), &w.spl.key, &TOKEN);
    let markets = [w.m1, w.m2];
    let mints: Vec<(Pubkey, Pubkey)> = [&w.spl, &w.fee_mint, &w.plain_2022, &w.stranger]
        .iter()
        .map(|m| (m.key, m.program))
        .collect();
    let market_mints = |market: &Pubkey| -> Vec<Pubkey> { w.env.market(market).mints().to_vec() };
    let m1_mints = market_mints(&w.m1);
    let m2_mints = market_mints(&w.m2);
    let (m1, m2) = (w.m1, w.m2);
    // `TestRunner::run` takes an `Fn`, so the SVM is mutated through a `RefCell`.
    let cell = std::cell::RefCell::new(w);
    let outcomes = std::cell::Cell::new((0u32, 0u32));

    // (market index, fee authority: derived or the other market's, mint index,
    //  token program: mint's own or the other one, fee account: derived / other market's /
    //  non-ATA, treasury: derived / thief)
    let strategy = (
        0usize..2,
        any::<bool>(),
        0usize..4,
        prop_oneof![3 => Just(true), 1 => Just(false)],
        0usize..3,
        prop_oneof![3 => Just(true), 1 => Just(false)],
    );
    let mut runner = TestRunner::new(RunnerConfig {
        cases: 400,
        ..RunnerConfig::default()
    });
    runner
        .run(
            &strategy,
            |(mi, fa_derived, ki, own_program, fee_choice, treasury_derived)| {
                let mut w = cell.borrow_mut();
                let market = markets[mi];
                let other_market = markets[1 - mi];
                let (mint, mint_program) = mints[ki];
                let tp = if own_program {
                    mint_program
                } else if mint_program == TOKEN {
                    TOKEN_2022
                } else {
                    TOKEN
                };
                let fee_authority = if fa_derived {
                    pda::fee_authority(&market)
                } else {
                    pda::fee_authority(&other_market)
                };
                let derived_fees = pda::associated_token(&pda::fee_authority(&market), &mint, &tp);
                let fee_account = match fee_choice {
                    0 => derived_fees,
                    1 => pda::associated_token(&pda::fee_authority(&other_market), &mint, &tp),
                    _ => non_ata_fees,
                };
                let treasury = if treasury_derived {
                    pda::associated_token(&treasury_owner, &mint, &tp)
                } else {
                    thief_ata
                };

                // Refill every real fee account so a success has something to move.
                for (m, (k, p)) in [
                    (m1, mints[0]),
                    (m1, mints[1]),
                    (m2, mints[0]),
                    (m2, mints[2]),
                ] {
                    let account = pda::fee_account(&m, &k, &p);
                    if w.env.token_balance(&account) == 0 {
                        w.env.mint_to(&k, &p, &account, 1_000 + ki as u64);
                    }
                }

                let mut ix = client::sweep_fees(&market, &mint, &tp, &treasury_owner, vec![]);
                ix.accounts[FEE_AUTHORITY].pubkey = fee_authority;
                ix.accounts[FEE_ACCOUNT].pubkey = fee_account;
                ix.accounts[TREASURY].pubkey = treasury;

                let in_market = if mi == 0 { &m1_mints } else { &m2_mints }.contains(&mint);
                let expected_error = if !fa_derived {
                    Some("ConstraintSeeds")
                } else if !in_market {
                    Some("MintNotInMarket")
                } else if tp != mint_program {
                    Some("WrongTokenProgram")
                } else if fee_account != derived_fees {
                    Some("WrongFeeAccount")
                } else if !treasury_derived {
                    Some("WrongTreasuryAccount")
                } else {
                    None
                };

                let watched = [fee_account, treasury, non_ata_fees, thief_ata];
                let before: Vec<(Pubkey, Vec<u8>)> = w
                    .tracked()
                    .into_iter()
                    .chain(watched)
                    .map(|k| (k, w.env.account_data(&k)))
                    .collect();
                let a = w.env.token_balance(&fee_account);
                let (t0, h0) = (w.env.token_balance(&treasury), withheld(&w.env, &treasury));
                let result = w.env.send(&[ix], &[]);
                let after: Vec<(Pubkey, Vec<u8>)> = before
                    .iter()
                    .map(|(k, _)| (*k, w.env.account_data(k)))
                    .collect();
                match expected_error {
                    Some(error) => {
                        assert_fails_with(result, error);
                        prop_assert_eq!(after, before);
                        let (ok, err) = outcomes.get();
                        outcomes.set((ok, err + 1));
                    }
                    None => {
                        let meta =
                            result.map_err(|e| TestCaseError::fail(format!("{:?}", e.err)))?;
                        prop_assert!(a > 0);
                        prop_assert_eq!(w.env.token_balance(&fee_account), 0);
                        let gained = (w.env.token_balance(&treasury) - t0)
                            + (withheld(&w.env, &treasury) - h0);
                        prop_assert_eq!(gained, a);
                        prop_assert_eq!(swept_events(&meta)[0].amount, a);
                        assert_only_changed(&before, &after, &[fee_account, treasury]);
                        let (ok, err) = outcomes.get();
                        outcomes.set((ok + 1, err));
                    }
                }
                Ok(())
            },
        )
        .unwrap();
    let (ok, err) = outcomes.get();
    println!("random sweeps: {ok} succeeded, {err} refused");
    assert!(
        ok >= 3 && err >= 100,
        "the random cases must exercise both outcomes"
    );
}
