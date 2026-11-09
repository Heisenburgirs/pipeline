//! SOS audit #2, H037: a strategy closes and re-creates (or reassigns) its vault during the
//! call.
//!
//! No prebuilt fixture closes a token account mid-call, and LiteSVM cannot change an account
//! in the middle of a CPI. So these tests do NOT run an in-call close/re-create. They show,
//! with crafted account states put in place BETWEEN transactions, what the router's single
//! account-reading function (`token::checked_balance`, used identically for the pre-call
//! snapshot and the post-call re-read) does with each shape a closed / reassigned /
//! re-created vault can have, and what the settlement does with the numbers a re-created
//! vault produces:
//!
//! 1. Closed, reassigned, wrong-mint, zeroed, truncated and wrong-program shapes are refused
//!    with `BadTokenAccount` and nothing moves (never read as a zero balance).
//! 2. A vault re-created with a different balance, authority, delegate or close authority
//!    settles exactly as before: the fee base never drops below the trader's real spend and
//!    never exceeds `amount_in`.
//! 3. `settle` on every combination of (vault before, vault after) a re-creation can
//!    produce keeps `net_sent` within `[trader_spent, amount_in]`.
//! 4. The fee ATA cannot be revived by anyone but the associated token program; a missing
//!    one refuses swaps and sweeps atomically and is recreated by `init_fee_account`.

#![allow(clippy::result_large_err, clippy::vec_init_then_push)]

mod bok_swap_util;
mod common;

use bok_swap_util::*;
use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use pipeline_strategy_interface::{fees, settlement};
use solana_account::Account;
use solana_signer::Signer;

const RESERVE: u64 = 1_000_000_000;
const AMOUNT: u64 = 1_000_000;
const SYSTEM: Pubkey = client::SYSTEM_PROGRAM_ID;

struct Setup {
    env: Env,
    m: EvilMarket,
    trader: solana_keypair::Keypair,
    trader_a: Pubkey,
    trader_b: Pubkey,
    fee_a: Pubkey,
    original_vault: Account,
}

fn setup(tp_a: Pubkey) -> Setup {
    let mut env = Env::new();
    let mint_a = env.create_mint(&tp_a, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let m = evil_market(&mut env, mint_a, tp_a, mint_b, None, 10 * RESERVE);
    let (trader, trader_a) = env.trader_with(&mint_a, &tp_a, 100 * RESERVE);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let fee_a = pda::fee_account(&m.market, &mint_a, &tp_a);
    let original_vault = env.svm.get_account(&m.vault_a).expect("vault exists");
    Setup {
        env,
        m,
        trader,
        trader_a,
        trader_b,
        fee_a,
        original_vault,
    }
}

impl Setup {
    fn watched(&self) -> [Pubkey; 4] {
        [self.trader_a, self.trader_b, self.m.vault_a, self.fee_a]
    }

    fn swap(&mut self) -> TxResult {
        let accounts = evil_swap_accounts(
            &self.m,
            &self.trader.pubkey(),
            &self.trader_a,
            &self.trader_b,
        );
        send_swap(
            &mut self.env,
            client::swap(&accounts, &evil_args(HONEST, AMOUNT, 0)),
            &self.trader,
        )
    }

    fn put_vault(&mut self, account: Account) {
        self.env.svm.set_account(self.m.vault_a, account).unwrap();
    }

    fn restore_vault(&mut self) {
        let account = self.original_vault.clone();
        self.put_vault(account);
    }
}

/// A freshly "re-created" token account at the vault's address, as the token program would
/// build it for a close followed by create_account + InitializeAccount3.
fn recreated(
    original: &Account,
    amount: u64,
    owner: &Pubkey,
    delegate: bool,
    close: bool,
) -> Account {
    let mut account = original.clone();
    let data = &mut account.data;
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    // delegate: COption tag (72..76), key (76..108), delegated_amount (121..129)
    data[72..76].copy_from_slice(&(delegate as u32).to_le_bytes());
    if delegate {
        data[76..108].copy_from_slice(Pubkey::new_unique().as_ref());
        data[121..129].copy_from_slice(&u64::MAX.to_le_bytes());
    } else {
        data[76..108].fill(0);
        data[121..129].fill(0);
    }
    // close authority: COption tag (129..133), key (133..165)
    data[129..133].copy_from_slice(&(close as u32).to_le_bytes());
    if close {
        data[133..165].copy_from_slice(Pubkey::new_unique().as_ref());
    } else {
        data[133..165].fill(0);
    }
    account
}

// ------------------------------------------------------------------------ 1. shapes

/// Every shape a closed or reassigned vault can take is refused by `checked_balance`
/// (`BadTokenAccount`), never read as a balance, and nothing moves. The same function reads
/// the vault after the strategy call, so the post-call re-read refuses the same shapes.
fn rejected_shapes(tp_a: Pubkey) {
    let mut s = setup(tp_a);
    s.swap()
        .expect("setup: a swap on the untouched market settles");

    let original = s.original_vault.clone();
    let other_token_program = if tp_a == TOKEN { TOKEN_2022 } else { TOKEN };
    let mut cases: Vec<(&str, Account)> = Vec::new();

    // Closed in the same instruction: no lamports, no data, system owner.
    cases.push((
        "closed (0 lamports, empty, system)",
        Account {
            lamports: 0,
            data: vec![],
            owner: SYSTEM,
            executable: false,
            rent_epoch: 0,
        },
    ));
    // Closed but rent still there (Token's close assigns to the system program).
    cases.push((
        "closed (rent kept, empty, system)",
        Account {
            lamports: 2_039_280,
            data: vec![],
            owner: SYSTEM,
            executable: false,
            rent_epoch: 0,
        },
    ));
    // Older close behaviour: data zeroed, owner unchanged.
    cases.push((
        "zeroed 165 bytes, token-owned",
        Account {
            data: vec![0; 165],
            ..original.clone()
        },
    ));
    // Token-owned, data truncated to nothing.
    cases.push((
        "token-owned, empty data",
        Account {
            data: vec![],
            ..original.clone()
        },
    ));
    // Token-owned, shorter than a token account.
    cases.push((
        "token-owned, 164 bytes",
        Account {
            data: original.data[..164].to_vec(),
            ..original.clone()
        },
    ));
    // Token-owned multisig length.
    let mut multisig = original.data.clone();
    multisig.resize(355, 0);
    cases.push((
        "token-owned, 355 bytes (multisig length)",
        Account {
            data: multisig,
            ..original.clone()
        },
    ));
    // Reassigned to the hostile strategy with the token account bytes intact.
    cases.push((
        "reassigned to the strategy, bytes intact",
        Account {
            owner: EVIL_ID,
            ..original.clone()
        },
    ));
    // Owned by the other token program.
    cases.push((
        "owned by the other token program",
        Account {
            owner: other_token_program,
            ..original.clone()
        },
    ));
    // Re-created with a different mint.
    let mut wrong_mint = original.clone();
    wrong_mint.data[..32].copy_from_slice(s.m.mint_b.as_ref());
    cases.push(("re-created with another mint", wrong_mint));
    // Re-created uninitialised (all zero mint) but with an amount.
    let mut zero_mint = original.clone();
    zero_mint.data[..32].fill(0);
    cases.push(("re-created with a zero mint", zero_mint));
    // Mint-like 82-byte account owned by the token program.
    cases.push((
        "token-owned, 82 bytes (mint-sized)",
        Account {
            data: original.data[..82].to_vec(),
            ..original.clone()
        },
    ));
    // Token-2022 style: 166 bytes whose type byte is "mint" (1).
    let mut mint_typed = original.data.clone();
    mint_typed.resize(166, 0);
    mint_typed[165] = 1;
    cases.push((
        "166 bytes, type byte = mint",
        Account {
            data: mint_typed,
            ..original.clone()
        },
    ));

    let watched = s.watched();
    for (label, account) in cases {
        s.put_vault(account);
        let before = balances(&s.env, &watched);
        let result = s.swap();
        assert!(
            result.is_err(),
            "H037 {label}: a swap with this vault must not settle"
        );
        assert_fails_with(result, "Not a valid token account");
        assert_eq!(
            balances(&s.env, &watched),
            before,
            "H037 {label}: a refused swap moved tokens"
        );
        s.restore_vault();
    }

    // After restoring, the market trades again: the failures left no residue.
    s.swap()
        .expect("setup: market trades after the crafted states are removed");
}

#[test]
fn h037_closed_reassigned_or_misshapen_vault_is_refused_spl() {
    rejected_shapes(TOKEN);
}

#[test]
fn h037_closed_reassigned_or_misshapen_vault_is_refused_token_2022() {
    rejected_shapes(TOKEN_2022);
}

// -------------------------------------------------------------------- 2. re-created

/// A vault re-created at the same address with another balance, authority, delegate or close
/// authority is just another token account of the right mint. The router charges on what the
/// trader spent (here `AMOUNT`), the fee lands in the fee ATA, and the trader's debit is
/// `AMOUNT + fee`.
fn recreated_vault_settles_on_the_real_spend(tp_a: Pubkey) {
    let mut s = setup(tp_a);
    s.swap()
        .expect("setup: a swap on the untouched market settles");
    let state = s.m.state;
    let original = s.original_vault.clone();
    let watched = s.watched();
    let mut cases = 0;

    for amount in [
        0u64,
        1,
        AMOUNT - 1,
        AMOUNT,
        AMOUNT + 1,
        50 * AMOUNT,
        u64::MAX - AMOUNT,
    ] {
        for owner in [state, Pubkey::new_unique()] {
            for (delegate, close) in [(false, false), (true, true)] {
                s.put_vault(recreated(&original, amount, &owner, delegate, close));
                let label = format!(
                    "tp {tp_a} amount {amount} owner_is_state {} delegate {delegate} close {close}",
                    owner == state
                );
                let before = balances(&s.env, &watched);
                let meta = s.swap().unwrap_or_else(|e| {
                    panic!(
                        "{label}: setup swap must succeed: {:?}\n{}",
                        e.err,
                        e.meta.logs.join("\n")
                    )
                });
                let after = balances(&s.env, &watched);
                let (r, _) = reported(&meta);
                let fee = fees::pipeline_fee(AMOUNT);
                assert_eq!(
                    r.net_sent, AMOUNT,
                    "H037 {label}: net_sent below the trader's real spend"
                );
                assert_eq!(r.pipeline_fee, fee, "H037 {label}: pipeline fee");
                assert_eq!(
                    before[0] - after[0],
                    AMOUNT + fee,
                    "H037 {label}: trader debit"
                );
                assert_eq!(
                    after[3] - before[3],
                    fee,
                    "H037 {label}: the fee ATA received the recorded fee"
                );
                assert_eq!(
                    after[2] - before[2],
                    AMOUNT,
                    "H037 {label}: the vault kept the input"
                );
                cases += 1;
            }
        }
    }
    println!("tp {tp_a}: {cases} re-created-vault cases");
    assert_eq!(cases, 28);
}

#[test]
fn h037_recreated_vault_settles_on_the_real_spend_spl() {
    recreated_vault_settles_on_the_real_spend(TOKEN);
}

#[test]
fn h037_recreated_vault_settles_on_the_real_spend_token_2022() {
    recreated_vault_settles_on_the_real_spend(TOKEN_2022);
}

// ---------------------------------------------------------------- 3. settle's bounds

/// Whatever a close and re-create does to the vault balance (to zero, to anything up to
/// `u64::MAX`, or below what it was), the fee base stays between the trader's spend and
/// `amount_in`: re-creation can only raise it to `amount_in`, never lower it below what the
/// trader paid. This is pure arithmetic on `settle`, not an in-call run.
#[test]
fn h037_vault_balance_changes_never_lower_the_fee_base_below_the_trader_spend() {
    let edges = [
        0u64,
        1,
        2,
        999_999,
        1_000_000,
        1_000_001,
        u64::MAX / 2,
        u64::MAX - 1,
        u64::MAX,
    ];
    let amount_in = 1_000_000u64;
    let mut checked = 0u64;
    for &vault_before in &edges {
        for &vault_after in &edges {
            for &input_before in &edges {
                for spent in [0u64, 1, 990_000, 1_000_000] {
                    let Some(input_after) = input_before.checked_sub(spent) else {
                        continue;
                    };
                    let measured = settlement::Measured {
                        input_before,
                        input_after,
                        output_before: 0,
                        output_after: 1,
                        vault_before,
                        vault_after,
                    };
                    let result = settlement::settle(&measured, amount_in, 0, 0);
                    match result {
                        Ok(s) => {
                            assert!(
                                s.net_sent >= spent,
                                "H037: net_sent {} below trader spend {spent} \
                                 (vault {vault_before} -> {vault_after})",
                                s.net_sent
                            );
                            assert!(s.net_sent <= amount_in, "H037: net_sent above amount_in");
                            assert!(s.pipeline_fee >= fees::pipeline_fee(spent));
                            assert!(s.pipeline_fee <= fees::pipeline_fee(amount_in));
                        }
                        Err(settlement::SettleError::OutputWithoutInput) => {
                            // Nothing spent and nothing kept: refused, not settled free.
                            assert_eq!(spent, 0);
                            assert!(vault_after <= vault_before);
                        }
                        Err(e) => panic!("H037: unexpected refusal {e:?}"),
                    }
                    checked += 1;
                }
            }
        }
    }
    println!("{checked} settle cases checked");
}

// ----------------------------------------------------------------- 4. fee ATA revival

/// The fee ATA's address is the associated token program's PDA, so only that program can
/// create an account there, and only the fee authority (which signs only in `sweep_fees`) can
/// close it. A strategy cannot close or revive it. This models the worst case with a crafted
/// state: the ATA is gone. Swaps and sweeps then refuse atomically; `init_fee_account`
/// re-creates it as a proper token account; fees then flow again and sweep exactly.
#[test]
fn h037_missing_fee_ata_refuses_atomically_and_is_recreated_by_init() {
    let mut s = setup(TOKEN);
    s.swap().expect("setup: swap settles");
    let fee_account = s.env.svm.get_account(&s.fee_a).expect("fee ATA exists");
    assert!(s.env.token_balance(&s.fee_a) > 0, "setup: fees collected");

    // Crafted: the fee ATA is closed (its collected fees are gone, as if swept elsewhere).
    s.env
        .svm
        .set_account(
            s.fee_a,
            Account {
                lamports: 0,
                data: vec![],
                owner: SYSTEM,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    let watched = s.watched();
    let before = balances(&s.env, &watched);
    let result = s.swap();
    assert!(
        result.is_err(),
        "H037: a swap must not settle while its fee account is missing"
    );
    assert_eq!(
        balances(&s.env, &watched),
        before,
        "H037: a refused swap moved tokens"
    );

    let treasury_owner = s.env.treasury_owner;
    let treasury = s.env.create_ata(&treasury_owner, &s.m.mint_a, &TOKEN);
    let sweep = client::sweep_fees(&s.m.market, &s.m.mint_a, &TOKEN, &treasury_owner, vec![]);
    assert!(
        s.env.send(&[sweep], &[]).is_err(),
        "H037: sweep must not treat a missing fee account as an empty one"
    );

    // Anyone re-creates it; it is the proper ATA of the fee authority again.
    let payer = s.env.admin.pubkey();
    s.env
        .send(
            &[client::init_fee_account(
                &payer,
                &s.m.market,
                &s.m.mint_a,
                &TOKEN,
            )],
            &[],
        )
        .expect("init_fee_account recreates the fee ATA");
    let recreated = s.env.svm.get_account(&s.fee_a).expect("recreated");
    assert_eq!(recreated.owner, fee_account.owner);
    assert_eq!(recreated.data.len(), fee_account.data.len());
    assert_eq!(
        &recreated.data[32..64],
        pda::fee_authority(&s.m.market).as_ref(),
        "H037: recreated fee ATA is owned by the fee authority"
    );
    assert_eq!(s.env.token_balance(&s.fee_a), 0);

    let meta = s.swap().expect("swap settles again");
    let (r, _) = reported(&meta);
    assert_eq!(s.env.token_balance(&s.fee_a), r.pipeline_fee);
    let sweep = client::sweep_fees(&s.m.market, &s.m.mint_a, &TOKEN, &treasury_owner, vec![]);
    s.env.send(&[sweep], &[]).expect("sweep");
    assert_eq!(s.env.token_balance(&treasury), r.pipeline_fee);
    assert_eq!(s.env.token_balance(&s.fee_a), 0);
}
