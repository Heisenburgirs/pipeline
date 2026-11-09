//! SOS audit #2, H038: a keypair-addressed vault that the strategy later closes can be
//! re-created by the keypair's holder, while the immutable `Market` keeps pointing at
//! its address.
//!
//! The router's only checks on `input_vault` are the address (`market.vaults[i]`) and
//! `token::checked_balance`: token-program owner, account length/type and mint. It never
//! looks at the token account's `owner` field. So a re-created vault owned by someone
//! else passes. What these tests settle:
//!
//! 1. End to end on the evil strategy with a keypair vault: the keypair holder really
//!    can re-create the vault after it is closed (real `create_account` and
//!    `initialize_account3`, signed by the keypair). The closure itself is emulated with
//!    `set_account` because no fixture closes a vault.
//! 2. A closed, not re-created vault makes swaps on that input side fail cleanly in
//!    `checked_balance`, and nothing moves.
//! 3. Vault substitutes of the wrong mint or token program are refused.
//! 4. A swap into an attacker-owned re-created vault is accepted. The router still keeps
//!    its own guarantees: the trader spends exactly `amount_in` plus the full fee, the
//!    fee lands in the market's fee account, the trader receives the strategy's output,
//!    and no other market's balance moves. The only loss is the strategy's own pool,
//!    which the attacker (the vault's new owner) can now withdraw (accepted design).
//! 5. The same on the constant-product fixture with a crafted owner swap at its real
//!    vault address, where the strategy credits input by measuring the vault.

#![allow(clippy::result_large_err)]

mod bok_swap_util;
mod common;

use bok_swap_util::*;
use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use pipeline_strategy_interface as psi;
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

const RESERVE: u64 = 1_000_000_000;
const AMOUNT: u64 = 10_000;
const TOKEN_ACCOUNT_LEN: usize = 165;

fn closed() -> Account {
    Account::default()
}

/// A token account owned (in the token program's sense) by `new_owner`, derived from a
/// real account so every other field is valid.
fn with_token_owner(base: &Account, new_owner: &Pubkey, amount: u64) -> Account {
    let mut account = base.clone();
    account.data[32..64].copy_from_slice(new_owner.as_ref());
    account.data[64..72].copy_from_slice(&amount.to_le_bytes());
    account
}

/// Re-creates a token account at the keypair address with real transactions signed by
/// the keypair, owned (token-program-wise) by `owner`.
fn recreate_vault(env: &mut Env, vault: &Keypair, mint: &Pubkey, owner: &Pubkey) {
    let rent = env
        .svm
        .minimum_balance_for_rent_exemption(TOKEN_ACCOUNT_LEN);
    let create = solana_system_interface::instruction::create_account(
        &env.admin.pubkey(),
        &vault.pubkey(),
        rent,
        TOKEN_ACCOUNT_LEN as u64,
        &TOKEN,
    );
    let init =
        spl_token_interface::instruction::initialize_account3(&TOKEN, &vault.pubkey(), mint, owner)
            .unwrap();
    env.send(&[create, init], &[vault])
        .expect("the keypair holder can re-create the closed vault");
}

fn withdraw(env: &mut Env, owner: &Keypair, from: &Pubkey, to: &Pubkey, amount: u64) {
    let ix =
        spl_token_interface::instruction::transfer(&TOKEN, from, to, &owner.pubkey(), &[], amount)
            .unwrap();
    env.send(&[ix], &[owner]).expect("owner withdraws");
}

struct Evil {
    env: Env,
    m: EvilMarket,
    vault_a_key: Keypair,
    trader: Keypair,
    trader_a: Pubkey,
    trader_b: Pubkey,
    fee_a: Pubkey,
    /// A bystander: another evil-strategy-independent CP market that must not move.
    cp: CpMarket,
}

/// An evil market whose input vault (mint A) is a *keypair* token account owned by the
/// state PDA, plus a CP market as a bystander.
fn evil_world() -> Evil {
    let mut env = Env::new();
    let cp = setup_cp_market(&mut env, TOKEN, TOKEN, RESERVE, RESERVE, 30);
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state = evil_state(&mint_a, &mint_b);
    let vault_a_key = Keypair::new();
    // Create the keypair vault as a token account owned by the state PDA, by copying a
    // real ATA's layout to the keypair address (crafted `set_account`).
    let template = env.create_ata(&state, &mint_a, &TOKEN);
    let template_account = env.svm.get_account(&template).unwrap();
    env.svm
        .set_account(vault_a_key.pubkey(), template_account)
        .unwrap();
    let m = evil_market(
        &mut env,
        mint_a,
        TOKEN,
        mint_b,
        Some(vault_a_key.pubkey()),
        RESERVE,
    );
    assert_eq!(
        env.market(&m.market).vaults[0],
        vault_a_key.pubkey(),
        "setup: the market records the keypair address as vault A"
    );
    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let fee_a = pda::fee_account(&m.market, &mint_a, &TOKEN);
    Evil {
        env,
        m,
        vault_a_key,
        trader,
        trader_a,
        trader_b,
        fee_a,
        cp,
    }
}

impl Evil {
    fn swap_ix(&self) -> client::Instruction {
        client::swap(
            &evil_swap_accounts(
                &self.m,
                &self.trader.pubkey(),
                &self.trader_a,
                &self.trader_b,
            ),
            &evil_args(HONEST, AMOUNT, 0),
        )
    }

    fn swap(&mut self) -> TxResult {
        let ix = self.swap_ix();
        let trader = self.trader.insecure_clone();
        send_swap(&mut self.env, ix, &trader)
    }

    fn watched(&self) -> Vec<Pubkey> {
        vec![
            self.trader_a,
            self.trader_b,
            self.m.vault_a,
            self.m.vault_b,
            self.fee_a,
            self.cp.vault_a,
            self.cp.vault_b,
        ]
    }
}

/// The control: before anything is closed, the keypair-vault market swaps normally.
#[test]
fn sos2_h038_control_keypair_vault_market_swaps() {
    let mut w = evil_world();
    let meta = w.swap().expect("control swap");
    let (result, _) = reported(&meta);
    assert_eq!(result.net_sent, AMOUNT);
    assert_eq!(w.env.token_balance(&w.m.vault_a), AMOUNT);
}

/// A closed vault that nobody re-creates: the swap fails in the router's token-account
/// check and moves nothing. The market is dead on that input side, and the router has no
/// instruction that could repair the record (code reading: nothing writes `Market`
/// after `create_market`).
#[test]
fn sos2_h038_closed_vault_fails_cleanly_and_moves_nothing() {
    let mut w = evil_world();
    w.env
        .svm
        .set_account(w.vault_a_key.pubkey(), closed())
        .unwrap();
    let watched = w.watched();
    let before = balances(&w.env, &watched);
    let result = w.swap();
    assert!(
        result.is_err(),
        "H038: a swap into a closed vault must fail"
    );
    assert_fails_with(w.swap(), "BadTokenAccount");
    assert_eq!(
        balances(&w.env, &watched),
        before,
        "H038: a failed swap moved a balance"
    );
}

/// Substitutes at the recorded vault address that the router must refuse: wrong mint,
/// wrong token program, the mint account itself.
#[test]
fn sos2_h038_wrong_mint_or_program_recreations_are_refused() {
    let mut w = evil_world();
    let attacker = Pubkey::new_unique();
    let base = w
        .env
        .svm
        .get_account(&w.m.vault_a)
        .unwrap_or_else(|| panic!("vault exists"));
    let other_mint = w.env.create_mint(&TOKEN, 6, None, None);

    let mut wrong_mint = with_token_owner(&base, &attacker, 0);
    wrong_mint.data[..32].copy_from_slice(other_mint.as_ref());
    let mut wrong_program = with_token_owner(&base, &attacker, 0);
    wrong_program.owner = TOKEN_2022;
    let mint_account = w.env.svm.get_account(&w.m.mint_a).unwrap();

    let watched = w.watched();
    for (label, account) in [
        ("wrong mint", wrong_mint),
        ("wrong token program", wrong_program),
        ("the mint account itself", mint_account),
    ] {
        w.env.svm.set_account(w.m.vault_a, account).unwrap();
        let before = balances(&w.env, &watched);
        let result = w.swap();
        assert!(
            result.is_err(),
            "H038: the router accepted a vault substitute: {label}"
        );
        assert_eq!(
            balances(&w.env, &watched),
            before,
            "H038 {label}: a balance moved"
        );
    }
}

/// The hypothesis end to end: close, re-create by the keypair holder (real
/// transactions), swap. The router accepts the attacker-owned vault; its own guarantees
/// to the trader and to Pipeline hold, and the attacker can then withdraw the input.
#[test]
fn sos2_h038_recreated_attacker_vault_router_guarantees_hold() {
    let mut w = evil_world();
    let attacker = Keypair::new();
    w.env.svm.airdrop(&attacker.pubkey(), 10 * SOL).unwrap();
    let attacker_a = w.env.create_ata(&attacker.pubkey(), &w.m.mint_a, &TOKEN);

    // Close (emulated), then the keypair holder re-creates it for real.
    w.env
        .svm
        .set_account(w.vault_a_key.pubkey(), closed())
        .unwrap();
    assert!(
        w.swap().is_err(),
        "setup: the swap fails while the vault is closed"
    );
    let mint_a = w.m.mint_a;
    let vault_key = w.vault_a_key.insecure_clone();
    recreate_vault(&mut w.env, &vault_key, &mint_a, &attacker.pubkey());
    assert_eq!(w.env.token_balance(&w.m.vault_a), 0, "setup: fresh vault");

    let trader_a_before = w.env.token_balance(&w.trader_a);
    let trader_b_before = w.env.token_balance(&w.trader_b);
    let fee_before = w.env.token_balance(&w.fee_a);
    let cp_before = balances(&w.env, &[w.cp.vault_a, w.cp.vault_b]);

    let meta = w
        .swap()
        .expect("the router accepts a re-created vault of the right mint and program");
    let (result, event) = reported(&meta);

    // What the router still guarantees.
    let fee = psi::fees::ceil_bps(AMOUNT, 25).unwrap();
    assert_eq!(result.net_sent, AMOUNT, "H038: net_sent");
    assert_eq!(result.pipeline_fee, fee, "H038: full fee charged");
    assert_eq!(event.pipeline_fee, fee);
    assert_eq!(
        w.env.token_balance(&w.fee_a),
        fee_before + fee,
        "H038: the fee reached the market's fee account"
    );
    assert_eq!(
        w.env.token_balance(&w.trader_a),
        trader_a_before - AMOUNT - fee,
        "H038: the trader paid exactly amount_in plus the fee"
    );
    assert_eq!(
        w.env.token_balance(&w.trader_b),
        trader_b_before + result.received,
        "H038: the trader received the reported output"
    );
    assert_eq!(result.received, AMOUNT, "evil honest mode pays 1:1");
    assert_eq!(
        balances(&w.env, &[w.cp.vault_a, w.cp.vault_b]),
        cp_before,
        "H038: another market's vaults moved"
    );

    // The input sits in the attacker-owned vault and is withdrawable by the attacker.
    assert_eq!(w.env.token_balance(&w.m.vault_a), AMOUNT);
    let vault_a = w.m.vault_a;
    withdraw(&mut w.env, &attacker, &vault_a, &attacker_a, AMOUNT);
    assert_eq!(w.env.token_balance(&attacker_a), AMOUNT);
    println!(
        "H038: router accepted the attacker-owned vault; trader and fee intact; \
         {AMOUNT} input withdrawn by the keypair holder (loss is the strategy's own pool)"
    );
}

/// The same on the constant-product fixture, where the strategy credits input by
/// measuring the vault: an owner swap at the real vault address (crafted) is accepted,
/// the trader still gets the quoted output and the fee, and the attacker can drain the
/// pool's input reserve. The router can't tell, because it records no vault owner.
#[test]
fn sos2_h038_cp_owner_swapped_vault_accepted_and_trader_unharmed() {
    let mut env = Env::new();
    let m = setup_cp_market(&mut env, TOKEN, TOKEN, RESERVE, RESERVE, 30);
    let (trader, trader_a) = env.trader_with(&m.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &m.mint_b, &TOKEN);
    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 10 * SOL).unwrap();
    let attacker_a = env.create_ata(&attacker.pubkey(), &m.mint_a, &TOKEN);

    let accounts = cp_swap_accounts(
        &m,
        &trader.pubkey(),
        &m.mint_a,
        &trader_a,
        &m.mint_b,
        &trader_b,
    );
    // Control: the real vault.
    let meta = send_swap(
        &mut env,
        client::swap(&accounts, &exact_in(AMOUNT, 0)),
        &trader,
    )
    .expect("control swap");
    let control = swap_result(&meta);
    let reserve_a = RESERVE + AMOUNT;
    let reserve_b = RESERVE - control.received;
    assert_eq!(env.token_balance(&m.vault_a), reserve_a);

    // The vault is re-created at the same address, owner = attacker, same balance.
    let real = env.svm.get_account(&m.vault_a).unwrap();
    env.svm
        .set_account(
            m.vault_a,
            with_token_owner(&real, &attacker.pubkey(), reserve_a),
        )
        .unwrap();

    let expected_out = cp_out(reserve_a, reserve_b, AMOUNT, 30);
    let trader_a_before = env.token_balance(&trader_a);
    let trader_b_before = env.token_balance(&trader_b);
    let fee_account = pda::fee_account(&m.market, &m.mint_a, &TOKEN);
    let fee_before = env.token_balance(&fee_account);
    let meta = send_swap(
        &mut env,
        client::swap(&accounts, &exact_in(AMOUNT, expected_out)),
        &trader,
    )
    .expect("the router accepts the owner-swapped vault");
    let (result, _) = reported(&meta);
    let fee = psi::fees::ceil_bps(AMOUNT, 25).unwrap();
    assert_eq!(result.received, expected_out, "H038: quoted output");
    assert_eq!(result.pipeline_fee, fee);
    assert_eq!(env.token_balance(&fee_account), fee_before + fee);
    assert_eq!(env.token_balance(&trader_a), trader_a_before - AMOUNT - fee);
    assert_eq!(env.token_balance(&trader_b), trader_b_before + expected_out);

    // The attacker drains the pool's whole input reserve; the trader lost nothing.
    let vault_a = m.vault_a;
    let balance = env.token_balance(&vault_a);
    withdraw(&mut env, &attacker, &vault_a, &attacker_a, balance);
    assert_eq!(env.token_balance(&attacker_a), balance);
    assert_eq!(env.token_balance(&vault_a), 0);
}
