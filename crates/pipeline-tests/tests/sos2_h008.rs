//! SOS audit #2, H008: in `swap` the strategy receives exactly one signature, the
//! market PDA's. No outer signer (the trader, the transaction fee payer, a co-signer, a
//! program-signed PDA trader) and no router authority (`fee_authority`) reaches it,
//! however the caller duplicates them into the strategy's accounts.
//!
//! Cases not already covered by `adversarial.rs`, `sos_h009.rs` and `bok_markets.rs`
//! (which pin the plain "trader repeated as a writable signer" case):
//!   * the trader is also the transaction fee payer, and repeated writable;
//!   * the trader is a PDA signed by a calling program (an aggregator), repeated;
//!   * `fee_account` and `fee_authority` placed in the strategy's accounts, together
//!     with a second transaction signer;
//!   * the trader's own address is the market state (crafted with `set_account`).

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

/// Evil strategy swap behaviours (programs/test-strategy-evil/src/lib.rs).
const HONEST: u8 = 0;
const TAKE_MORE: u8 = 1;
const STEAL_FEES: u8 = 6;
const TAKE_LAMPORTS: u8 = 9;

/// The evil market state's length: four keys, bump, info mode.
const STATE_LEN: usize = 32 * 4 + 2;

const START: u64 = 10_000_000;
const AMOUNT: u64 = 1_000;

struct EvilMarket {
    state: Pubkey,
    market: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
}

impl EvilMarket {
    fn fee_account_a(&self) -> Pubkey {
        pda::fee_account(&self.market, &self.mint_a, &TOKEN)
    }
}

fn evil_pda(mint_a: &Pubkey, mint_b: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID)
}

fn finish_market(env: &mut Env, m: &EvilMarket) {
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &m.market, &m.mint_a, &TOKEN),
            client::init_fee_account(&payer, &m.market, &m.mint_b, &TOKEN),
        ],
        &[],
    )
    .expect("init fee accounts");
    env.mint_to(&m.mint_b, &TOKEN, &m.vault_b, 1_000_000_000);
}

/// A normal evil market (state at its PDA), with liquidity and fee accounts.
fn setup() -> (Env, EvilMarket) {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let (state, _) = evil_pda(&mint_a, &mint_b);
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
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
    let m = EvilMarket {
        state,
        market: pda::market(&EVIL_ID, &state),
        mint_a,
        mint_b,
        vault_a,
        vault_b,
    };
    finish_market(&mut env, &m);
    (env, m)
}

/// Trader token accounts (A funded with `START`, B empty) for any owner address.
fn trader_accounts(env: &mut Env, m: &EvilMarket, owner: &Pubkey) -> (Pubkey, Pubkey) {
    let a = env.create_ata(owner, &m.mint_a, &TOKEN);
    env.mint_to(&m.mint_a, &TOKEN, &a, START);
    let b = env.create_ata(owner, &m.mint_b, &TOKEN);
    (a, b)
}

fn swap_ix(
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
        input_token_program: TOKEN,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts,
        input_hook_accounts: vec![],
    };
    let mut args = exact_in(AMOUNT, 0);
    args.extra = vec![behaviour];
    client::swap(&accounts, &args)
}

/// Sends with `payer` as the transaction fee payer (and only signer besides `extra`).
fn send_paid_by(env: &mut Env, ixs: &[Instruction], payer: &Keypair) -> TxResult {
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        ixs,
        Some(&payer.pubkey()),
        &[payer],
        env.svm.latest_blockhash(),
    );
    env.svm.send_transaction(tx)
}

/// The safe outcome: the transaction fails and its logs (or error) contain `needle`.
fn assert_refused(result: TxResult, needle: &str, case: &str) {
    match result {
        Ok(meta) => panic!(
            "H008 VIOLATED ({case}): the hostile strategy's swap succeeded, so it used a \
             signature other than the market PDA's:\n{}",
            meta.logs.join("\n")
        ),
        Err(failed) => {
            let logs = failed.meta.logs.join("\n");
            assert!(
                logs.contains(needle) || format!("{:?}", failed.err).contains(needle),
                "H008 ({case}): expected failure containing {needle:?}, got {:?}:\n{logs}",
                failed.err
            );
        }
    }
}

fn lamport_extras(trader: &Pubkey) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        AccountMeta::new(*trader, true),
    ]
}

// ---------------------------------------------------------------------------------------
// The trader is also the transaction fee payer
// ---------------------------------------------------------------------------------------

/// The trader pays the transaction fee (so it is a writable signer for the whole
/// message) and is repeated, writable and signing, in the strategy's accounts. The
/// strategy still gets it unsigned: it can take neither tokens nor lamports. The only
/// lamports the trader loses are the transaction fee, the same as for an honest swap.
#[test]
fn h008_trader_as_fee_payer_repeated_writable_gains_no_signature() {
    let (mut env, m) = setup();
    let trader = env.funded_keypair();
    let key = trader.pubkey();
    let (a, b) = trader_accounts(&mut env, &m, &key);

    // Setup check: an honest swap paid by the trader works; it fixes the tx fee.
    let lamports = env.lamports(&key);
    let honest = swap_ix(&m, &key, &a, &b, HONEST, vec![AccountMeta::new(key, true)]);
    send_paid_by(&mut env, &[honest], &trader).expect("honest swap paid by the trader");
    let tx_fee = lamports - env.lamports(&key);
    assert!(tx_fee > 0 && tx_fee < 100_000, "tx fee {tx_fee}");
    let after_honest_a = env.token_balance(&a);
    let after_honest_b = env.token_balance(&b);
    assert_eq!(after_honest_b, AMOUNT);

    let cases = [
        ("TakeMore", TAKE_MORE, vec![AccountMeta::new(key, true)]),
        ("TakeLamports", TAKE_LAMPORTS, lamport_extras(&key)),
    ];
    for (name, behaviour, extra) in cases {
        let lamports = env.lamports(&key);
        let ix = swap_ix(&m, &key, &a, &b, behaviour, extra);
        let result = send_paid_by(&mut env, &[ix], &trader);
        assert_refused(result, "privilege escalated", name);
        assert_eq!(
            lamports - env.lamports(&key),
            tx_fee,
            "H008 ({name}): the trader lost more than the transaction fee"
        );
        assert_eq!(env.token_balance(&a), after_honest_a, "H008 ({name})");
        assert_eq!(env.token_balance(&b), after_honest_b, "H008 ({name})");
    }
}

// ---------------------------------------------------------------------------------------
// The trader is a PDA signed by a calling program
// ---------------------------------------------------------------------------------------

/// An aggregator program (a second copy of the evil program, which forwards a router
/// instruction signed by its own `pipeline-register` PDA) swaps with that PDA as the
/// trader, and repeats the PDA in the strategy's accounts. The PDA signs for the router
/// (the honest swap succeeds), but its signature stops there: the strategy can't spend
/// the PDA's tokens or lamports.
#[test]
fn h008_program_signed_pda_trader_signature_stops_at_the_router() {
    let (mut env, m) = setup();
    let aggregator = Pubkey::new_unique();
    env.svm
        .add_program_from_file(aggregator, sbf_dir().join("test_strategy_evil.so"))
        .expect("aggregator program");
    let trader = pda::register(&aggregator);
    env.svm.airdrop(&trader, SOL).unwrap();
    let (a, b) = trader_accounts(&mut env, &m, &trader);

    let forward = |inner: Instruction| -> Instruction {
        let mut accounts = vec![AccountMeta::new_readonly(client::PROGRAM_ID, false)];
        accounts.extend(inner.accounts.iter().map(|meta| AccountMeta {
            // The PDA can't sign the transaction; the aggregator signs for it.
            is_signer: meta.is_signer && meta.pubkey != trader,
            ..meta.clone()
        }));
        let mut data = b"evilfwd!".to_vec();
        data.extend_from_slice(&inner.data);
        Instruction::new_with_bytes(aggregator, &data, accounts)
    };

    // Setup check: the PDA trader can swap through the aggregator.
    let honest = swap_ix(
        &m,
        &trader,
        &a,
        &b,
        HONEST,
        vec![AccountMeta::new(trader, true)],
    );
    env.send(&[forward(honest)], &[])
        .expect("honest swap with a program-signed PDA trader");
    let after_honest_a = env.token_balance(&a);
    assert_eq!(env.token_balance(&b), AMOUNT);
    assert!(after_honest_a < START);

    let cases = [
        ("TakeMore", TAKE_MORE, vec![AccountMeta::new(trader, true)]),
        ("TakeLamports", TAKE_LAMPORTS, lamport_extras(&trader)),
    ];
    for (name, behaviour, extra) in cases {
        let lamports = env.lamports(&trader);
        let ix = swap_ix(&m, &trader, &a, &b, behaviour, extra);
        let result = env.send(&[forward(ix)], &[]);
        assert_refused(result, "privilege escalated", name);
        assert_eq!(env.lamports(&trader), lamports, "H008 ({name})");
        assert_eq!(env.token_balance(&a), after_honest_a, "H008 ({name})");
        assert_eq!(env.token_balance(&b), AMOUNT, "H008 ({name})");
    }
}

// ---------------------------------------------------------------------------------------
// Fee authority, fee account and a co-signer in the strategy's accounts
// ---------------------------------------------------------------------------------------

/// The market's fee account and fee authority, plus a second transaction signer, are
/// handed to the strategy (all writable, the co-signer flagged signing). Moving the
/// collected fees fails: the market PDA, the only signer the strategy has, is not the
/// fee account's owner, and the router never signs for the fee authority in `swap`.
/// Neither the fees nor the co-signer's lamports move.
#[test]
fn h008_fee_authority_and_cosigner_in_strategy_accounts_give_no_fee_access() {
    let (mut env, m) = setup();
    let trader = env.funded_keypair();
    let key = trader.pubkey();
    let (a, b) = trader_accounts(&mut env, &m, &key);
    let fee_account = m.fee_account_a();
    let fee_authority = pda::fee_authority(&m.market);

    // Setup check: an honest swap collects a fee.
    let honest = swap_ix(&m, &key, &a, &b, HONEST, vec![]);
    env.send(&[honest], &[&trader]).expect("honest swap");
    let collected = env.token_balance(&fee_account);
    assert!(collected > 0, "setup: no fee collected");

    let cosigner = env.funded_keypair();
    let cosigner_lamports = env.lamports(&cosigner.pubkey());
    let extras = vec![
        AccountMeta::new(fee_account, false),
        AccountMeta::new(fee_authority, false),
        AccountMeta::new(cosigner.pubkey(), true),
    ];
    let trader_a = env.token_balance(&a);

    let ix = swap_ix(&m, &key, &a, &b, STEAL_FEES, extras.clone());
    let result = env.send(&[ix], &[&trader, &cosigner]);
    assert_refused(result, "owner does not match", "StealFees");
    assert_eq!(
        env.token_balance(&fee_account),
        collected,
        "H008: the strategy moved Pipeline's fees"
    );

    // The same accounts with the trader-token and lamport attacks.
    let mut take_more = extras.clone();
    take_more.push(AccountMeta::new(key, true));
    let ix = swap_ix(&m, &key, &a, &b, TAKE_MORE, take_more);
    let result = env.send(&[ix], &[&trader, &cosigner]);
    assert_refused(result, "privilege escalated", "TakeMore with fee accounts");

    let mut take_lamports = lamport_extras(&key);
    take_lamports.extend(extras);
    let ix = swap_ix(&m, &key, &a, &b, TAKE_LAMPORTS, take_lamports);
    let result = env.send(&[ix], &[&trader, &cosigner]);
    assert_refused(
        result,
        "privilege escalated",
        "TakeLamports with fee accounts",
    );

    assert_eq!(
        env.token_balance(&fee_account),
        collected,
        "H008: fees moved"
    );
    assert_eq!(env.token_balance(&a), trader_a, "H008: trader input moved");
    assert_eq!(env.token_balance(&b), AMOUNT, "H008: trader output moved");
    assert_eq!(
        env.lamports(&cosigner.pubkey()),
        cosigner_lamports,
        "H008: the co-signer's lamports moved"
    );
}

// ---------------------------------------------------------------------------------------
// The trader's address is the market state (crafted)
// ---------------------------------------------------------------------------------------

/// A market whose state account sits at the trader's own keypair address (planted with
/// `set_account` as an evil-strategy-owned state, then registered with the permissionless
/// `register_existing_market`), with both vaults owned by that address. The trader signs
/// the swap while also being `market_state`, which the router passes writable to the
/// strategy. The evil strategy pays out of its output vault with `market_state` as the
/// authority, so even its *honest* behaviour succeeds only if the trader's signature
/// reached it. The safe outcome is that the payout CPI is refused as a privilege
/// escalation inside the strategy, and nothing moves.
#[test]
fn h008_trader_as_market_state_gains_no_signature() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let trader = Keypair::new();
    let key = trader.pubkey();
    let vault_a = env.create_ata(&key, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&key, &mint_b, &TOKEN);

    let mut data = Vec::with_capacity(STATE_LEN);
    for k in [mint_a, mint_b, vault_a, vault_b] {
        data.extend_from_slice(k.as_ref());
    }
    // A valid bump for the strategy's usual state PDA, so its signer seeds derive an
    // address (that PDA, which is not the trader's) rather than failing outright.
    data.push(evil_pda(&mint_a, &mint_b).1);
    data.push(0);
    env.svm
        .set_account(
            key,
            Account {
                lamports: SOL,
                data,
                owner: EVIL_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    let payer = env.admin.pubkey();
    env.send(
        &[client::register_existing_market(
            &payer,
            &EVIL_ID,
            &key,
            vec![],
        )],
        &[],
    )
    .expect("register the planted market");
    let m = EvilMarket {
        state: key,
        market: pda::market(&EVIL_ID, &key),
        mint_a,
        mint_b,
        vault_a,
        vault_b,
    };
    assert_eq!(
        env.market(&m.market).market_state,
        key,
        "setup: market bound to the trader's address"
    );
    finish_market(&mut env, &m);
    // The trader's associated token accounts are the vaults, so its trading accounts
    // are planted at fresh addresses (still owned by the trader).
    let a = Keypair::new().pubkey();
    let b = Keypair::new().pubkey();
    let a = plant_token_account(&mut env, a, &mint_a, &key, START);
    let b = plant_token_account(&mut env, b, &mint_b, &key, 0);
    let vault_b_before = env.token_balance(&vault_b);
    let lamports = env.lamports(&key);

    for (name, behaviour, extra) in [
        ("Honest payout signed as market_state", HONEST, vec![]),
        (
            "Honest payout, trader repeated",
            HONEST,
            vec![AccountMeta::new(key, true)],
        ),
        ("TakeLamports", TAKE_LAMPORTS, lamport_extras(&key)),
    ] {
        let ix = swap_ix(&m, &key, &a, &b, behaviour, extra);
        let result = env.send(&[ix], &[&trader]);
        if let Err(failed) = &result {
            let reached = failed
                .meta
                .logs
                .iter()
                .any(|l| l.contains(&format!("Program {EVIL_ID} invoke [2]")));
            assert!(
                reached,
                "H008 setup ({name}): the swap never reached the strategy:\n{}",
                failed.meta.logs.join("\n")
            );
        }
        assert_refused(result, "privilege escalated", name);
        assert_eq!(env.lamports(&key), lamports, "H008 ({name})");
        assert_eq!(env.token_balance(&a), START, "H008 ({name})");
        assert_eq!(env.token_balance(&b), 0, "H008 ({name})");
        assert_eq!(env.token_balance(&vault_b), vault_b_before, "H008 ({name})");
    }
}

/// Writes a plain 165-byte SPL token account (initialized) at `address`.
fn plant_token_account(
    env: &mut Env,
    address: Pubkey,
    mint: &Pubkey,
    owner: &Pubkey,
    amount: u64,
) -> Pubkey {
    let mut data = vec![0u8; 165];
    data[..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1;
    let lamports = env.svm.minimum_balance_for_rent_exemption(165);
    env.svm
        .set_account(
            address,
            Account {
                lamports,
                data,
                owner: TOKEN,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    address
}
