//! SOS audit #2, H012: the `swap` account-binding substitution matrix.
//!
//! Every account `swap` reads or pays into must be bound to the market: the market
//! (owner, discriminator, seeds from the passed strategy program and market state,
//! stored bump), the strategy program, the market state, both mints, both token
//! programs, the input vault, the fee authority and the fee account, the config, and
//! the trader's token accounts. The integrator account is free, but the token program
//! must still refuse one in the wrong mint or program.
//!
//! The world has three constant-product markets (two on the same SPL mints, one with a
//! Token-2022 input mint) and one market of the hostile `test-strategy-evil` program on
//! the same mints as the first. The evil strategy pays out 1:1 and never checks the
//! market PDA's signature, so if any binding were missing, a substituted swap through
//! it would succeed instead of failing in the router.
//!
//! Each case starts from a working swap instruction, replaces one or two accounts, and
//! asserts the expected error and that no watched balance moved. Controls before and
//! after the matrix show the unmodified swaps succeed.

#![allow(clippy::result_large_err)]

mod bok_swap_util;
mod common;

use bok_swap_util::*;
use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_keypair::Keypair;
use solana_signer::Signer;

const RESERVE: u64 = 1_000_000_000;
const AMOUNT: u64 = 1_000;

/// Positions of `swap`'s named accounts in `client::swap`'s instruction.
mod at {
    pub const CONFIG: usize = 1;
    pub const STRATEGY_PROGRAM: usize = 2;
    pub const MARKET: usize = 3;
    pub const MARKET_STATE: usize = 4;
    pub const TRADER_INPUT: usize = 5;
    pub const TRADER_OUTPUT: usize = 6;
    pub const INPUT_VAULT: usize = 7;
    pub const INPUT_MINT: usize = 8;
    pub const OUTPUT_MINT: usize = 9;
    pub const FEE_AUTHORITY: usize = 12;
    pub const FEE_ACCOUNT: usize = 13;
}

struct World {
    env: Env,
    /// CP market, SPL mint A → SPL mint B, 30 bps.
    m1: CpMarket,
    /// CP market on the same mints, 100 bps.
    m2: CpMarket,
    /// CP market, Token-2022 mint C → SPL mint D.
    m3: CpMarket,
    /// Evil-strategy market on mints A and B.
    e: EvilMarket,
    trader: Keypair,
    trader_a: Pubkey,
    trader_b: Pubkey,
    trader_c: Pubkey,
    trader_d: Pubkey,
    /// A mint-A account owned by someone other than the fee authority (the attacker's
    /// side account).
    side_a: Pubkey,
    victim_a: Pubkey,
    treasury_a: Pubkey,
    other_mint: Pubkey,
}

fn world() -> World {
    let mut env = Env::new();
    let m1 = setup_cp_market(&mut env, TOKEN, TOKEN, RESERVE, RESERVE, 30);
    let m2 = setup_cp_market_with_mints(
        &mut env,
        m1.mint_a,
        TOKEN,
        m1.mint_b,
        TOKEN,
        RESERVE,
        RESERVE,
        100,
        vec![],
    );
    let m3 = setup_cp_market(&mut env, TOKEN_2022, TOKEN, RESERVE, RESERVE, 30);
    let e = evil_market(&mut env, m1.mint_a, TOKEN, m1.mint_b, None, RESERVE);
    let (trader, trader_a) = env.trader_with(&m1.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &m1.mint_b, &TOKEN);
    let trader_c = env.create_ata(&trader.pubkey(), &m3.mint_a, &TOKEN_2022);
    env.mint_to(&m3.mint_a, &TOKEN_2022, &trader_c, 10_000_000);
    let trader_d = env.create_ata(&trader.pubkey(), &m3.mint_b, &TOKEN);
    let side_a = env.create_ata(&Pubkey::new_unique(), &m1.mint_a, &TOKEN);
    let (_victim, victim_a) = env.trader_with(&m1.mint_a, &TOKEN, 5_000_000);
    let treasury_owner = env.treasury_owner;
    let treasury_a = env.create_ata(&treasury_owner, &m1.mint_a, &TOKEN);
    let other_mint = env.create_mint(&TOKEN, 6, None, None);
    World {
        env,
        m1,
        m2,
        m3,
        e,
        trader,
        trader_a,
        trader_b,
        trader_c,
        trader_d,
        side_a,
        victim_a,
        treasury_a,
        other_mint,
    }
}

impl World {
    fn m1_accounts(&self) -> client::SwapAccounts {
        cp_swap_accounts(
            &self.m1,
            &self.trader.pubkey(),
            &self.m1.mint_a,
            &self.trader_a,
            &self.m1.mint_b,
            &self.trader_b,
        )
    }

    /// The working A → B swap on M1.
    fn m1_ix(&self) -> Instruction {
        client::swap(&self.m1_accounts(), &exact_in(AMOUNT, 0))
    }

    fn m1_ix_with(&self, index: usize, replacement: Pubkey) -> Instruction {
        let mut ix = self.m1_ix();
        ix.accounts[index].pubkey = replacement;
        ix
    }

    fn m3_accounts(&self) -> client::SwapAccounts {
        cp_swap_accounts(
            &self.m3,
            &self.trader.pubkey(),
            &self.m3.mint_a,
            &self.trader_c,
            &self.m3.mint_b,
            &self.trader_d,
        )
    }

    fn e_ix(&self) -> Instruction {
        client::swap(
            &evil_swap_accounts(
                &self.e,
                &self.trader.pubkey(),
                &self.trader_a,
                &self.trader_b,
            ),
            &evil_args(HONEST, AMOUNT, 0),
        )
    }

    /// Every token account a misbound swap could move.
    fn watched(&self) -> Vec<Pubkey> {
        let mut accounts = vec![
            self.trader_a,
            self.trader_b,
            self.trader_c,
            self.trader_d,
            self.side_a,
            self.victim_a,
            self.treasury_a,
        ];
        for m in [&self.m1, &self.m2, &self.m3] {
            accounts.extend([m.vault_a, m.vault_b]);
            accounts.push(pda::fee_account(&m.market, &m.mint_a, &m.token_program_a));
            accounts.push(pda::fee_account(&m.market, &m.mint_b, &m.token_program_b));
        }
        accounts.extend([self.e.vault_a, self.e.vault_b]);
        accounts.push(pda::fee_account(&self.e.market, &self.e.mint_a, &TOKEN));
        accounts.push(pda::fee_account(&self.e.market, &self.e.mint_b, &TOKEN));
        accounts
    }

    /// Sends `ix` and asserts it fails with `needle` (if given) and moves nothing.
    fn refused(&mut self, label: &str, ix: Instruction, needle: Option<&str>) {
        let watched = self.watched();
        let before = balances(&self.env, &watched);
        let trader = self.trader.insecure_clone();
        let result = send_swap(&mut self.env, ix, &trader);
        if let Ok(meta) = &result {
            panic!(
                "H012 BUG: substituted swap `{label}` succeeded; the router must refuse it:\n{}",
                meta.logs.join("\n")
            );
        }
        if let Some(needle) = needle {
            let failed = result.as_ref().err().unwrap();
            let logs = failed.meta.logs.join("\n");
            assert!(
                logs.contains(needle) || format!("{:?}", failed.err).contains(needle),
                "H012 `{label}`: expected failure containing {needle:?}, got {:?}:\n{logs}",
                failed.err
            );
        }
        assert_eq!(
            balances(&self.env, &watched),
            before,
            "H012 `{label}`: a balance moved"
        );
    }

    /// Sends `ix`, asserts it succeeds through `strategy` on `market`.
    fn succeeds(&mut self, label: &str, ix: Instruction, market: Pubkey, strategy: Pubkey) {
        let trader = self.trader.insecure_clone();
        let meta = send_swap(&mut self.env, ix, &trader)
            .unwrap_or_else(|e| panic!("control `{label}` must swap: {:?}", e.meta.logs));
        let (result, event) = reported(&meta);
        assert_eq!(event.market, market, "{label}: event market");
        assert_eq!(event.strategy, strategy, "{label}: event strategy");
        assert_eq!(result.net_sent, AMOUNT, "{label}: net_sent");
        // The strategy the router called (one level below the router) is the market's.
        let invoked = format!("Program {strategy} invoke [2]");
        assert!(
            meta.logs.iter().any(|line| line == &invoked),
            "{label}: strategy {strategy} was not the program called"
        );
    }

    fn controls(&mut self) {
        let (m1, m3, e) = (self.m1.market, self.m3.market, self.e.market);
        let ix = self.m1_ix();
        self.succeeds("M1 A→B", ix, m1, CP_PROGRAM_ID);
        let ix = client::swap(&self.m3_accounts(), &exact_in(AMOUNT, 0));
        self.succeeds("M3 C(T22)→D", ix, m3, CP_PROGRAM_ID);
        let ix = self.e_ix();
        self.succeeds("E A→B", ix, e, EVIL_ID);
    }
}

/// H012, market / strategy / market-state / config / fee-authority identity: every
/// substitution that would pair a strategy with another market's state, a market with
/// another strategy, a forged or wrong-type market, a forged config or another
/// market's fee authority fails in Anchor's account validation and moves nothing.
#[test]
fn sos2_h012_market_identity_substitutions_are_refused() {
    let mut w = world();
    w.controls();

    // Another strategy's program with this market and its state.
    let ix = w.m1_ix_with(at::STRATEGY_PROGRAM, EVIL_ID);
    w.refused("strategy_program = EVIL on M1", ix, Some("ConstraintSeeds"));

    // A consistent instruction for (EVIL, M1's state): no such market can exist.
    let mut accounts = w.m1_accounts();
    accounts.strategy_program = EVIL_ID;
    let ix = client::swap(&accounts, &exact_in(AMOUNT, 0));
    w.refused(
        "market = PDA(EVIL, M1 state)",
        ix,
        Some("AccountNotInitialized"),
    );

    // This strategy with another market's state, keeping M1.
    let ix = w.m1_ix_with(at::MARKET_STATE, w.m2.pool);
    w.refused("market_state = M2 pool on M1", ix, Some("ConstraintSeeds"));

    // Another market of the same strategy, keeping M1's state.
    let ix = w.m1_ix_with(at::MARKET, w.m2.market);
    w.refused("market = M2 with M1 state", ix, Some("ConstraintSeeds"));

    // The evil market with CP as its strategy program.
    let mut ix = w.e_ix();
    ix.accounts[at::STRATEGY_PROGRAM].pubkey = CP_PROGRAM_ID;
    w.refused("E with strategy_program = CP", ix, Some("ConstraintSeeds"));

    // The evil market with the CP pool as its state.
    let mut ix = w.e_ix();
    ix.accounts[at::MARKET_STATE].pubkey = w.m1.pool;
    w.refused("E with market_state = M1 pool", ix, Some("ConstraintSeeds"));

    // M1 under EVIL with E's state (and the reverse): the evil program can't act as
    // M1's strategy, nor CP as E's.
    let mut ix = w.e_ix();
    ix.accounts[at::MARKET].pubkey = w.m1.market;
    w.refused(
        "market = M1 under EVIL/E state",
        ix,
        Some("ConstraintSeeds"),
    );

    // A byte-for-byte copy of M1's market account at another address, owned by the
    // router (crafted: nobody else can make one) and by another program.
    let real = w.env.svm.get_account(&w.m1.market).unwrap();
    let router_copy = Pubkey::new_unique();
    w.env.svm.set_account(router_copy, real.clone()).unwrap();
    let ix = w.m1_ix_with(at::MARKET, router_copy);
    w.refused("market = router-owned copy", ix, Some("ConstraintSeeds"));
    let mut foreign = real.clone();
    foreign.owner = EVIL_ID;
    let foreign_copy = Pubkey::new_unique();
    w.env.svm.set_account(foreign_copy, foreign).unwrap();
    let ix = w.m1_ix_with(at::MARKET, foreign_copy);
    w.refused(
        "market = EVIL-owned copy",
        ix,
        Some("AccountOwnedByWrongProgram"),
    );

    // Router accounts of the wrong type in the market slot.
    let ix = w.m1_ix_with(at::MARKET, pda::config());
    w.refused(
        "market = config PDA",
        ix,
        Some("AccountDiscriminatorMismatch"),
    );
    let ix = w.m1_ix_with(at::MARKET, pda::strategy(&CP_PROGRAM_ID));
    w.refused(
        "market = strategy PDA",
        ix,
        Some("AccountDiscriminatorMismatch"),
    );

    // Config substitutions.
    let ix = w.m1_ix_with(at::CONFIG, pda::strategy(&CP_PROGRAM_ID));
    w.refused(
        "config = strategy PDA",
        ix,
        Some("AccountDiscriminatorMismatch"),
    );
    let mut config = w.env.svm.get_account(&pda::config()).unwrap();
    let config_copy = Pubkey::new_unique();
    w.env.svm.set_account(config_copy, config.clone()).unwrap();
    let ix = w.m1_ix_with(at::CONFIG, config_copy);
    w.refused("config = router-owned copy", ix, Some("ConstraintSeeds"));
    config.owner = EVIL_ID;
    let foreign_config = Pubkey::new_unique();
    w.env.svm.set_account(foreign_config, config).unwrap();
    let ix = w.m1_ix_with(at::CONFIG, foreign_config);
    w.refused(
        "config = EVIL-owned copy",
        ix,
        Some("AccountOwnedByWrongProgram"),
    );

    // Another market's fee authority together with that market's fee account.
    let mut ix = w.m1_ix_with(at::FEE_AUTHORITY, pda::fee_authority(&w.m2.market));
    ix.accounts[at::FEE_ACCOUNT].pubkey = pda::fee_account(&w.m2.market, &w.m1.mint_a, &TOKEN);
    w.refused("fee_authority = M2's", ix, Some("ConstraintSeeds"));

    w.controls();
}

/// H012, mints and token programs: an input or output mint outside the market, the
/// same mint on both sides, a Token-2022 mint with SPL Token as its program and the
/// reverse, and a token-program slot holding a non-token program all fail.
#[test]
fn sos2_h012_mint_and_token_program_substitutions_are_refused() {
    let mut w = world();
    w.controls();

    let ix = w.m1_ix_with(at::INPUT_MINT, w.other_mint);
    w.refused("input_mint not in market", ix, Some("MintNotInMarket"));
    let ix = w.m1_ix_with(at::OUTPUT_MINT, w.other_mint);
    w.refused("output_mint not in market", ix, Some("MintNotInMarket"));
    let ix = w.m1_ix_with(at::INPUT_MINT, w.m3.mint_a);
    w.refused("input_mint = M3's T22 mint", ix, Some("MintNotInMarket"));
    let ix = w.m1_ix_with(at::INPUT_MINT, w.m1.mint_b);
    w.refused("input_mint = output_mint", ix, Some("SameMint"));

    // Token-2022 input mint with SPL Token as the input program.
    let mut accounts = w.m3_accounts();
    accounts.input_token_program = TOKEN;
    let ix = client::swap(&accounts, &exact_in(AMOUNT, 0));
    w.refused("T22 input mint, SPL program", ix, Some("WrongTokenProgram"));

    // SPL input mint with Token-2022 as the input program.
    let mut accounts = w.m1_accounts();
    accounts.input_token_program = TOKEN_2022;
    let ix = client::swap(&accounts, &exact_in(AMOUNT, 0));
    w.refused("SPL input mint, T22 program", ix, Some("WrongTokenProgram"));

    // SPL output mint with Token-2022 as the output program, and the reverse on M3
    // (B → A direction there has the T22 mint as output).
    let mut accounts = w.m1_accounts();
    accounts.output_token_program = TOKEN_2022;
    let ix = client::swap(&accounts, &exact_in(AMOUNT, 0));
    w.refused(
        "SPL output mint, T22 program",
        ix,
        Some("WrongTokenProgram"),
    );
    let mut accounts = cp_swap_accounts(
        &w.m3,
        &w.trader.pubkey(),
        &w.m3.mint_b,
        &w.trader_d,
        &w.m3.mint_a,
        &w.trader_c,
    );
    accounts.output_token_program = TOKEN;
    let ix = client::swap(&accounts, &exact_in(AMOUNT, 0));
    w.refused(
        "T22 output mint, SPL program",
        ix,
        Some("WrongTokenProgram"),
    );

    // Non-token programs in the token-program slots (an attacker program that would
    // otherwise receive the trader's signature through the input transfer).
    for (label, program) in [
        ("input program = EVIL", EVIL_ID),
        ("input program = CP", CP_PROGRAM_ID),
        ("input program = system", client::SYSTEM_PROGRAM_ID),
    ] {
        let mut accounts = w.m1_accounts();
        accounts.input_token_program = program;
        let ix = client::swap(&accounts, &exact_in(AMOUNT, 0));
        w.refused(label, ix, Some("WrongTokenProgram"));
        let mut accounts = w.m1_accounts();
        accounts.output_token_program = program;
        let ix = client::swap(&accounts, &exact_in(AMOUNT, 0));
        w.refused(label, ix, Some("WrongTokenProgram"));
    }

    w.controls();
}

/// H012, input vault and fee account: the other mint's vault, another market's vault
/// in the same mint (same strategy and different strategy), the trader's own account,
/// and every wrong fee account (the other token program's ATA, another market's,
/// the evil market's, the treasury's, the output mint's, an attacker account, the
/// trader's input) fail with `WrongVault` / `WrongFeeAccount`.
#[test]
fn sos2_h012_vault_and_fee_account_substitutions_are_refused() {
    let mut w = world();
    w.controls();

    for (label, vault) in [
        ("vault = vaults[other] (M1 B)", w.m1.vault_b),
        ("vault = M2's vault A", w.m2.vault_a),
        ("vault = E's vault A", w.e.vault_a),
        ("vault = attacker side account", w.side_a),
        (
            "vault = M1 fee ATA",
            pda::fee_account(&w.m1.market, &w.m1.mint_a, &TOKEN),
        ),
    ] {
        let ix = w.m1_ix_with(at::INPUT_VAULT, vault);
        w.refused(label, ix, Some("WrongVault"));
    }

    // On the evil market, M1's vault A (same mint) is not E's declared vault.
    let mut ix = w.e_ix();
    ix.accounts[at::INPUT_VAULT].pubkey = w.m1.vault_a;
    w.refused("E with vault = M1's vault A", ix, Some("WrongVault"));

    let wrong_fee_accounts = [
        (
            "fee = M1 fee authority's T22 ATA",
            pda::fee_account(&w.m1.market, &w.m1.mint_a, &TOKEN_2022),
        ),
        (
            "fee = M2's fee ATA",
            pda::fee_account(&w.m2.market, &w.m1.mint_a, &TOKEN),
        ),
        (
            "fee = E's fee ATA",
            pda::fee_account(&w.e.market, &w.m1.mint_a, &TOKEN),
        ),
        ("fee = treasury ATA", w.treasury_a),
        (
            "fee = M1 output-mint fee ATA",
            pda::fee_account(&w.m1.market, &w.m1.mint_b, &TOKEN),
        ),
        ("fee = attacker side account", w.side_a),
        ("fee = trader_input", w.trader_a),
        ("fee = M1 vault A", w.m1.vault_a),
    ];
    for (label, fee) in wrong_fee_accounts {
        let ix = w.m1_ix_with(at::FEE_ACCOUNT, fee);
        w.refused(label, ix, Some("WrongFeeAccount"));
    }

    // On the evil market, M1's fee ATA in the same mint is not E's.
    let mut ix = w.e_ix();
    ix.accounts[at::FEE_ACCOUNT].pubkey = pda::fee_account(&w.m1.market, &w.m1.mint_a, &TOKEN);
    w.refused("E with fee = M1's fee ATA", ix, Some("WrongFeeAccount"));

    w.controls();
}

/// H012, the trader's accounts and the integrator account: a trader account in the
/// wrong mint or program, someone else's account as `trader_input`, and an integrator
/// account in the wrong mint or token program all fail and move nothing. (The crafted
/// owner / length / type-byte variants are `bok_inv_adm_11_*` in `bok_swap_accounts.rs`.)
#[test]
fn sos2_h012_trader_and_integrator_substitutions_are_refused() {
    let mut w = world();
    w.controls();

    let ix = w.m1_ix_with(at::TRADER_INPUT, w.trader_b);
    w.refused("trader_input in output mint", ix, Some("BadTokenAccount"));
    let ix = w.m1_ix_with(at::TRADER_INPUT, w.trader_c);
    w.refused("trader_input = T22 account", ix, Some("BadTokenAccount"));
    let ix = w.m1_ix_with(at::TRADER_OUTPUT, w.trader_a);
    w.refused("trader_output in input mint", ix, Some("BadTokenAccount"));
    let ix = w.m1_ix_with(at::TRADER_OUTPUT, w.trader_c);
    w.refused("trader_output = T22 account", ix, Some("BadTokenAccount"));

    // Someone else's mint-A account as the source: the trader signs, but isn't its
    // owner, so SPL Token refuses the input transfer (OwnerMismatch, 0x4).
    let ix = w.m1_ix_with(at::TRADER_INPUT, w.victim_a);
    w.refused("trader_input = victim's account", ix, Some("0x4"));

    // Integrator accounts in the wrong mint / token program.
    let mut args = exact_in(AMOUNT, 0);
    args.integrator_fee_bps = 100;
    for (label, integrator) in [
        ("integrator in output mint", w.trader_b),
        ("integrator = T22 account", w.trader_c),
    ] {
        let mut accounts = w.m1_accounts();
        accounts.integrator_fee_account = Some(integrator);
        let ix = client::swap(&accounts, &args);
        w.refused(label, ix, None);
    }

    // A crafted copy of a mint-A account owned by Token-2022: the SPL Token transfer
    // into it must fail.
    let mut wrong_program = w.env.svm.get_account(&w.side_a).unwrap();
    wrong_program.owner = TOKEN_2022;
    let wrong_program_address = Pubkey::new_unique();
    w.env
        .svm
        .set_account(wrong_program_address, wrong_program)
        .unwrap();
    let mut accounts = w.m1_accounts();
    accounts.integrator_fee_account = Some(wrong_program_address);
    let ix = client::swap(&accounts, &args);
    w.refused("integrator owned by T22, mint-A bytes", ix, None);
    assert_eq!(
        w.env.token_balance(&wrong_program_address),
        0,
        "H012: the crafted integrator account was credited"
    );

    // A fee rate with no integrator account.
    let ix = client::swap(&w.m1_accounts(), &args);
    w.refused(
        "integrator bps without account",
        ix,
        Some("MissingIntegratorFeeAccount"),
    );

    // The integrator slot can't be used to sneak in the trader's input account.
    let mut accounts = w.m1_accounts();
    accounts.integrator_fee_account = Some(w.trader_a);
    let ix = client::swap(&accounts, &args);
    w.refused("integrator = trader_input", ix, Some("FeeToTraderInput"));

    // A valid integrator account still works (control for the slot itself).
    let mut accounts = w.m1_accounts();
    accounts.integrator_fee_account = Some(w.side_a);
    let ix = client::swap(&accounts, &args);
    let before = w.env.token_balance(&w.side_a);
    let trader = w.trader.insecure_clone();
    let meta = send_swap(&mut w.env, ix, &trader).expect("integrator control swap");
    let (result, _) = reported(&meta);
    assert_eq!(result.integrator_fee, 10);
    assert_eq!(w.env.token_balance(&w.side_a), before + 10);

    w.controls();
}

/// H012, creation side of the binding: a market for (strategy X, state owned by Y)
/// can't be recorded, so the PDA a mismatched swap would need never exists; and an
/// existing market can't be re-recorded.
#[test]
fn sos2_h012_markets_cannot_be_recorded_for_another_strategys_state() {
    let mut w = world();
    let payer = w.env.funded_keypair();
    let evil_info = vec![
        AccountMeta::new_readonly(w.m1.mint_a, false),
        AccountMeta::new_readonly(w.m1.mint_b, false),
    ];

    let ix =
        client::register_existing_market(&payer.pubkey(), &EVIL_ID, &w.m1.pool, evil_info.clone());
    assert_fails_with(
        w.env.send(&[ix], &[&payer]),
        "MarketStateNotOwnedByStrategy",
    );
    let ix = client::register_existing_market(&payer.pubkey(), &CP_PROGRAM_ID, &w.e.state, vec![]);
    assert_fails_with(
        w.env.send(&[ix], &[&payer]),
        "MarketStateNotOwnedByStrategy",
    );

    // create_market under EVIL with the CP pool as the state.
    let ix = client::create_market(&client::CreateMarket {
        creator: payer.pubkey(),
        strategy_program: EVIL_ID,
        market_state: w.m1.pool,
        market_state_signs: false,
        init_data: vec![0],
        max_creator_lamports: MAX_SETUP_LAMPORTS,
        strategy_accounts: vec![
            AccountMeta::new_readonly(w.m1.mint_a, false),
            AccountMeta::new_readonly(w.m1.mint_b, false),
            AccountMeta::new(w.e.vault_a, false),
            AccountMeta::new(w.e.vault_b, false),
        ],
    });
    assert!(
        w.env.send(&[ix], &[&payer]).is_err(),
        "H012: create_market(EVIL, CP pool) must fail"
    );
    assert!(
        w.env
            .svm
            .get_account(&pda::market(&EVIL_ID, &w.m1.pool))
            .is_none_or(|a| a.lamports == 0),
        "H012: a market PDA for (EVIL, CP pool) exists"
    );

    // Re-recording an existing market is refused (the market PDA is `init`).
    let ix = client::register_existing_market(&payer.pubkey(), &EVIL_ID, &w.e.state, evil_info);
    assert_fails_with(w.env.send(&[ix], &[&payer]), "already in use");

    // The stored binding equals the seeds the swap derives it from.
    for (market, program, state) in [
        (w.m1.market, CP_PROGRAM_ID, w.m1.pool),
        (w.m2.market, CP_PROGRAM_ID, w.m2.pool),
        (w.m3.market, CP_PROGRAM_ID, w.m3.pool),
        (w.e.market, EVIL_ID, w.e.state),
    ] {
        let stored = w.env.market(&market);
        assert_eq!(stored.strategy_program, program);
        assert_eq!(stored.market_state, state);
        assert_eq!(pda::market(&program, &state), market);
    }
}
