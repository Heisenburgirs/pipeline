//! SOS H009: the router's guarantees under a hostile strategy.
//!
//! Variants not already covered by `adversarial.rs` / `security.rs`:
//!   * the trader's key duplicated in the strategy's remaining accounts (writable),
//!   * `trader_input` passed again as a remaining account,
//!   * a strategy-controlled integrator account (total debit must stay within
//!     `max_total_spend`),
//!   * the trader (as owner of a token account) placed in the input mint's transfer-hook
//!     account list, with a hostile hook that asks Token-2022 for the owner's signature.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_strategy_interface::fees;
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
const REFILL_HOOK_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ4");
const EXECUTE: [u8; 8] = [105, 37, 101, 197, 75, 251, 102, 26];

const HONEST: u8 = 0;
const TAKE_MORE: u8 = 1;
const TAKE_LAMPORTS: u8 = 9;
const PULL_AS_DELEGATE: u8 = 10;

fn compute(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction::new_with_bytes(
        client::pubkey!("ComputeBudget111111111111111111111111111111"),
        &data,
        vec![],
    )
}

struct EvilMarket {
    state: Pubkey,
    market: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
    tp_a: Pubkey,
}

fn evil_state(mint_a: &Pubkey, mint_b: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0
}

/// An evil market (mint A in `tp_a`, mint B classic) with 1e9 of B in its vault and
/// both fee accounts created.
fn setup_with_mint_a(env: &mut Env, mint_a: Pubkey, tp_a: Pubkey) -> EvilMarket {
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state = evil_state(&mint_a, &mint_b);
    let vault_a = env.create_ata(&state, &mint_a, &tp_a);
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
    env.send(&[ix], &[&creator]).expect("create evil market");
    let market = pda::market(&EVIL_ID, &state);
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &market, &mint_a, &tp_a),
            client::init_fee_account(&payer, &market, &mint_b, &TOKEN),
        ],
        &[],
    )
    .unwrap();
    env.mint_to(&mint_b, &TOKEN, &vault_b, 1_000_000_000);
    EvilMarket {
        state,
        market,
        mint_a,
        mint_b,
        vault_a,
        vault_b,
        tp_a,
    }
}

fn setup() -> (Env, EvilMarket) {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let market = setup_with_mint_a(&mut env, mint_a, TOKEN);
    (env, market)
}

struct Trader {
    keypair: Keypair,
    account_a: Pubkey,
    account_b: Pubkey,
}

fn trader(env: &mut Env, m: &EvilMarket, amount: u64) -> Trader {
    let (keypair, account_a) = env.trader_with(&m.mint_a, &m.tp_a, amount);
    let account_b = env.create_ata(&keypair.pubkey(), &m.mint_b, &TOKEN);
    Trader {
        keypair,
        account_a,
        account_b,
    }
}

fn swap_accounts(
    m: &EvilMarket,
    t: &Trader,
    strategy_accounts: Vec<AccountMeta>,
) -> client::SwapAccounts {
    let mut accounts = vec![AccountMeta::new(m.vault_b, false)];
    accounts.extend(strategy_accounts);
    client::SwapAccounts {
        trader: t.keypair.pubkey(),
        strategy_program: EVIL_ID,
        market_state: m.state,
        trader_input: t.account_a,
        trader_output: t.account_b,
        input_vault: m.vault_a,
        input_mint: m.mint_a,
        output_mint: m.mint_b,
        input_token_program: m.tp_a,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts: accounts,
        input_hook_accounts: vec![],
    }
}

fn args(behaviour: u8, amount: u64, integrator_bps: u16) -> client::SwapArgs {
    let mut a = exact_in(amount, 0);
    a.extra = vec![behaviour];
    a.integrator_fee_bps = integrator_bps;
    a
}

// ---------------------------------------------------------------------------------
// (a) the trader's signature never reaches the strategy, even across duplicate metas
// ---------------------------------------------------------------------------------

/// The trader's own key appears again in the strategy's accounts, writable and flagged
/// as a signer. The router forwards every remaining account without signer privilege, so
/// the strategy still cannot use the trader as an authority.
#[test]
fn duplicate_trader_key_in_remaining_accounts_gains_no_signature() {
    let (mut env, m) = setup();
    let t = trader(&mut env, &m, 10_000_000);
    let trader_key = t.keypair.pubkey();
    let lamports_before = env.lamports(&trader_key);

    // TakeMore: token transfer out of trader_input with the trader as authority.
    let accounts = swap_accounts(&m, &t, vec![AccountMeta::new(trader_key, true)]);
    let result = env.send(
        &[client::swap(&accounts, &args(TAKE_MORE, 1_000, 0))],
        &[&t.keypair],
    );
    println!(
        "TakeMore with duplicate trader: {:?}",
        result.as_ref().err().map(|e| &e.err)
    );
    assert_fails_with(result, "privilege escalated");

    // TakeLamports: system transfer out of the trader's wallet with the trader as signer.
    let accounts = swap_accounts(
        &m,
        &t,
        vec![
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
            AccountMeta::new(trader_key, true),
        ],
    );
    let result = env.send(
        &[client::swap(&accounts, &args(TAKE_LAMPORTS, 1_000, 0))],
        &[&t.keypair],
    );
    assert_fails_with(result, "privilege escalated");

    assert_eq!(env.token_balance(&t.account_a), 10_000_000);
    assert_eq!(env.token_balance(&t.account_b), 0);
    assert_eq!(env.lamports(&trader_key), lamports_before);
}

/// `trader_input` passed a second time as a strategy account. The strategy gets it
/// writable, but without the owner's signature it can neither move tokens out of it nor
/// does the market PDA (which does sign) have any authority over it.
#[test]
fn trader_input_as_remaining_account_gains_no_authority() {
    let (mut env, m) = setup();
    let t = trader(&mut env, &m, 10_000_000);
    for behaviour in [TAKE_MORE, PULL_AS_DELEGATE] {
        let accounts = swap_accounts(&m, &t, vec![AccountMeta::new(t.account_a, false)]);
        let result = env.send(
            &[client::swap(&accounts, &args(behaviour, 1_000, 0))],
            &[&t.keypair],
        );
        assert!(result.is_err(), "behaviour {behaviour} unexpectedly worked");
        println!(
            "behaviour {behaviour}: {:?}",
            result.as_ref().err().map(|e| &e.err)
        );
    }
    assert_eq!(env.token_balance(&t.account_a), 10_000_000);
    assert_eq!(env.token_balance(&t.account_b), 0);
}

// ---------------------------------------------------------------------------------
// (c) total debit never exceeds amount_in + fees, whoever the integrator is
// ---------------------------------------------------------------------------------

/// The integrator account is an account the strategy controls (its own input vault, or
/// the market's fee account). The trader's total debit is still exactly
/// `max_total_spend(amount_in, bps)` and no more, for a range of integrator rates.
#[test]
fn strategy_controlled_integrator_account_cannot_raise_the_debit() {
    for (label, target_is_vault) in [("input vault", true), ("fee account", false)] {
        for bps in [0u16, 1, 500, 10_000] {
            let (mut env, m) = setup();
            let t = trader(&mut env, &m, 10_000_000);
            let amount: u64 = 1_000_000;
            let target = if target_is_vault {
                m.vault_a
            } else {
                pda::fee_account(&m.market, &m.mint_a, &TOKEN)
            };
            let mut accounts = swap_accounts(&m, &t, vec![]);
            accounts.integrator_fee_account = if bps == 0 { None } else { Some(target) };
            let meta = env
                .send(
                    &[client::swap(&accounts, &args(HONEST, amount, bps))],
                    &[&t.keypair],
                )
                .expect("honest swap");
            let debit = 10_000_000 - env.token_balance(&t.account_a);
            let ceiling = fees::max_total_spend(amount, bps).unwrap();
            let event = &swap_events(&meta)[0];
            println!(
                "[{label}] bps {bps}: debit {debit}, ceiling {ceiling}, pipeline_fee {}, \
                 integrator_fee {}, received {}",
                event.pipeline_fee, event.integrator_fee, event.received
            );
            assert!(debit <= ceiling, "debit {debit} above ceiling {ceiling}");
            assert_eq!(debit, ceiling);
            assert_eq!(env.token_balance(&t.account_b), amount);
        }
    }
}

// ---------------------------------------------------------------------------------
// (a) via transfer hooks: the owner placed in the hook list must not become a signer
// ---------------------------------------------------------------------------------

fn validation_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], &REFILL_HOOK_ID).0
}

/// Writes an `ExtraAccountMetaList` for the refill hook with fixed-address entries
/// `(address, is_signer, is_writable)`, which `initialize` of the hook cannot express for
/// the signer flag.
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

struct HookWorld {
    env: Env,
    m: EvilMarket,
    t: Trader,
    /// A classic-SPL token account of the trader in an unrelated mint, with 5,000 in it.
    trader_x: Pubkey,
    /// A classic-SPL token account of someone else, same mint.
    sink_x: Pubkey,
    /// A classic-SPL token account owned by the hook's stash PDA, with 5,000 in it.
    stash: Pubkey,
    stash_authority: Pubkey,
}

fn hook_world() -> HookWorld {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    env.svm
        .add_program_from_file(REFILL_HOOK_ID, sbf_dir().join("test_hook_refill.so"))
        .unwrap();
    // The input mint is a Token-2022 mint with a hostile transfer hook.
    let mint_a = env.create_mint(&TOKEN_2022, 6, None, Some(REFILL_HOOK_ID));
    let m = setup_with_mint_a(&mut env, mint_a, TOKEN_2022);
    // Validation account needed by the trader's ATA creation? No, only by transfers.
    set_validation(&mut env, &mint_a, &[]);
    let t = trader(&mut env, &m, 10_000_000);

    let mint_x = env.create_mint(&TOKEN, 6, None, None);
    let trader_x = env.create_ata(&t.keypair.pubkey(), &mint_x, &TOKEN);
    env.mint_to(&mint_x, &TOKEN, &trader_x, 5_000);
    let sink_x = env.create_ata(&Pubkey::new_unique(), &mint_x, &TOKEN);
    let stash_authority = Pubkey::find_program_address(&[b"stash"], &REFILL_HOOK_ID).0;
    let stash = env.create_ata(&stash_authority, &mint_x, &TOKEN);
    env.mint_to(&mint_x, &TOKEN, &stash, 5_000);
    HookWorld {
        env,
        m,
        t,
        trader_x,
        sink_x,
        stash,
        stash_authority,
    }
}

fn hook_swap(w: &mut HookWorld, hook_accounts: Vec<AccountMeta>) -> TxResult {
    let mut accounts = swap_accounts(&w.m, &w.t, vec![]);
    accounts.input_hook_accounts = hook_accounts;
    let ix = client::swap(&accounts, &args(HONEST, 1_000, 0));
    w.env.send(&[compute(400_000), ix], &[&w.t.keypair])
}

fn hook_base_accounts(mint: &Pubkey) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(REFILL_HOOK_ID, false),
        AccountMeta::new_readonly(validation_address(mint), false),
    ]
}

/// Control: with the hook's own PDA as the authority entry, the hostile hook does run
/// during the router's input transfer and moves its stash. This shows the probe below
/// reaches the hook and that a failure there is about the signer privilege.
#[test]
fn hook_probe_control_hook_runs_during_the_input_transfer() {
    let mut w = hook_world();
    let mint_a = w.m.mint_a;
    set_validation(
        &mut w.env,
        &mint_a,
        &[
            (w.stash, false, true),
            (w.stash_authority, false, false),
            (w.sink_x, false, true),
            (TOKEN, false, false),
        ],
    );
    let mut hook_accounts = hook_base_accounts(&mint_a);
    hook_accounts.extend([
        AccountMeta::new(w.stash, false),
        AccountMeta::new_readonly(w.stash_authority, false),
        AccountMeta::new(w.sink_x, false),
        AccountMeta::new_readonly(TOKEN, false),
    ]);
    let result = hook_swap(&mut w, hook_accounts);
    if let Err(e) = &result {
        println!("control failed: {:?}\n{}", e.err, e.meta.logs.join("\n"));
    }
    result.expect("control swap");
    println!("control: sink_x = {}", w.env.token_balance(&w.sink_x));
    assert_eq!(w.env.token_balance(&w.sink_x), 5_000);
}

/// The probe: the hostile hook lists the trader's own wallet as a *signer* extra account
/// and uses it as the authority of a transfer out of the trader's other token account.
/// Token-2022 must strip signer privilege from hook extras, so the hook's CPI is
/// refused and the trader's other tokens stay put.
#[test]
fn hook_cannot_obtain_the_traders_signature_through_the_hook_list() {
    let mut w = hook_world();
    let mint_a = w.m.mint_a;
    let trader_key = w.t.keypair.pubkey();
    // Roles in the hook's `execute`: stash = trader_x, authority = trader (signer flag
    // set), target = sink_x, token program.
    set_validation(
        &mut w.env,
        &mint_a,
        &[
            (w.trader_x, false, true),
            (trader_key, true, false),
            (w.sink_x, false, true),
            (TOKEN, false, false),
        ],
    );
    let mut hook_accounts = hook_base_accounts(&mint_a);
    hook_accounts.extend([
        AccountMeta::new(w.trader_x, false),
        AccountMeta::new_readonly(trader_key, false),
        AccountMeta::new(w.sink_x, false),
        AccountMeta::new_readonly(TOKEN, false),
    ]);
    let result = hook_swap(&mut w, hook_accounts);
    match &result {
        Ok(_) => println!("probe: swap SUCCEEDED"),
        Err(e) => println!("probe: failed: {:?}\n{}", e.err, e.meta.logs.join("\n")),
    }
    println!(
        "trader_x = {}, sink_x = {}",
        w.env.token_balance(&w.trader_x),
        w.env.token_balance(&w.sink_x)
    );
    assert_eq!(
        w.env.token_balance(&w.trader_x),
        5_000,
        "BUG: the hook moved the trader's tokens with the trader's signature"
    );
    assert_eq!(w.env.token_balance(&w.sink_x), 0);
    assert_fails_with(result, "privilege escalated");
}

// ---------------------------------------------------------------------------------
// (d) aliasing: a hostile market declares the victim's own token account as its vault
// ---------------------------------------------------------------------------------

/// The strategy chooses the market's vault addresses when the market is created, so it can
/// name the victim's own input token account as `vault[A]`. The router then moves the
/// input from the trader to the same account (a no-op), and the strategy cannot take it
/// without the trader's signature. The router must not let the strategy deliver output
/// for "input" that never left the trader.
#[test]
fn market_vault_aliasing_the_traders_input_account_cannot_sell_nothing() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let victim = env.funded_keypair();
    let victim_a = env.create_ata(&victim.pubkey(), &mint_a, &TOKEN);
    let victim_b = env.create_ata(&victim.pubkey(), &mint_b, &TOKEN);
    env.mint_to(&mint_a, &TOKEN, &victim_a, 10_000_000);

    let state = evil_state(&mint_a, &mint_b);
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
            AccountMeta::new(victim_a, false), // vault[A] is the victim's own account
            AccountMeta::new(vault_b, false),
        ],
    });
    env.send(&[ix], &[&creator]).expect("hostile market");
    let market = pda::market(&EVIL_ID, &state);
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &market, &mint_a, &TOKEN),
            client::init_fee_account(&payer, &market, &mint_b, &TOKEN),
        ],
        &[],
    )
    .unwrap();
    env.mint_to(&mint_b, &TOKEN, &vault_b, 1_000_000_000);

    let m = EvilMarket {
        state,
        market,
        mint_a,
        mint_b,
        vault_a: victim_a,
        vault_b,
        tp_a: TOKEN,
    };
    let t = Trader {
        keypair: victim,
        account_a: victim_a,
        account_b: victim_b,
    };
    let accounts = swap_accounts(&m, &t, vec![]);
    let result = env.send(
        &[client::swap(&accounts, &args(HONEST, 1_000, 0))],
        &[&t.keypair],
    );
    println!(
        "aliased vault swap: {:?}",
        result.as_ref().err().map(|e| &e.err)
    );
    assert_fails_with(result, "OutputWithoutInput");
    assert_eq!(env.token_balance(&victim_a), 10_000_000);
    assert_eq!(env.token_balance(&victim_b), 0);
}
