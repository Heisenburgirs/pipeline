//! SOS audit #2, H016: which token-program-owned accounts can pass `checked_balance`,
//! `balance` or `decimals` (type confusion).
//!
//! The router's parsers are deliberately loose (`token.rs`): `checked_balance` checks
//! owner, length (>= 165, not 355), the Token-2022 type byte (when longer than 165) and
//! the mint field, and then reads `amount` at bytes 64..72. It does not check the
//! account state byte or the token-account owner. `balance` checks only length (the
//! caller pins the address). `decimals` reads byte 44 of any >= 82-byte account.
//!
//! The question is whether any account that the token programs can actually produce (or
//! that a transaction can reach on a real cluster) passes a parser while not being a
//! real token account / mint, with an attacker-controlled "amount" or "decimals".
//!
//! Part 1 asks the real SPL Token and Token-2022 programs (loaded in LiteSVM) to build
//! accounts of odd lengths and shapes, and checks that nothing that is not an
//! initialised token account passes the router's shape test with a real mint in bytes
//! 0..32. Part 2 injects layouts the token programs would never produce with
//! `set_account` (state crafting, unreachable on a real cluster) and asserts a swap can
//! never succeed through them. Part 3 does the same for `sweep_fees` (`balance`). Part 4
//! covers `decimals` with a market "mint" that is not a mint.

#![allow(clippy::result_large_err)]

mod bok_swap_util;
mod common;

use bok_swap_util::*;
use common::*;
use pipeline_client::{self as client, Instruction, Pubkey};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

const RESERVE: u64 = 1_000_000_000;

/// The router's shape test, exactly as `token::checked_balance` applies it (minus the
/// owner and mint checks).
fn router_shape(data: &[u8]) -> bool {
    data.len() >= 165 && data.len() != 355 && (data.len() == 165 || data[165] == 2)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Init {
    Mint,
    Account,
    Multisig,
    /// Token-2022 only: the `ImmutableOwner` extension on an otherwise uninitialised
    /// account.
    ImmutableOwnerOnly,
}

fn init_ixs(
    program: &Pubkey,
    kind: Init,
    account: &Pubkey,
    mint: &Pubkey,
    owner: &Pubkey,
) -> Vec<Instruction> {
    let signer_a = Pubkey::new_unique();
    let signer_b = Pubkey::new_unique();
    let t22 = *program == TOKEN_2022;
    match kind {
        Init::Mint => vec![if t22 {
            spl_token_2022_interface::instruction::initialize_mint2(
                program, account, owner, None, 6,
            )
            .unwrap()
        } else {
            spl_token_interface::instruction::initialize_mint2(program, account, owner, None, 6)
                .unwrap()
        }],
        Init::Account => vec![if t22 {
            spl_token_2022_interface::instruction::initialize_account3(
                program, account, mint, owner,
            )
            .unwrap()
        } else {
            spl_token_interface::instruction::initialize_account3(program, account, mint, owner)
                .unwrap()
        }],
        Init::Multisig => vec![if t22 {
            spl_token_2022_interface::instruction::initialize_multisig2(
                program,
                account,
                &[&signer_a, &signer_b],
                1,
            )
            .unwrap()
        } else {
            spl_token_interface::instruction::initialize_multisig2(
                program,
                account,
                &[&signer_a, &signer_b],
                1,
            )
            .unwrap()
        }],
        Init::ImmutableOwnerOnly => {
            assert!(t22);
            vec![
                spl_token_2022_interface::instruction::initialize_immutable_owner(program, account)
                    .unwrap(),
            ]
        }
    }
}

/// Creates a `len`-byte account owned by `program` and runs `kind`'s initialiser in the
/// same transaction. Returns the account's final data if the token program accepted it.
fn try_create(
    env: &mut Env,
    program: &Pubkey,
    kind: Init,
    len: usize,
    mint: &Pubkey,
) -> Option<(Pubkey, Vec<u8>)> {
    let account = Keypair::new();
    let owner = Pubkey::new_unique();
    let rent = env.svm.minimum_balance_for_rent_exemption(len);
    let payer = env.admin.pubkey();
    let mut ixs = vec![solana_system_interface::instruction::create_account(
        &payer,
        &account.pubkey(),
        rent,
        len as u64,
        program,
    )];
    ixs.extend(init_ixs(program, kind, &account.pubkey(), mint, &owner));
    env.send(&ixs, &[&account]).ok()?;
    Some((account.pubkey(), env.account_data(&account.pubkey())))
}

/// Part 1: ask the real token programs for every odd shape we can think of and check
/// that nothing which is not a real, initialised token account in `mint` passes the
/// router's shape test with `mint` in bytes 0..32.
#[test]
fn sos2_h016_real_token_programs_cannot_build_a_confusable_account() {
    let mut env = Env::new();
    let lens = [82usize, 83, 165, 166, 170, 200, 355, 356, 400];
    let mut table = vec![];
    let mut accepted = 0;
    for program in [TOKEN, TOKEN_2022] {
        let mint = env.create_mint(&program, 6, None, None);
        let mut kinds = vec![Init::Mint, Init::Account, Init::Multisig];
        if program == TOKEN_2022 {
            kinds.push(Init::ImmutableOwnerOnly);
        }
        for kind in kinds {
            for &len in &lens {
                let Some((_, data)) = try_create(&mut env, &program, kind, len, &mint) else {
                    table.push(format!(
                        "{program} {kind:?} len {len}: refused by token program"
                    ));
                    continue;
                };
                accepted += 1;
                let shape = router_shape(&data);
                let mint_field_matches = data[..32] == mint.to_bytes();
                table.push(format!(
                    "{program} {kind:?} len {len}: created; router_shape={shape} mint_field_matches={mint_field_matches} byte165={:?} state108={:?}",
                    data.get(165),
                    data.get(108)
                ));
                match kind {
                    Init::Mint | Init::Multisig => {
                        // A mint or multisig must never pass the shape test.
                        assert!(
                            !shape,
                            "H016 BUG: a {kind:?} of {len} bytes under {program} passes the router's token-account shape test"
                        );
                    }
                    Init::Account => {
                        if shape && mint_field_matches {
                            assert_eq!(
                                data[108], 1,
                                "H016 BUG: shape-passing account with matching mint is not initialised"
                            );
                        }
                    }
                    Init::ImmutableOwnerOnly => {
                        // Uninitialised base: the token program leaves the type byte at 0
                        // (so the shape test refuses it) and the mint field all zero, so
                        // it could never match a market mint either.
                        assert_eq!(data[108], 0, "base must be uninitialised");
                        assert!(
                            data[..32].iter().all(|b| *b == 0),
                            "mint field must be zero"
                        );
                    }
                }
            }
        }
    }
    for line in &table {
        println!("{line}");
    }
    assert!(
        accepted > 0,
        "the token programs built nothing: the harness is broken"
    );
    // Anchors: the shapes the analysis relies on.
    assert!(
        table
            .iter()
            .any(|l| l.contains("Mint len 165:") && l.contains("refused")),
        "a 165-byte mint must not be creatable"
    );
    assert!(
        table
            .iter()
            .all(|l| !(l.contains("Mint len 165:") && l.contains("created"))),
        "H016 BUG: a 165-byte mint was creatable"
    );
}

// ------------------------------------------------------------------ the swap world

struct World {
    env: Env,
    m: CpMarket,
    trader: Keypair,
    trader_a: Pubkey,
    trader_b: Pubkey,
}

fn world(tp: Pubkey) -> World {
    let mut env = Env::new();
    let m = setup_cp_market(&mut env, tp, tp, RESERVE, RESERVE, 30);
    let (trader, trader_a) = env.trader_with(&m.mint_a, &tp, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &m.mint_b, &tp);
    World {
        env,
        m,
        trader,
        trader_a,
        trader_b,
    }
}

impl World {
    fn watched(&self) -> Vec<Pubkey> {
        vec![self.trader_a, self.trader_b, self.m.vault_a, self.m.vault_b]
    }

    /// Swap A -> B for `amount` with trader_input / trader_output replaced by crafted
    /// accounts (if given), asserting nothing moved on failure.
    fn swap(
        &mut self,
        crafted_input: Option<Account>,
        crafted_output: Option<Account>,
        amount: u64,
    ) -> TxResult {
        let mut accounts = cp_swap_accounts(
            &self.m,
            &self.trader.pubkey(),
            &self.m.mint_a,
            &self.trader_a,
            &self.m.mint_b,
            &self.trader_b,
        );
        if let Some(account) = crafted_input {
            let address = Pubkey::new_unique();
            self.env.svm.set_account(address, account).unwrap();
            accounts.trader_input = address;
        }
        if let Some(account) = crafted_output {
            let address = Pubkey::new_unique();
            self.env.svm.set_account(address, account).unwrap();
            accounts.trader_output = address;
        }
        let ix = client::swap(&accounts, &exact_in(amount, 0));
        let trader = self.trader.insecure_clone();
        send_swap(&mut self.env, ix, &trader)
    }
}

fn error_text(result: &TxResult) -> String {
    match result {
        Ok(_) => "ok".into(),
        Err(failed) => format!("{:?}\n{}", failed.err, failed.meta.logs.join("\n")),
    }
}

/// Layouts the token programs never produce (unreachable on a real cluster), built
/// from a real token account `real` and the market mint, that the router's parser may
/// still accept.
fn crafted_layouts(real: &Account, mint: &Pubkey, tp: &Pubkey) -> Vec<(&'static str, Account)> {
    let lamports = SOL;
    let mk = |data: Vec<u8>, owner: Pubkey| Account {
        lamports,
        data,
        owner,
        executable: false,
        rent_epoch: 0,
    };
    let mut out = vec![];

    // A 165-byte account in mint layout: bytes 0..32 = the market mint, `amount`
    // region (64..72) huge, state byte 0. The shape test cannot tell it from a token
    // account; only the token program can.
    let mut data = vec![0u8; 165];
    data[..32].copy_from_slice(mint.as_ref());
    data[64..72].copy_from_slice(&(u64::MAX / 2).to_le_bytes());
    out.push(("165 bytes, uninitialised state, huge amount", mk(data, *tp)));

    // The same, state byte set to initialised but with a mint-shaped 82-byte prefix
    // semantics (mint field forced).
    let mut data = vec![0u8; 165];
    data[..32].copy_from_slice(mint.as_ref());
    data[44] = 6; // decimals position of a mint
    data[45] = 1; // mint is_initialized position
    data[64..72].copy_from_slice(&(u64::MAX / 2).to_le_bytes());
    out.push(("165 bytes, mint-layout flags, huge amount", mk(data, *tp)));

    // Token-2022 only shape: extension space with type byte 2 but uninitialised base.
    let mut data = vec![0u8; 200];
    data[..32].copy_from_slice(mint.as_ref());
    data[64..72].copy_from_slice(&(u64::MAX / 2).to_le_bytes());
    data[165] = 2;
    out.push((
        "200 bytes, type byte 2, uninitialised base, huge amount",
        mk(data, *tp),
    ));

    // A real account made frozen (state 2).
    let mut frozen = real.clone();
    frozen.data[108] = 2;
    out.push(("real account, frozen", frozen));

    // A real account with a forged amount (the token program uses the same field, so
    // this is just a different real account; it must still move nothing it does not own).
    let mut rich = real.clone();
    rich.data[64..72].copy_from_slice(&(u64::MAX / 2).to_le_bytes());
    out.push(("real layout, forged huge amount (unowned funds)", rich));
    out
}

/// Part 2: every crafted layout, as trader_input or trader_output, on SPL Token and
/// Token-2022 markets. A swap through a layout the token program cannot or may not use
/// must fail and move nothing. For the "forged amount" layout, the swap must not
/// succeed with an inflated result.
#[test]
fn sos2_h016_crafted_layouts_never_yield_a_successful_swap() {
    for tp in [TOKEN, TOKEN_2022] {
        let mut w = world(tp);
        let real_in = w.env.svm.get_account(&w.trader_a).unwrap();
        let real_out = w.env.svm.get_account(&w.trader_b).unwrap();

        // Control: the unmodified swap succeeds.
        let meta = w.swap(None, None, 1_000).expect("control swap");
        assert_eq!(reported(&meta).0.net_sent, 1_000);

        for slot in ["input", "output"] {
            let (real, mint) = if slot == "input" {
                (&real_in, w.m.mint_a)
            } else {
                (&real_out, w.m.mint_b)
            };
            for (label, account) in crafted_layouts(real, &mint, &tp) {
                let watched = w.watched();
                let before = balances(&w.env, &watched);
                let result = if slot == "input" {
                    w.swap(Some(account.clone()), None, 1_000)
                } else {
                    w.swap(None, Some(account.clone()), 1_000)
                };
                println!(
                    "[{tp}] {slot} `{label}` -> {}",
                    error_text(&result).lines().next().unwrap()
                );
                if label.starts_with("real layout, forged") {
                    // A forged balance on an account the token program can use is a
                    // different real account; it can only matter if it makes the swap
                    // pay out unowned funds. The output must not exceed what the
                    // market's pricing owes for 1_000 in.
                    if let Ok(meta) = &result {
                        let (swap_result, _) = reported(meta);
                        assert!(
                            swap_result.received <= 1_000,
                            "H016 BUG: forged amount inflated `received` to {} ({slot})",
                            swap_result.received
                        );
                    }
                    continue;
                }
                assert!(
                    result.is_err(),
                    "H016 BUG: crafted `{label}` as trader_{slot} under {tp} swapped successfully"
                );
                assert_eq!(
                    balances(&w.env, &watched),
                    before,
                    "H016 `{label}` ({slot}, {tp}): a balance moved on a failed swap"
                );
            }
        }
    }
}

/// A closed account must not pass: close the (empty) output account in the same
/// transaction, before the swap, and use it as trader_output. Covers the "closed then
/// reallocated" candidate with the token programs' real close semantics.
#[test]
fn sos2_h016_closed_account_is_refused() {
    for tp in [TOKEN, TOKEN_2022] {
        let mut w = world(tp);
        let close = if tp == TOKEN_2022 {
            spl_token_2022_interface::instruction::close_account(
                &tp,
                &w.trader_b,
                &w.trader.pubkey(),
                &w.trader.pubkey(),
                &[],
            )
            .unwrap()
        } else {
            spl_token_interface::instruction::close_account(
                &tp,
                &w.trader_b,
                &w.trader.pubkey(),
                &w.trader.pubkey(),
                &[],
            )
            .unwrap()
        };
        let accounts = cp_swap_accounts(
            &w.m,
            &w.trader.pubkey(),
            &w.m.mint_a,
            &w.trader_a,
            &w.m.mint_b,
            &w.trader_b,
        );
        let swap = client::swap(&accounts, &exact_in(1_000, 0));
        let before = balances(&w.env, &[w.trader_a, w.m.vault_a, w.m.vault_b]);
        let trader = w.trader.insecure_clone();
        let result = w.env.send(&[compute(1_000_000), close, swap], &[&trader]);
        println!(
            "[{tp}] closed output -> {}",
            error_text(&result).lines().next().unwrap()
        );
        assert!(
            result.is_err(),
            "H016 BUG: a swap into an account closed earlier in the transaction succeeded under {tp}"
        );
        assert_eq!(
            balances(&w.env, &[w.trader_a, w.m.vault_a, w.m.vault_b]),
            before
        );
    }
}

/// The uninitialised-with-extension-space account the token programs *can* make
/// (`ImmutableOwner` only, Token-2022) has type byte 0 and a zero mint field, so
/// `checked_balance` refuses it.
#[test]
fn sos2_h016_uninitialised_t22_account_with_type_byte_is_refused() {
    let mut w = world(TOKEN_2022);
    let (address, data) = try_create(
        &mut w.env,
        &TOKEN_2022,
        Init::ImmutableOwnerOnly,
        200,
        &w.m.mint_b,
    )
    .expect("token-2022 builds an ImmutableOwner-only account");
    println!(
        "ImmutableOwnerOnly len 200: byte165={:?} shape={}",
        data.get(165),
        router_shape(&data)
    );
    // The extension initialiser leaves the type byte at 0: the type byte is only set by
    // InitializeAccount, so an uninitialised base never carries the "account" type.
    assert!(data[108] == 0 && data[165] == 0);
    let account = w.env.svm.get_account(&address).unwrap();
    let result = w.swap(None, Some(account.clone()), 1_000);
    assert!(
        error_text(&result).contains("BadTokenAccount"),
        "H016: uninitialised T22 account was not refused by checked_balance:\n{}",
        error_text(&result)
    );
    let result = w.swap(Some(account), None, 1_000);
    assert!(
        error_text(&result).contains("BadTokenAccount"),
        "H016: uninitialised T22 account was not refused by checked_balance:\n{}",
        error_text(&result)
    );
}

// ------------------------------------------------------------------ sweep_fees

/// Part 3: `balance` reads any >= 165-byte account at the pinned fee address. Crafted
/// accounts there never let `sweep_fees` move anything it should not; the real fee
/// account is swept in full once restored.
#[test]
fn sos2_h016_sweep_fees_balance_parser_cannot_be_abused() {
    for tp in [TOKEN, TOKEN_2022] {
        let mut w = world(tp);
        let mint = w.m.mint_a;
        let treasury_owner = w.env.treasury_owner;
        let treasury = w.env.create_ata(&treasury_owner, &mint, &tp);
        w.swap(None, None, 1_000_000).expect("swap to accrue fees");
        let fee_address = client::pda::fee_account(&w.m.market, &mint, &tp);
        let real_fee = w.env.svm.get_account(&fee_address).unwrap();
        let fees = w.env.token_balance(&fee_address);
        assert!(fees > 0, "no fee accrued");

        let sweep = |w: &mut World| {
            let ix = client::sweep_fees(&w.m.market, &mint, &tp, &treasury_owner, vec![]);
            w.env.send(&[ix], &[])
        };

        let mut variants: Vec<(&str, Account)> = vec![];
        let mut a = real_fee.clone();
        a.data = {
            let mut d = vec![0u8; 165];
            d[..32].copy_from_slice(mint.as_ref());
            d[64..72].copy_from_slice(&u64::MAX.to_le_bytes());
            d
        };
        variants.push(("token-owned, uninitialised, amount u64::MAX", a));
        let mut a = real_fee.clone();
        a.owner = client::SYSTEM_PROGRAM_ID;
        variants.push(("real contents but system-owned", a));
        let mut a = real_fee.clone();
        a.owner = EVIL_ID;
        variants.push(("real contents but foreign-owned", a));
        let mut a = real_fee.clone();
        a.data[108] = 2;
        variants.push(("real account, frozen", a));
        let mut a = real_fee.clone();
        a.data.truncate(164);
        variants.push(("164 bytes", a));
        let mut a = real_fee.clone();
        a.data.clear();
        variants.push(("empty", a));
        let mut a = w.env.svm.get_account(&mint).unwrap();
        a.data.resize(a.data.len().max(200), 0);
        a.data[64..72].copy_from_slice(&u64::MAX.to_le_bytes());
        variants.push(("the mint itself, extended to 200 bytes", a));
        let mut a = real_fee.clone();
        a.data.resize(355, 0);
        a.data[64..72].copy_from_slice(&u64::MAX.to_le_bytes());
        variants.push(("355-byte multisig length, huge amount", a));

        for (label, account) in variants {
            w.env.svm.set_account(fee_address, account).unwrap();
            let treasury_before = w.env.token_balance(&treasury);
            let result = sweep(&mut w);
            println!(
                "[{tp}] sweep with fee account `{label}` -> {}",
                error_text(&result).lines().next().unwrap()
            );
            assert!(
                result.is_err(),
                "H016 BUG: sweep_fees succeeded with a crafted fee account `{label}` under {tp}"
            );
            assert_eq!(
                w.env.token_balance(&treasury),
                treasury_before,
                "H016 `{label}`: the treasury balance moved"
            );
        }

        // A crafted account whose amount field reads zero is a no-op (nothing moves).
        let mut zero = real_fee.clone();
        zero.data[64..72].fill(0);
        zero.owner = client::SYSTEM_PROGRAM_ID;
        w.env.svm.set_account(fee_address, zero).unwrap();
        let treasury_before = w.env.token_balance(&treasury);
        sweep(&mut w).expect("zero amount is a no-op");
        assert_eq!(w.env.token_balance(&treasury), treasury_before);

        // Control: the real account is swept in full.
        w.env.svm.set_account(fee_address, real_fee).unwrap();
        sweep(&mut w).expect("real sweep");
        assert_eq!(w.env.token_balance(&treasury), fees);
        assert_eq!(w.env.token_balance(&fee_address), 0);
    }
}

// ------------------------------------------------------------------ decimals

/// Part 4: `decimals` reads byte 44 of any >= 82-byte account owned by the token
/// program. A market whose "mint" account is really a token account, or is truncated,
/// can't be swapped or swept: the token program refuses a non-mint in `transfer_checked`,
/// and a < 82 byte account is refused by the router.
#[test]
fn sos2_h016_decimals_parser_is_backstopped_by_the_token_program() {
    for tp in [TOKEN, TOKEN_2022] {
        let mut w = world(tp);
        let mint = w.m.mint_a;
        let treasury_owner = w.env.treasury_owner;
        let _treasury = w.env.create_ata(&treasury_owner, &mint, &tp);
        w.swap(None, None, 1_000_000).expect("swap to accrue fees");
        let real_mint = w.env.svm.get_account(&mint).unwrap();
        let real_trader = w.env.svm.get_account(&w.trader_a).unwrap();
        let watched = w.watched();

        // The mint slot holds a token account whose first 32 bytes name the mint
        // address itself, so the trader's account passes `checked_balance` against it.
        let mut as_token_account = real_trader.clone();
        as_token_account.data[..32].copy_from_slice(mint.as_ref());
        // A truncated mint.
        let mut short = real_mint.clone();
        short.data.truncate(81);
        // A multisig-length "mint".
        let mut multisig = real_mint.clone();
        multisig.data = vec![0u8; 355];
        multisig.data[44] = 6;

        for (label, account, needle) in [
            ("mint slot = token account", as_token_account, None),
            ("mint slot = 81 bytes", short, Some("WrongTokenProgram")),
            ("mint slot = 355-byte multisig", multisig, None),
        ] {
            w.env.svm.set_account(mint, account).unwrap();
            let before = balances(&w.env, &watched);
            let result = w.swap(None, None, 1_000);
            println!(
                "[{tp}] swap, {label} -> {}",
                error_text(&result).lines().next().unwrap()
            );
            assert!(
                result.is_err(),
                "H016 BUG: swap succeeded with `{label}` under {tp}"
            );
            if let Some(needle) = needle {
                assert!(
                    error_text(&result).contains(needle),
                    "{label}: {}",
                    error_text(&result)
                );
            }
            assert_eq!(
                balances(&w.env, &watched),
                before,
                "{label}: balances moved"
            );

            let ix = client::sweep_fees(&w.m.market, &mint, &tp, &treasury_owner, vec![]);
            let fee_address = client::pda::fee_account(&w.m.market, &mint, &tp);
            let fee_before = w.env.token_balance(&fee_address);
            let result = w.env.send(&[ix], &[]);
            println!(
                "[{tp}] sweep, {label} -> {}",
                error_text(&result).lines().next().unwrap()
            );
            assert!(
                result.is_err(),
                "H016 BUG: sweep succeeded with `{label}` under {tp}"
            );
            assert_eq!(w.env.token_balance(&fee_address), fee_before);
        }

        // Control: the real mint restored, everything works again.
        w.env.svm.set_account(mint, real_mint).unwrap();
        w.swap(None, None, 1_000).expect("swap with restored mint");
        let ix = client::sweep_fees(&w.m.market, &mint, &tp, &treasury_owner, vec![]);
        w.env.send(&[ix], &[]).expect("sweep with restored mint");
    }
}
