//! SOS audit #2, G003: how many remaining accounts a router swap handles within the
//! default 32 KiB heap and the transaction limits, and how it fails beyond that.
//!
//! A swap's remaining accounts are the strategy's accounts followed by the input mint's
//! transfer-hook accounts. The router copies every remaining account into the
//! strategy call, and the hook tail again into each of its input-side transfers (the
//! trade input, the Pipeline fee and the integrator fee). The bump heap never frees,
//! so hook accounts cost more heap than strategy accounts.
//!
//! The market is a CP pool whose input mint is a Token-2022 mint with the test transfer
//! hook (two real hook accounts: the hook program and its validation account). The
//! swap is stepped up one padding account at a time, with no compute-budget or
//! heap-frame request, until it fails. Measured against the current router binary:
//!
//! | Shape | Largest working swap | What binds next |
//! |---|---|---|
//! | Padding in the hook tail, new accounts each | 29 remaining (1 strategy + 28 hook) | heap |
//! | The same with an integrator fee | 28 remaining (1 + 27) | heap |
//! | Padding in the strategy accounts, one account repeated | 70 remaining (68 + 2) | heap |
//! | Padding in the strategy accounts, new accounts each | 47 remaining | 64-account lock limit |
//! | New accounts in a legacy transaction (no lookup table) | 14 remaining | 1,232-byte size limit |
//!
//! "New accounts each" past the legacy size limit is sent without the harness's size
//! check, standing in for a v0 transaction with an address lookup table (which loads
//! the same accounts), as `audit3_composition.rs` does. Repeated accounts cost one byte
//! each, so those swaps are real legacy transactions within 1,232 bytes.
//!
//! Beyond the heap ceiling the router runs out of memory: the default bump allocator
//! aborts the program (`ProgramFailedToComplete`, logged as "memory allocation failed,
//! out of memory" and "SBF program panicked"). That is an abort inside the SBF program,
//! not a typed router error, but the transaction fails as a whole: in the hook-tail
//! case it fails after the strategy has already paid out, and every balance and the
//! pool's state are unchanged afterwards. The other limits are refused before the
//! router runs. Run with `--nocapture` to print the figures.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use litesvm::types::FailedTransactionMetadata;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::{InstructionError, Transaction, TransactionError};

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const HOOK_PROGRAM_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ3");
const RESERVE: u64 = 1_000_000_000;
const SWAP: u64 = 1_000;
/// The CP strategy's one required account (the output vault) and the hook's two.
const BASE_REMAINING: usize = 3;
/// Stop stepping here; every shape binds well before it.
const MAX_PADDING: usize = 250;

/// Heap ceilings measured on the current router. A change that lowers one fails here,
/// so the figure published for integrators stays true.
const HOOK_TAIL_CEILING: usize = 29;
const HOOK_TAIL_WITH_INTEGRATOR_CEILING: usize = 28;
const STRATEGY_ACCOUNTS_CEILING: usize = 70;

fn validation(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], &HOOK_PROGRAM_ID).0
}

fn whitelist(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"whitelist", mint.as_ref()], &HOOK_PROGRAM_ID).0
}

/// A Token-2022 mint whose test hook approves every transfer.
fn hooked_mint(env: &mut Env) -> Pubkey {
    env.svm
        .add_program_from_file(HOOK_PROGRAM_ID, sbf_dir().join("test_hook.so"))
        .unwrap();
    let mint = env.create_mint(&TOKEN_2022, 6, None, Some(HOOK_PROGRAM_ID));
    let ix = Instruction::new_with_bytes(
        HOOK_PROGRAM_ID,
        b"tsthkini",
        vec![
            AccountMeta::new(env.admin.pubkey(), true),
            AccountMeta::new(validation(&mint), false),
            AccountMeta::new(whitelist(&mint), false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    );
    env.send(&[ix], &[]).expect("hook initialize");
    mint
}

fn hook_accounts(mint: &Pubkey) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(HOOK_PROGRAM_ID, false),
        AccountMeta::new_readonly(validation(mint), false),
    ]
}

struct World {
    env: Env,
    market: CpMarket,
    trader: Keypair,
    trader_a: Pubkey,
    trader_b: Pubkey,
    integrator_a: Pubkey,
}

fn world() -> World {
    let mut env = Env::new();
    let mint_a = hooked_mint(&mut env);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let market = setup_cp_market_with_mints(
        &mut env,
        mint_a,
        TOKEN_2022,
        mint_b,
        TOKEN,
        RESERVE,
        RESERVE,
        30,
        hook_accounts(&mint_a),
    );
    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN_2022, RESERVE);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let integrator_a = env.create_ata(&Pubkey::new_unique(), &mint_a, &TOKEN_2022);
    World {
        env,
        market,
        trader,
        trader_a,
        trader_b,
        integrator_a,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Padding {
    /// Extra accounts at the end of the input hook tail.
    HookTail,
    /// Extra accounts after the strategy's output vault.
    StrategyAccounts,
}

#[derive(Clone, Copy, PartialEq)]
enum Filler {
    /// A new address for every padding account.
    New,
    /// The hook program, repeated: one byte per account in the transaction.
    Repeated,
}

#[derive(Clone, Copy)]
struct Shape {
    padding: Padding,
    filler: Filler,
    integrator_fee: bool,
    /// Refuse transactions over 1,232 bytes (a legacy transaction with no lookup table).
    legacy_size: bool,
}

enum Failure {
    /// The transaction would exceed the legacy size limit; it was not sent.
    TooLarge(usize),
    Failed(FailedTransactionMetadata),
}

struct Ceiling {
    /// Remaining accounts (strategy plus hook) in the largest swap that worked.
    largest_working: usize,
    failure: Failure,
}

fn swap_ix(w: &World, shape: Shape, padding: usize) -> Instruction {
    let m = &w.market;
    let mut accounts = cp_swap_accounts(
        m,
        &w.trader.pubkey(),
        &m.mint_a,
        &w.trader_a,
        &m.mint_b,
        &w.trader_b,
    );
    let fillers = (0..padding).map(|_| match shape.filler {
        Filler::New => AccountMeta::new_readonly(Pubkey::new_unique(), false),
        Filler::Repeated => AccountMeta::new_readonly(HOOK_PROGRAM_ID, false),
    });
    let mut hooks = hook_accounts(&m.mint_a);
    match shape.padding {
        Padding::HookTail => hooks.extend(fillers),
        Padding::StrategyAccounts => accounts.strategy_accounts.extend(fillers),
    }
    let mut args = exact_in(SWAP, 0);
    args.input_hook_accounts = u8::try_from(hooks.len()).unwrap();
    accounts.input_hook_accounts = hooks;
    if shape.integrator_fee {
        accounts.integrator_fee_account = Some(w.integrator_a);
        args.integrator_fee_bps = 50;
    }
    client::swap(&accounts, &args)
}

/// Every balance and state a swap could touch.
fn snapshot(w: &World) -> Vec<Vec<u8>> {
    let m = &w.market;
    [
        w.trader_a,
        w.trader_b,
        m.vault_a,
        m.vault_b,
        m.pool,
        pda::fee_account(&m.market, &m.mint_a, &TOKEN_2022),
        w.integrator_a,
    ]
    .iter()
    .map(|account| w.env.account_data(account))
    .collect()
}

/// Steps the padding up from zero until the swap fails, and checks that the failing
/// swap changed nothing.
fn step_up(w: &mut World, shape: Shape) -> Ceiling {
    for padding in 0..=MAX_PADDING {
        let ix = swap_ix(w, shape, padding);
        w.env.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&w.env.admin.pubkey()),
            &[&w.env.admin, &w.trader],
            w.env.svm.latest_blockhash(),
        );
        let size = bincode::serialize(&tx).unwrap().len();
        if shape.legacy_size && size > MAX_TRANSACTION_SIZE {
            assert!(padding > 0, "the unpadded swap must fit");
            return Ceiling {
                largest_working: BASE_REMAINING + padding - 1,
                failure: Failure::TooLarge(size),
            };
        }
        let before = snapshot(w);
        if let Err(failed) = w.env.svm.send_transaction(tx) {
            assert!(padding > 0, "the unpadded swap must work");
            assert_eq!(snapshot(w), before, "a failed swap must change nothing");
            return Ceiling {
                largest_working: BASE_REMAINING + padding - 1,
                failure: Failure::Failed(failed),
            };
        }
    }
    panic!("no limit reached within {MAX_PADDING} padding accounts");
}

fn assert_out_of_memory(failure: &Failure) {
    let Failure::Failed(failed) = failure else {
        panic!("expected the router to run out of heap, not the size limit");
    };
    assert_eq!(
        failed.err,
        TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete),
        "expected an out-of-memory abort"
    );
    let logs = failed.meta.logs.join("\n");
    assert!(
        logs.contains("memory allocation failed, out of memory"),
        "expected an out-of-memory abort:\n{logs}"
    );
    let router_failed = format!("Program {} failed", client::PROGRAM_ID);
    assert!(
        logs.contains(&router_failed),
        "the router must be what failed:\n{logs}"
    );
}

/// The heap binds first for a hook-heavy swap: 29 remaining accounts work (28 with an
/// integrator fee, which copies the hook tail once more), and one more fails with an
/// out-of-memory abort after the strategy has paid out, leaving every balance as it
/// was. New accounts are used, as a lookup table would load them.
#[test]
fn g003_hook_tail_ceiling_is_the_heap_and_beyond_it_nothing_moves() {
    for (integrator_fee, documented) in [
        (false, HOOK_TAIL_CEILING),
        (true, HOOK_TAIL_WITH_INTEGRATOR_CEILING),
    ] {
        let mut w = world();
        let shape = Shape {
            padding: Padding::HookTail,
            filler: Filler::New,
            integrator_fee,
            legacy_size: false,
        };
        let ceiling = step_up(&mut w, shape);
        println!(
            "G003 hook tail (integrator fee: {integrator_fee}): {} remaining accounts \
             (1 strategy + {} hook) fit the 32 KiB heap; one more runs out of memory",
            ceiling.largest_working,
            ceiling.largest_working - 1
        );
        assert_out_of_memory(&ceiling.failure);
        if let Failure::Failed(failed) = &ceiling.failure {
            let strategy_succeeded = format!("Program {CP_PROGRAM_ID} success");
            assert!(
                failed.meta.logs.contains(&strategy_succeeded),
                "the failing swap runs out of heap after the strategy paid out"
            );
        }
        assert!(
            ceiling.largest_working >= documented,
            "hook-tail ceiling fell to {} (documented {documented})",
            ceiling.largest_working
        );
    }
}

/// Strategy accounts are copied once, so many more fit: repeated accounts in a real
/// legacy transaction reach 70 remaining accounts before the heap runs out. With new
/// accounts, the 64-account lock limit refuses the transaction first, before the
/// router runs, so the heap is never reached.
#[test]
fn g003_strategy_account_ceiling_heap_with_repeats_lock_limit_with_new_accounts() {
    let mut w = world();
    let repeated = Shape {
        padding: Padding::StrategyAccounts,
        filler: Filler::Repeated,
        integrator_fee: false,
        legacy_size: true,
    };
    let ceiling = step_up(&mut w, repeated);
    println!(
        "G003 strategy accounts (one account repeated, legacy transaction): {} remaining \
         accounts fit the 32 KiB heap; one more runs out of memory",
        ceiling.largest_working
    );
    assert_out_of_memory(&ceiling.failure);
    assert!(
        ceiling.largest_working >= STRATEGY_ACCOUNTS_CEILING,
        "strategy-account ceiling fell to {} (documented {STRATEGY_ACCOUNTS_CEILING})",
        ceiling.largest_working
    );

    let mut w = world();
    let new_accounts = Shape {
        filler: Filler::New,
        legacy_size: false,
        ..repeated
    };
    let ceiling = step_up(&mut w, new_accounts);
    println!(
        "G003 strategy accounts (new accounts, lookup-table stand-in): {} remaining \
         accounts; one more exceeds the transaction's account lock limit",
        ceiling.largest_working
    );
    let Failure::Failed(failed) = &ceiling.failure else {
        panic!("expected the lock limit");
    };
    assert_eq!(failed.err, TransactionError::TooManyAccountLocks);
    assert!(ceiling.largest_working < STRATEGY_ACCOUNTS_CEILING);
}

/// Without a lookup table, the 1,232-byte legacy size limit binds long before the
/// heap: each new account costs 33 bytes.
#[test]
fn g003_legacy_transaction_without_lookup_table_binds_on_size_first() {
    let mut w = world();
    let shape = Shape {
        padding: Padding::HookTail,
        filler: Filler::New,
        integrator_fee: false,
        legacy_size: true,
    };
    let ceiling = step_up(&mut w, shape);
    let Failure::TooLarge(size) = ceiling.failure else {
        panic!("expected the legacy size limit to bind before the heap");
    };
    println!(
        "G003 legacy transaction, new accounts: {} remaining accounts fit in 1,232 bytes \
         (the next is {size} bytes)",
        ceiling.largest_working
    );
    assert!(ceiling.largest_working < HOOK_TAIL_CEILING);
}
