//! SOS audit #2, H011: cross-market isolation.
//!
//! A hostile market M1 (the evil strategy) shares mint X with a victim market M2 (the
//! CP strategy). During M1's own swap, M1's strategy holds M1's market-PDA signature and
//! any writable accounts the caller hands it. These tests check it can't use either on
//! M2's fee accounts, vaults, `Market` record, the treasury or the config:
//!   * `StealFees` (transfer out of a chosen account, signed as M1's market PDA) against
//!     M2's fee account, M2's vault and the treasury account fails, and every M2 account
//!     is byte-identical afterwards, even when all of them were passed writable;
//!   * the market-PDA capability is scoped: it moves tokens from an account M1's PDA owns
//!     (a third party made it), not from one M2's PDA owns, and router-created accounts
//!     are never owned by, or delegated to, a market PDA;
//!   * a Token-2022 permanent delegate set to M1's PDA (an issuer power, accepted design)
//!     reaches M2's fee account in that token; set to M2's PDA it gives M1 nothing;
//!   * fee accounts, fee authorities and the treasury account are bound per market in both
//!     `swap` and `sweep_fees`, and fee account addresses never collide across markets or
//!     token programs;
//!   * a hostile market declaring M2's fee account as its own vault only donates to it.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

/// Evil strategy swap behaviours (programs/test-strategy-evil/src/lib.rs).
const HONEST: u8 = 0;
const REFUND_MORE: u8 = 3;
const STEAL_FEES: u8 = 6;

const AMOUNT: u64 = 1_000_000;
const OWNER_MISMATCH: &str = "owner does not match";

struct EvilMarket {
    state: Pubkey,
    market: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    tp_a: Pubkey,
    tp_b: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
}

fn evil_state(mint_a: &Pubkey, mint_b: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0
}

/// Creates an evil market on (mint_a, mint_b) with fee accounts and B liquidity. If
/// `vault_a` is given, the market declares that account as its A vault instead of the
/// state's own associated token account.
fn evil_market(
    env: &mut Env,
    mint_a: Pubkey,
    tp_a: Pubkey,
    mint_b: Pubkey,
    tp_b: Pubkey,
    vault_a: Option<Pubkey>,
) -> EvilMarket {
    if env.svm.get_account(&pda::strategy(&EVIL_ID)).is_none() {
        env.register(&EVIL_ID, "test_strategy_evil");
    }
    let state = evil_state(&mint_a, &mint_b);
    let vault_a = vault_a.unwrap_or_else(|| env.create_ata(&state, &mint_a, &tp_a));
    let vault_b = env.create_ata(&state, &mint_b, &tp_b);
    let creator = env.funded_keypair();
    let create = client::create_market(&client::CreateMarket {
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
    env.send(&[create], &[&creator])
        .expect("create evil market");
    let market = pda::market(&EVIL_ID, &state);
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &market, &mint_a, &tp_a),
            client::init_fee_account(&payer, &market, &mint_b, &tp_b),
        ],
        &[],
    )
    .expect("init evil fee accounts");
    env.mint_to(&mint_b, &tp_b, &vault_b, 1_000_000_000_000);
    EvilMarket {
        state,
        market,
        mint_a,
        mint_b,
        tp_a,
        tp_b,
        vault_a,
        vault_b,
    }
}

/// Swap A -> B on the evil market. The first strategy account is B's vault (the evil
/// strategy pays from it); `extra` follows, and `extra[0]` is `StealFees`' source.
fn evil_swap(
    m: &EvilMarket,
    trader: &Pubkey,
    trader_a: &Pubkey,
    trader_b: &Pubkey,
    behaviour: u8,
    extra: Vec<AccountMeta>,
) -> Instruction {
    let mut strategy_accounts = vec![AccountMeta::new(m.vault_b, false)];
    strategy_accounts.extend(extra);
    let accounts = client::SwapAccounts {
        trader: *trader,
        strategy_program: EVIL_ID,
        market_state: m.state,
        trader_input: *trader_a,
        trader_output: *trader_b,
        input_vault: m.vault_a,
        input_mint: m.mint_a,
        output_mint: m.mint_b,
        input_token_program: m.tp_a,
        output_token_program: m.tp_b,
        integrator_fee_account: None,
        strategy_accounts,
        input_hook_accounts: vec![],
    };
    let mut args = exact_in(AMOUNT, 0);
    args.extra = vec![behaviour];
    client::swap(&accounts, &args)
}

/// A CP swap of `amount` A into the victim market, so it collects fees in A.
fn cp_trade_a(env: &mut Env, market: &CpMarket, amount: u64) {
    let tp_a = market.token_program_a;
    let tp_b = market.token_program_b;
    let (trader, trader_a) = env.trader_with(&market.mint_a, &tp_a, amount * 2);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &tp_b);
    let accounts = cp_swap_accounts(
        market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    env.send(&[client::swap(&accounts, &exact_in(amount, 0))], &[&trader])
        .expect("cp swap");
}

fn snapshot(env: &Env, keys: &[Pubkey]) -> Vec<(Pubkey, Option<Account>)> {
    keys.iter()
        .map(|key| (*key, env.svm.get_account(key)))
        .collect()
}

fn token_owner(env: &Env, account: &Pubkey) -> Pubkey {
    Pubkey::new_from_array(env.account_data(account)[32..64].try_into().unwrap())
}

/// A Token-2022 mint (6 decimals, admin mint authority) whose permanent delegate is
/// `delegate`, at the address of `mint`.
fn permanent_delegate_mint(env: &mut Env, mint: &Keypair, delegate: &Pubkey) {
    use spl_token_2022_interface::extension::ExtensionType;
    let space =
        ExtensionType::try_calculate_account_len::<spl_token_2022_interface::state::Mint>(&[
            ExtensionType::PermanentDelegate,
        ])
        .unwrap();
    let admin = env.admin.pubkey();
    let rent = env.svm.minimum_balance_for_rent_exemption(space);
    env.send(
        &[
            solana_system_interface::instruction::create_account(
                &admin,
                &mint.pubkey(),
                rent,
                space as u64,
                &TOKEN_2022,
            ),
            spl_token_2022_interface::instruction::initialize_permanent_delegate(
                &TOKEN_2022,
                &mint.pubkey(),
                delegate,
            )
            .unwrap(),
            spl_token_2022_interface::instruction::initialize_mint2(
                &TOKEN_2022,
                &mint.pubkey(),
                &admin,
                None,
                6,
            )
            .unwrap(),
        ],
        &[mint],
    )
    .expect("permanent-delegate mint");
}

/// Victim CP market M2 on (X, Y) with fees in X both in its fee account and already swept
/// to the treasury, and a hostile evil market M1 on (X, Z).
struct World {
    env: Env,
    m2: CpMarket,
    m1: EvilMarket,
    m2_fee_x: Pubkey,
    m2_fee_y: Pubkey,
    treasury_x: Pubkey,
}

fn world() -> World {
    let mut env = Env::new();
    let x = env.create_mint(&TOKEN, 6, None, None);
    let y = env.create_mint(&TOKEN, 6, None, None);
    let z = env.create_mint(&TOKEN, 6, None, None);
    let m2 = setup_cp_market_with_mints(
        &mut env,
        x,
        TOKEN,
        y,
        TOKEN,
        1_000_000_000_000,
        1_000_000_000_000,
        30,
        vec![],
    );
    let m2_fee_x = pda::fee_account(&m2.market, &x, &TOKEN);
    let m2_fee_y = pda::fee_account(&m2.market, &y, &TOKEN);
    let treasury_owner = env.treasury_owner;
    let treasury_x = env.create_ata(&treasury_owner, &x, &TOKEN);

    // Fees in X: one swap swept to the treasury, one left in M2's fee account.
    cp_trade_a(&mut env, &m2, AMOUNT);
    env.send(
        &[client::sweep_fees(
            &m2.market,
            &x,
            &TOKEN,
            &treasury_owner,
            vec![],
        )],
        &[],
    )
    .expect("sweep M2");
    cp_trade_a(&mut env, &m2, AMOUNT);
    assert!(
        env.token_balance(&treasury_x) > 0,
        "setup: treasury holds X"
    );
    assert!(
        env.token_balance(&m2_fee_x) > 0,
        "setup: M2 fee account holds X"
    );

    let m1 = evil_market(&mut env, x, TOKEN, z, TOKEN, None);
    World {
        env,
        m2,
        m1,
        m2_fee_x,
        m2_fee_y,
        treasury_x,
    }
}

impl World {
    /// Every account belonging to M2, the treasury account and the router's records.
    fn victim_accounts(&self) -> Vec<Pubkey> {
        vec![
            self.m2.market,
            self.m2.pool,
            self.m2.vault_a,
            self.m2.vault_b,
            self.m2_fee_x,
            self.m2_fee_y,
            pda::fee_authority(&self.m2.market),
            self.treasury_x,
            pda::config(),
            pda::strategy(&CP_PROGRAM_ID),
        ]
    }

    fn trader(&mut self) -> (Keypair, Pubkey, Pubkey) {
        let (trader, trader_x) = self.env.trader_with(&self.m1.mint_a, &TOKEN, 10 * AMOUNT);
        let trader_z = self
            .env
            .create_ata(&trader.pubkey(), &self.m1.mint_b, &TOKEN);
        (trader, trader_x, trader_z)
    }
}

/// (a) + (c): M1's strategy, signing as M1's market PDA, can't move M2's fee account, M2's
/// vault or the treasury account, even with every M2 account, the config and the CP
/// strategy record passed writable. A control swap with the same writable accounts
/// succeeds and leaves all of them byte-identical.
#[test]
fn hostile_market_cannot_touch_another_markets_fees_vaults_records_or_treasury() {
    let mut w = world();
    let (trader, trader_x, trader_z) = w.trader();
    let victims = w.victim_accounts();
    let before = snapshot(&w.env, &victims);
    let all_writable: Vec<AccountMeta> = victims
        .iter()
        .map(|key| AccountMeta::new(*key, false))
        .collect();

    for target in [w.m2_fee_x, w.m2.vault_a, w.treasury_x] {
        let mut extra = vec![AccountMeta::new(target, false)];
        extra.extend(all_writable.iter().filter(|m| m.pubkey != target).cloned());
        let ix = evil_swap(
            &w.m1,
            &trader.pubkey(),
            &trader_x,
            &trader_z,
            STEAL_FEES,
            extra,
        );
        assert_fails_with(w.env.send(&[ix], &[&trader]), OWNER_MISMATCH);
        assert_eq!(
            snapshot(&w.env, &victims),
            before,
            "H011: StealFees on {target} changed a victim account"
        );
    }

    // Control: writability alone grants nothing. The honest swap goes through with every
    // victim account writable in the strategy's accounts, and none of them changes.
    let ix = evil_swap(
        &w.m1,
        &trader.pubkey(),
        &trader_x,
        &trader_z,
        HONEST,
        all_writable.clone(),
    );
    w.env.send(&[ix], &[&trader]).expect("control swap on M1");
    assert_eq!(
        w.env.token_balance(&trader_z),
        AMOUNT,
        "control swap paid out"
    );
    assert_eq!(
        snapshot(&w.env, &victims),
        before,
        "H011: a swap on M1 changed an M2, treasury or router account"
    );
}

/// (b): the market-PDA signature works only on what that PDA owns. A third party can give
/// M1's PDA a token account, and M1's strategy can move it; the same signature can't move
/// an account owned by M2's PDA. Router-created accounts are never owned by, or delegated
/// to, a market PDA.
#[test]
fn market_pda_capability_is_scoped_to_its_own_market() {
    let mut w = world();
    let (trader, trader_x, trader_z) = w.trader();
    let x = w.m1.mint_a;

    // Third-party token accounts owned by each market PDA.
    let m1_owned = w.env.create_ata(&w.m1.market, &x, &TOKEN);
    let m2_owned = w.env.create_ata(&w.m2.market, &x, &TOKEN);
    w.env.mint_to(&x, &TOKEN, &m1_owned, 10);
    w.env.mint_to(&x, &TOKEN, &m2_owned, 10);

    let ix = evil_swap(
        &w.m1,
        &trader.pubkey(),
        &trader_x,
        &trader_z,
        STEAL_FEES,
        vec![AccountMeta::new(m2_owned, false)],
    );
    assert_fails_with(w.env.send(&[ix], &[&trader]), OWNER_MISMATCH);
    assert_eq!(
        w.env.token_balance(&m2_owned),
        10,
        "H011: M1 moved M2-PDA tokens"
    );

    // Control: M1's signature is real, and works on M1's own account.
    let ix = evil_swap(
        &w.m1,
        &trader.pubkey(),
        &trader_x,
        &trader_z,
        STEAL_FEES,
        vec![AccountMeta::new(m1_owned, false)],
    );
    w.env
        .send(&[ix], &[&trader])
        .expect("M1 moves M1-PDA tokens");
    assert_eq!(w.env.token_balance(&m1_owned), 9);

    // Router-created accounts: `Market` records are router-owned data accounts, and fee
    // accounts are owned by the separate `["fees", market]` PDA with no delegate or close
    // authority.
    for market in [w.m1.market, w.m2.market] {
        let account = w.env.svm.get_account(&market).unwrap();
        assert_eq!(account.owner, client::PROGRAM_ID);
    }
    let fee_accounts = [
        (w.m2.market, w.m2_fee_x),
        (w.m2.market, w.m2_fee_y),
        (w.m1.market, pda::fee_account(&w.m1.market, &x, &TOKEN)),
        (
            w.m1.market,
            pda::fee_account(&w.m1.market, &w.m1.mint_b, &TOKEN),
        ),
    ];
    for (market, fee_account) in fee_accounts {
        let data = w.env.account_data(&fee_account);
        assert_eq!(
            token_owner(&w.env, &fee_account),
            pda::fee_authority(&market)
        );
        assert_ne!(pda::fee_authority(&market), market);
        assert_eq!(&data[72..76], &[0; 4], "fee account has no delegate");
        assert_eq!(
            &data[129..133],
            &[0; 4],
            "fee account has no close authority"
        );
    }
}

/// (b), issuer power: a Token-2022 mint X whose permanent delegate is M1's market PDA lets
/// M1's strategy move X out of M2's fee account. This is the issuer's own power over its
/// own token (accepted design): it reaches every X account, not just Pipeline's. With the
/// delegate set to M2's market PDA instead, M1 gets nothing.
#[test]
fn permanent_delegate_on_a_market_pda_is_an_issuer_power_only() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let y = env.create_mint(&TOKEN, 6, None, None);
    let z = env.create_mint(&TOKEN, 6, None, None);

    // X's delegate is M1's market PDA; X2's delegate is M2b's (the CP market's) PDA.
    let x = Keypair::new();
    let m1_market = pda::market(&EVIL_ID, &evil_state(&x.pubkey(), &z));
    permanent_delegate_mint(&mut env, &x, &m1_market);
    let x2 = Keypair::new();
    let m2b_market = pda::market(&CP_PROGRAM_ID, &cp_pool(&x2.pubkey(), &y, 30));
    permanent_delegate_mint(&mut env, &x2, &m2b_market);
    let (x, x2) = (x.pubkey(), x2.pubkey());

    let mut victims = vec![];
    for mint in [x, x2] {
        let m2 = setup_cp_market_with_mints(
            &mut env,
            mint,
            TOKEN_2022,
            y,
            TOKEN,
            1_000_000_000_000,
            1_000_000_000_000,
            30,
            vec![],
        );
        cp_trade_a(&mut env, &m2, AMOUNT);
        let fee = pda::fee_account(&m2.market, &mint, &TOKEN_2022);
        assert!(env.token_balance(&fee) > 0, "setup: M2 fee account holds X");
        let m1 = evil_market(&mut env, mint, TOKEN_2022, z, TOKEN, None);
        victims.push((m1, fee));
    }
    assert_eq!(victims[0].0.market, m1_market);

    for (i, (m1, fee)) in victims.iter().enumerate() {
        let (trader, trader_x) = env.trader_with(&m1.mint_a, &TOKEN_2022, 10 * AMOUNT);
        let trader_z = env.create_ata(&trader.pubkey(), &z, &TOKEN);
        let fee_before = env.token_balance(fee);
        let ix = evil_swap(
            m1,
            &trader.pubkey(),
            &trader_x,
            &trader_z,
            STEAL_FEES,
            vec![AccountMeta::new(*fee, false)],
        );
        let result = env.send(&[ix], &[&trader]);
        if i == 0 {
            // Delegate = M1's PDA: the issuer's power, used by the market it chose.
            result.expect("permanent delegate = M1 PDA moves X");
            assert_eq!(env.token_balance(fee), fee_before - 1);
        } else {
            // Delegate = M2's PDA: M1's signature is not the delegate.
            assert_fails_with(result, OWNER_MISMATCH);
            assert_eq!(
                env.token_balance(fee),
                fee_before,
                "H011: M1 moved X2 out of M2's fee account"
            );
        }
    }
}

/// (d): fee accounts, fee authorities and the treasury account are bound per market in
/// `swap` and `sweep_fees`; addresses don't collide across markets or token programs.
#[test]
fn fee_accounts_and_authorities_are_bound_per_market() {
    let mut w = world();
    let x = w.m1.mint_a;
    let m1_fee_x = pda::fee_account(&w.m1.market, &x, &TOKEN);
    let treasury_owner = w.env.treasury_owner;

    // Distinct addresses: per market, per token program, and from the treasury and vaults.
    let addresses = [
        m1_fee_x,
        w.m2_fee_x,
        pda::fee_account(&w.m1.market, &x, &TOKEN_2022),
        pda::fee_account(&w.m2.market, &x, &TOKEN_2022),
        pda::fee_authority(&w.m1.market),
        pda::fee_authority(&w.m2.market),
        w.treasury_x,
        pda::associated_token(&treasury_owner, &x, &TOKEN_2022),
        w.m1.vault_a,
        w.m2.vault_a,
        w.m1.market,
        w.m2.market,
    ];
    for (i, a) in addresses.iter().enumerate() {
        for b in &addresses[i + 1..] {
            assert_ne!(a, b, "H011: address collision");
        }
    }

    let victims = w.victim_accounts();
    let before = snapshot(&w.env, &victims);

    // sweep_fees(M1) pointed at M2's fee account.
    let mut ix = client::sweep_fees(&w.m1.market, &x, &TOKEN, &treasury_owner, vec![]);
    ix.accounts[4].pubkey = w.m2_fee_x;
    assert_fails_with(w.env.send(&[ix], &[]), "WrongFeeAccount");
    // sweep_fees(M1) with M2's fee authority and fee account.
    let mut ix = client::sweep_fees(&w.m1.market, &x, &TOKEN, &treasury_owner, vec![]);
    ix.accounts[2].pubkey = pda::fee_authority(&w.m2.market);
    ix.accounts[4].pubkey = w.m2_fee_x;
    assert_fails_with(w.env.send(&[ix], &[]), "ConstraintSeeds");
    // sweep_fees(M2) into M1's fee account instead of the treasury.
    let mut ix = client::sweep_fees(&w.m2.market, &x, &TOKEN, &treasury_owner, vec![]);
    ix.accounts[5].pubkey = m1_fee_x;
    assert_fails_with(w.env.send(&[ix], &[]), "WrongTreasuryAccount");

    // A swap on M1 that names M2's fee account (alone, or with M2's fee authority).
    let (trader, trader_x, trader_z) = w.trader();
    let mut ix = evil_swap(
        &w.m1,
        &trader.pubkey(),
        &trader_x,
        &trader_z,
        HONEST,
        vec![],
    );
    ix.accounts[13].pubkey = w.m2_fee_x;
    assert_fails_with(w.env.send(&[ix], &[&trader]), "WrongFeeAccount");
    let mut ix = evil_swap(
        &w.m1,
        &trader.pubkey(),
        &trader_x,
        &trader_z,
        HONEST,
        vec![],
    );
    ix.accounts[12].pubkey = pda::fee_authority(&w.m2.market);
    ix.accounts[13].pubkey = w.m2_fee_x;
    assert_fails_with(w.env.send(&[ix], &[&trader]), "ConstraintSeeds");

    assert_eq!(
        snapshot(&w.env, &victims),
        before,
        "H011: a cross-market fee binding changed a victim account"
    );

    // The real swap on M1 pays M1's fee account; a real sweep of M1 leaves M2's alone.
    let ix = evil_swap(
        &w.m1,
        &trader.pubkey(),
        &trader_x,
        &trader_z,
        HONEST,
        vec![],
    );
    w.env.send(&[ix], &[&trader]).expect("swap on M1");
    let m1_fee = w.env.token_balance(&m1_fee_x);
    assert_eq!(m1_fee, AMOUNT.div_ceil(400));
    let m2_fee = w.env.token_balance(&w.m2_fee_x);
    let treasury = w.env.token_balance(&w.treasury_x);
    let ix = client::sweep_fees(&w.m1.market, &x, &TOKEN, &treasury_owner, vec![]);
    w.env.send(&[ix], &[]).expect("sweep M1");
    assert_eq!(w.env.token_balance(&m1_fee_x), 0);
    assert_eq!(w.env.token_balance(&w.m2_fee_x), m2_fee);
    assert_eq!(w.env.token_balance(&w.treasury_x), treasury + m1_fee);
}

/// (a), shadow vault: a hostile market M3 can declare M2's fee account as its own input
/// vault. Its swaps then only donate input to M2's fee account (which goes to the
/// treasury); its strategy can't take anything back out.
#[test]
fn shadow_vault_over_another_markets_fee_account_is_only_a_donation() {
    let mut w = world();
    let x = w.m1.mint_a;
    let wmint = w.env.create_mint(&TOKEN, 6, None, None);
    let m3 = evil_market(&mut w.env, x, TOKEN, wmint, TOKEN, Some(w.m2_fee_x));
    assert_eq!(w.env.market(&m3.market).vaults[0], w.m2_fee_x);

    let (trader, trader_x) = w.env.trader_with(&x, &TOKEN, 10 * AMOUNT);
    let trader_w = w.env.create_ata(&trader.pubkey(), &wmint, &TOKEN);
    let fee_before = w.env.token_balance(&w.m2_fee_x);

    let ix = evil_swap(
        &m3,
        &trader.pubkey(),
        &trader_x,
        &trader_w,
        REFUND_MORE,
        vec![],
    );
    assert_fails_with(w.env.send(&[ix], &[&trader]), OWNER_MISMATCH);
    assert_eq!(
        w.env.token_balance(&w.m2_fee_x),
        fee_before,
        "H011: M3 moved tokens out of M2's fee account"
    );

    let ix = evil_swap(&m3, &trader.pubkey(), &trader_x, &trader_w, HONEST, vec![]);
    w.env.send(&[ix], &[&trader]).expect("swap on M3");
    assert_eq!(w.env.token_balance(&w.m2_fee_x), fee_before + AMOUNT);

    // The donation can only go where M2's fees go: the treasury.
    let treasury_owner = w.env.treasury_owner;
    let treasury = w.env.token_balance(&w.treasury_x);
    let ix = client::sweep_fees(&w.m2.market, &x, &TOKEN, &treasury_owner, vec![]);
    w.env.send(&[ix], &[]).expect("sweep M2");
    assert_eq!(
        w.env.token_balance(&w.treasury_x),
        treasury + fee_before + AMOUNT
    );
}
