//! SOS audit #2, H033: the token-program binaries the tests run versus the ones mainnet
//! runs.
//!
//! LiteSVM 0.18.2 activates `replace_spl_token_with_p_token` in its mainnet feature set, so
//! every other test in this crate runs legacy SPL Token as p-token (`pinocchio_token_program`)
//! and Token-2022 as `spl_token_2022-11.0.0`. The pre-p-token SPL Token (`spl_token-3.5.0`)
//! ships inside LiteSVM too but is never exercised. This file runs the router properties that
//! depend on the legacy token program's behaviour under both binaries, so the result does not
//! hinge on which one a given cluster (or a rollback of SIMD-0266) has:
//!
//! - the router drops tail signer flags itself (a multisig co-signer in the tail reaches the
//!   token program unsigned), and the hook never receives a co-signer, the trader or the
//!   sweeper as a signer at any router transfer, with the hostile `test-hook-refill` hook;
//! - a market whose "vault" is the trader's own input account (a token self-transfer) cannot
//!   sell nothing;
//! - a wrapped-SOL input leg measures and charges the platform fee the same way.
//!
//! The Token-2022 binary can't be varied: no other Token-2022 `.so` is available offline.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const BPF_LOADER_2: Pubkey = client::pubkey!("BPFLoader2111111111111111111111111111111111");
const BPF_LOADER_UPGRADEABLE: Pubkey =
    client::pubkey!("BPFLoaderUpgradeab1e11111111111111111111111");
const REFILL_HOOK_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ4");
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
const EXECUTE: [u8; 8] = [105, 37, 101, 197, 75, 251, 102, 26];
const TRANSFER_CHECKED_TAG: u8 = 12;

const RESERVE: u64 = 1_000_000_000;
const TRADER_A: u64 = 10_000_000;
const AMOUNT_IN: u64 = 1_000_000;
const INTEGRATOR_BPS: u16 = 100;
const STASH: u64 = 4_242;
const FEES_A: u64 = 3_000;

/// Which legacy SPL Token binary is installed at `Tokenkeg...`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Spl {
    /// What LiteSVM 0.18.2 installs by default (p-token, upgradeable loader).
    PToken,
    /// The pre-SIMD-0266 SPL Token 3.5.0 (BPF loader v2).
    Spl35,
}

const BOTH: [Spl; 2] = [Spl::PToken, Spl::Spl35];

fn litesvm_elf(name: &str) -> Vec<u8> {
    let home = std::env::var("CARGO_HOME")
        .unwrap_or_else(|_| format!("{}/.cargo", std::env::var("HOME").unwrap()));
    let src = std::path::Path::new(&home).join("registry/src");
    for index in std::fs::read_dir(&src).unwrap() {
        let path = index
            .unwrap()
            .path()
            .join("litesvm-0.18.2/src/programs/elf")
            .join(name);
        if path.exists() {
            return std::fs::read(path).unwrap();
        }
    }
    panic!("litesvm-0.18.2 source with {name} not found under {src:?}");
}

/// Installs the chosen legacy SPL Token binary into `env`. Call before any token use.
fn install(env: &mut Env, spl: Spl) {
    if spl == Spl::Spl35 {
        let bytes = litesvm_elf("spl_token-3.5.0.so");
        env.svm
            .add_program_with_loader(TOKEN, &bytes, BPF_LOADER_2)
            .expect("install spl-token 3.5.0");
    }
    let owner = env.svm.get_account(&TOKEN).expect("token program").owner;
    match spl {
        Spl::PToken => assert_eq!(owner, BPF_LOADER_UPGRADEABLE),
        Spl::Spl35 => assert_eq!(owner, BPF_LOADER_2),
    }
}

fn compute(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction::new_with_bytes(
        client::pubkey!("ComputeBudget111111111111111111111111111111"),
        &data,
        vec![],
    )
}

fn validation_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], &REFILL_HOOK_ID).0
}

fn stash_for(destination: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[destination.as_ref()], &REFILL_HOOK_ID).0
}

fn stash_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"stash"], &REFILL_HOOK_ID).0
}

fn write_meta_list(env: &mut Env, mint: &Pubkey, entries: &[(u8, [u8; 32], bool, bool)]) {
    let mut tlv = EXECUTE.to_vec();
    tlv.extend_from_slice(&(4 + 35 * entries.len() as u32).to_le_bytes());
    tlv.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (discriminator, config, signer, writable) in entries {
        tlv.push(*discriminator);
        tlv.extend_from_slice(config);
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

/// Meta list: stash = hook PDA seeded by the transfer destination (writable), then
/// `authority` (signer flag as given), the sink, SPL Token.
fn set_validation(env: &mut Env, mint: &Pubkey, authority: &Pubkey, signs: bool, sink: &Pubkey) {
    let mut stash_seeds = [0u8; 32];
    stash_seeds[0] = 3;
    stash_seeds[1] = 2;
    let entries: [(u8, [u8; 32], bool, bool); 4] = [
        (1, stash_seeds, false, true),
        (0, authority.to_bytes(), signs, false),
        (0, sink.to_bytes(), false, true),
        (0, TOKEN.to_bytes(), false, false),
    ];
    write_meta_list(env, mint, &entries);
}

fn put_spl_account(env: &mut Env, address: &Pubkey, mint: &Pubkey, owner: &Pubkey, amount: u64) {
    let mut data = vec![0u8; 165];
    data[..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1;
    let lamports = env.svm.minimum_balance_for_rent_exemption(165);
    env.svm
        .set_account(
            *address,
            Account {
                lamports,
                data,
                owner: TOKEN,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
}

fn spl_owner_delegate_amount(env: &Env, account: &Pubkey) -> (Pubkey, u32, u64) {
    let data = env.account_data(account);
    (
        Pubkey::try_from(&data[32..64]).unwrap(),
        u32::from_le_bytes(data[72..76].try_into().unwrap()),
        u64::from_le_bytes(data[64..72].try_into().unwrap()),
    )
}

fn hook_invocations(logs: &[String]) -> usize {
    let needle = format!("Program {REFILL_HOOK_ID} invoke");
    logs.iter().filter(|line| line.contains(&needle)).count()
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Leg {
    Input,
    PipelineFee,
    IntegratorFee,
}

const LEGS: [Leg; 3] = [Leg::Input, Leg::PipelineFee, Leg::IntegratorFee];

impl Leg {
    fn hook_runs(self) -> usize {
        match self {
            Leg::Input => 1,
            Leg::PipelineFee => 2,
            Leg::IntegratorFee => 3,
        }
    }
}

struct World {
    env: Env,
    cp: CpMarket,
    mint_a: Pubkey,
    mint_b: Pubkey,
    trader: Keypair,
    trader_a: Pubkey,
    trader_b: Pubkey,
    integrator_a: Pubkey,
    fee_a: Pubkey,
    treasury_a: Pubkey,
    sink: Pubkey,
}

impl World {
    fn leg_destination(&self, leg: Leg) -> Pubkey {
        match leg {
            Leg::Input => self.cp.vault_a,
            Leg::PipelineFee => self.fee_a,
            Leg::IntegratorFee => self.integrator_a,
        }
    }

    fn arm_swap_stashes(&mut self, owner: &Pubkey, funded: Option<Leg>) {
        for leg in LEGS {
            let amount = if Some(leg) == funded { STASH } else { 0 };
            let stash = stash_for(&self.leg_destination(leg));
            let mint_b = self.mint_b;
            put_spl_account(&mut self.env, &stash, &mint_b, owner, amount);
        }
    }

    fn swap_stashes(&self) -> Vec<Pubkey> {
        LEGS.iter()
            .map(|leg| stash_for(&self.leg_destination(*leg)))
            .collect()
    }
}

/// CP market between A (Token-2022 with the hostile refill hook) and B (legacy SPL), run
/// with the chosen legacy SPL Token binary.
fn world(spl: Spl) -> World {
    let mut env = Env::new();
    install(&mut env, spl);
    env.svm
        .add_program_from_file(REFILL_HOOK_ID, sbf_dir().join("test_hook_refill.so"))
        .unwrap();
    let mint_a = env.create_mint(&TOKEN_2022, 6, None, Some(REFILL_HOOK_ID));
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    write_meta_list(&mut env, &mint_a, &[]);
    let cp = setup_cp_market_with_mints(
        &mut env,
        mint_a,
        TOKEN_2022,
        mint_b,
        TOKEN,
        RESERVE,
        RESERVE,
        30,
        vec![
            AccountMeta::new_readonly(REFILL_HOOK_ID, false),
            AccountMeta::new_readonly(validation_address(&mint_a), false),
        ],
    );
    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN_2022, TRADER_A);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let integrator_a = env.create_ata(&Pubkey::new_unique(), &mint_a, &TOKEN_2022);
    let fee_a = pda::fee_account(&cp.market, &mint_a, &TOKEN_2022);
    let treasury_owner = env.treasury_owner;
    let treasury_a = env.create_ata(&treasury_owner, &mint_a, &TOKEN_2022);
    let sink = env.create_ata(&Pubkey::new_unique(), &mint_b, &TOKEN);
    World {
        env,
        cp,
        mint_a,
        mint_b,
        trader,
        trader_a,
        trader_b,
        integrator_a,
        fee_a,
        treasury_a,
        sink,
    }
}

fn hook_tail(
    w: &World,
    stashes: &[Pubkey],
    authority: &Pubkey,
    authority_signs: bool,
) -> Vec<AccountMeta> {
    let mut tail = vec![
        AccountMeta::new_readonly(REFILL_HOOK_ID, false),
        AccountMeta::new_readonly(validation_address(&w.mint_a), false),
    ];
    for stash in stashes {
        tail.push(AccountMeta::new(*stash, false));
    }
    tail.push(AccountMeta::new_readonly(*authority, authority_signs));
    tail.push(AccountMeta::new(w.sink, false));
    tail.push(AccountMeta::new_readonly(TOKEN, false));
    tail
}

fn swap_a(w: &mut World, tail: Vec<AccountMeta>, cosigners: &[&Keypair]) -> TxResult {
    let mut accounts = cp_swap_accounts(
        &w.cp,
        &w.trader.pubkey(),
        &w.mint_a,
        &w.trader_a,
        &w.mint_b,
        &w.trader_b,
    );
    accounts.integrator_fee_account = Some(w.integrator_a);
    accounts.input_hook_accounts = tail;
    let mut args = exact_in(AMOUNT_IN, 0);
    args.integrator_fee_bps = INTEGRATOR_BPS;
    let ix = client::swap(&accounts, &args);
    let trader = w.trader.insecure_clone();
    let mut all = vec![&trader];
    all.extend_from_slice(cosigners);
    w.env.send(&[compute(1_400_000), ix], &all)
}

fn sweep_a(w: &mut World, tail: Vec<AccountMeta>, cosigners: &[&Keypair]) -> TxResult {
    let treasury_owner = w.env.treasury_owner;
    let ix = client::sweep_fees(&w.cp.market, &w.mint_a, &TOKEN_2022, &treasury_owner, tail);
    w.env.send(&[compute(400_000), ix], cosigners)
}

/// CU a program reports for its first top-level-or-nested invocation in `logs`.
fn first_consumed(logs: &[String], program: &Pubkey) -> u64 {
    let prefix = format!("Program {program} consumed ");
    logs.iter()
        .find_map(|line| {
            line.strip_prefix(&prefix)
                .and_then(|rest| rest.split(' ').next())
                .and_then(|n| n.parse().ok())
        })
        .expect("token program consumed-units log line")
}

// ------------------------------------------------------------------------------------
// 0. what the default tests run
// ------------------------------------------------------------------------------------

/// Characterisation. A default `Env` runs p-token as legacy SPL Token (upgradeable loader;
/// ~100 CU for a transfer) and Token-2022 under the upgradeable loader too. The 3.5.0
/// binary installed by `install` is the old program (BPF loader v2, thousands of CU). The
/// bundled ELFs are what the program-data accounts contain.
#[test]
fn default_tests_run_p_token_and_the_install_helper_swaps_in_spl_token_3_5() {
    let mut cu = vec![];
    for spl in BOTH {
        let mut env = Env::new();
        install(&mut env, spl);
        let mint = env.create_mint(&TOKEN, 6, None, None);
        let (trader, from) = env.trader_with(&mint, &TOKEN, 1_000);
        let to = env.create_ata(&Pubkey::new_unique(), &mint, &TOKEN);
        let mut data = vec![TRANSFER_CHECKED_TAG];
        data.extend_from_slice(&10u64.to_le_bytes());
        data.push(6);
        let ix = Instruction::new_with_bytes(
            TOKEN,
            &data,
            vec![
                AccountMeta::new(from, false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new(to, false),
                AccountMeta::new_readonly(trader.pubkey(), true),
            ],
        );
        let meta = env.send(&[ix], &[&trader]).expect("direct transfer");
        let used = first_consumed(&meta.logs, &TOKEN);
        println!("{spl:?}: transfer_checked consumed {used} CU");
        cu.push(used);
        assert_eq!(env.token_balance(&to), 10);
        if spl == Spl::PToken {
            // The Token-2022 and p-token program-data accounts hold the bundled ELFs.
            for (program, file) in [
                (TOKEN, "pinocchio_token_program.so"),
                (TOKEN_2022, "spl_token_2022-11.0.0.so"),
            ] {
                let program_account = env.svm.get_account(&program).unwrap();
                let programdata = Pubkey::try_from(&program_account.data[4..36]).unwrap();
                let data = env.account_data(&programdata);
                let elf = litesvm_elf(file);
                assert_eq!(
                    &data[45..45 + elf.len()],
                    &elf[..],
                    "{program} is not {file}"
                );
            }
        }
    }
    assert!(
        cu[0] * 10 < cu[1],
        "p-token ({}) should be far cheaper than SPL Token 3.5.0 ({})",
        cu[0],
        cu[1]
    );
}

// ------------------------------------------------------------------------------------
// 1. hook-signer properties under both legacy SPL binaries
// ------------------------------------------------------------------------------------

/// Control: the hostile hook runs at every router transfer under both binaries and its
/// stash move succeeds when the hook's own PDA is the owner (so the refusals below are
/// about signatures, not about the hook failing to run).
#[test]
fn control_hook_runs_under_both_legacy_binaries() {
    for spl in BOTH {
        let mut w = world(spl);
        let authority = stash_authority();
        let (mint_a, sink) = (w.mint_a, w.sink);
        set_validation(&mut w.env, &mint_a, &authority, false, &sink);
        w.arm_swap_stashes(&authority, Some(Leg::PipelineFee));
        let tail = hook_tail(&w, &w.swap_stashes(), &authority, false);
        let meta = swap_a(&mut w, tail, &[]).unwrap_or_else(|e| {
            panic!(
                "control swap under {spl:?}: {:?}\n{}",
                e.err,
                e.meta.logs.join("\n")
            )
        });
        assert_eq!(hook_invocations(&meta.logs), 3, "{spl:?}");
        assert_eq!(
            w.env.token_balance(&w.sink),
            STASH,
            "{spl:?}: hook did not move stash"
        );

        let mut w = world(spl);
        let (mint_a, mint_b, sink, fee_a, treasury_a) =
            (w.mint_a, w.mint_b, w.sink, w.fee_a, w.treasury_a);
        set_validation(&mut w.env, &mint_a, &authority, false, &sink);
        w.env.mint_to(&mint_a, &TOKEN_2022, &fee_a, FEES_A);
        let stash = stash_for(&treasury_a);
        put_spl_account(&mut w.env, &stash, &mint_b, &authority, STASH);
        let tail = hook_tail(&w, &[stash], &authority, false);
        sweep_a(&mut w, tail, &[]).expect("control sweep");
        assert_eq!(w.env.token_balance(&w.sink), STASH, "{spl:?}: sweep hook");
        assert_eq!(w.env.token_balance(&w.treasury_a), FEES_A);
    }
}

/// A co-signer (the fee payer, or a separate signer) or the trader itself, named as a
/// signer in the hook's meta list and passed as a signer in the tail, must reach the hook
/// unsigned at every swap leg whichever legacy SPL binary is installed. The co-signer's own
/// legacy-SPL stash must not move.
#[test]
fn hook_never_gets_a_signature_under_either_legacy_binary() {
    for spl in BOTH {
        for who in ["fee payer", "separate", "trader"] {
            for leg in LEGS {
                let mut w = world(spl);
                let cosigner = match who {
                    "separate" => w.env.funded_keypair(),
                    "trader" => w.trader.insecure_clone(),
                    _ => w.env.admin.insecure_clone(),
                };
                let s = cosigner.pubkey();
                let (mint_a, sink) = (w.mint_a, w.sink);
                set_validation(&mut w.env, &mint_a, &s, true, &sink);
                w.arm_swap_stashes(&s, Some(leg));
                let target = stash_for(&w.leg_destination(leg));
                let before = spl_owner_delegate_amount(&w.env, &target);
                assert_eq!(before, (s, 0, STASH));
                let tail = hook_tail(&w, &w.swap_stashes(), &s, true);
                let cosigners: Vec<&Keypair> = if who == "separate" {
                    vec![&cosigner]
                } else {
                    vec![]
                };
                let label = format!("{spl:?}/{who}/{leg:?}");
                let result = swap_a(&mut w, tail, &cosigners);
                assert_eq!(
                    spl_owner_delegate_amount(&w.env, &target),
                    before,
                    "H033: stash moved with a signature the hook must not get ({label})"
                );
                assert_eq!(
                    w.env.token_balance(&w.sink),
                    0,
                    "H033: loot reached the sink ({label})"
                );
                let failed = result.as_ref().expect_err("hostile swap must fail");
                assert_eq!(
                    hook_invocations(&failed.meta.logs),
                    leg.hook_runs(),
                    "failure must come from the hook at the leg ({label})"
                );
                assert_fails_with(result, &format!("{s}'s signer privilege escalated"));
                assert_eq!(
                    w.env.token_balance(&w.trader_a),
                    TRADER_A,
                    "{label}: reverted"
                );
            }
        }
    }
}

/// `sweep_fees` under both binaries: the fee authority PDA's own legacy-SPL fee account in
/// the other mint is the prize; the hook is told to use the sweeper's signature.
#[test]
fn sweeper_signature_never_reaches_the_hook_under_either_binary() {
    for spl in BOTH {
        let mut w = world(spl);
        let sweeper = w.env.funded_keypair();
        let s = sweeper.pubkey();
        let (mint_a, mint_b, sink, fee_a, treasury_a) =
            (w.mint_a, w.mint_b, w.sink, w.fee_a, w.treasury_a);
        set_validation(&mut w.env, &mint_a, &s, true, &sink);
        w.env.mint_to(&mint_a, &TOKEN_2022, &fee_a, FEES_A);
        let stash = stash_for(&treasury_a);
        put_spl_account(&mut w.env, &stash, &mint_b, &s, STASH);
        let before = spl_owner_delegate_amount(&w.env, &stash);
        let tail = hook_tail(&w, &[stash], &s, true);
        let result = sweep_a(&mut w, tail, &[&sweeper]);
        assert_eq!(
            spl_owner_delegate_amount(&w.env, &stash),
            before,
            "H033 {spl:?}"
        );
        assert_eq!(w.env.token_balance(&w.sink), 0);
        assert_fails_with(result, &format!("{s}'s signer privilege escalated"));
        assert_eq!(w.env.token_balance(&w.fee_a), FEES_A);
    }
}

/// The router's own stripping, observable with both SPL Token binaries: a multisig owner's
/// co-signer is read from the accounts after the authority (where the router appends the
/// tail). Passed as a signer in the tail, the co-signer must reach the token program
/// unsigned. Control: the same co-signer authorises a direct transfer.
#[test]
fn router_drops_a_multisig_cosigner_under_either_legacy_binary() {
    for spl in BOTH {
        let mut env = Env::new();
        install(&mut env, spl);
        let cp = setup_cp_market(&mut env, TOKEN, TOKEN, RESERVE, RESERVE, 30);
        let multisig = env.funded_keypair();
        let cosigner = env.funded_keypair();
        let mut data = vec![0u8; 355];
        data[0] = 1;
        data[1] = 1;
        data[2] = 1;
        data[3..35].copy_from_slice(cosigner.pubkey().as_ref());
        let lamports = env.svm.minimum_balance_for_rent_exemption(355);
        env.svm
            .set_account(
                multisig.pubkey(),
                Account {
                    lamports,
                    data,
                    owner: TOKEN,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        let input = env.create_ata(&multisig.pubkey(), &cp.mint_a, &TOKEN);
        let output = env.create_ata(&multisig.pubkey(), &cp.mint_b, &TOKEN);
        env.mint_to(&cp.mint_a, &TOKEN, &input, TRADER_A);
        let decimals = env.account_data(&cp.mint_a)[44];

        let sink = env.create_ata(&Pubkey::new_unique(), &cp.mint_a, &TOKEN);
        let mut data = vec![TRANSFER_CHECKED_TAG];
        data.extend_from_slice(&1u64.to_le_bytes());
        data.push(decimals);
        let direct = Instruction::new_with_bytes(
            TOKEN,
            &data,
            vec![
                AccountMeta::new(input, false),
                AccountMeta::new_readonly(cp.mint_a, false),
                AccountMeta::new(sink, false),
                AccountMeta::new_readonly(multisig.pubkey(), false),
                AccountMeta::new_readonly(cosigner.pubkey(), true),
            ],
        );
        env.send(&[direct], &[&cosigner])
            .unwrap_or_else(|e| panic!("control under {spl:?}: {:?}", e.err));
        assert_eq!(env.token_balance(&sink), 1);

        let mut accounts = cp_swap_accounts(
            &cp,
            &multisig.pubkey(),
            &cp.mint_a,
            &input,
            &cp.mint_b,
            &output,
        );
        accounts.input_hook_accounts = vec![AccountMeta::new_readonly(cosigner.pubkey(), true)];
        let ix = client::swap(&accounts, &exact_in(AMOUNT_IN, 0));
        let result = env.send(&[ix], &[&multisig, &cosigner]);
        assert_eq!(
            env.token_balance(&input),
            TRADER_A - 1,
            "H033 {spl:?}: the router forwarded a tail co-signer's signature to SPL Token"
        );
        assert_eq!(env.token_balance(&output), 0);
        assert_fails_with(result, "MissingRequiredSignature");
    }
}

// ------------------------------------------------------------------------------------
// 2. self-transfer measurement under both binaries
// ------------------------------------------------------------------------------------

struct Evil {
    env: Env,
    state: Pubkey,
    market: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    vault_b: Pubkey,
}

/// A market whose declared `vault[A]` is `victim_a`, the victim's own account.
fn aliased_vault_market(spl: Spl) -> (Evil, Keypair, Pubkey, Pubkey) {
    let mut env = Env::new();
    install(&mut env, spl);
    env.register(&EVIL_ID, "test_strategy_evil");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let victim = env.funded_keypair();
    let victim_a = env.create_ata(&victim.pubkey(), &mint_a, &TOKEN);
    let victim_b = env.create_ata(&victim.pubkey(), &mint_b, &TOKEN);
    env.mint_to(&mint_a, &TOKEN, &victim_a, 10_000_000);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
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
            AccountMeta::new(victim_a, false),
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
    (
        Evil {
            env,
            state,
            market,
            mint_a,
            mint_b,
            vault_b,
        },
        victim,
        victim_a,
        victim_b,
    )
}

/// `trader_input == input_vault`: the router's input leg is a token self-transfer. Under
/// both SPL Token binaries it must be a measured no-op that the settlement refuses
/// (`OutputWithoutInput`), with no tokens moved and no fee taken.
#[test]
fn self_transfer_vault_alias_is_refused_under_either_legacy_binary() {
    for spl in BOTH {
        let (mut e, victim, victim_a, victim_b) = aliased_vault_market(spl);
        let mut strategy_accounts = vec![AccountMeta::new(e.vault_b, false)];
        strategy_accounts.truncate(1);
        let accounts = client::SwapAccounts {
            trader: victim.pubkey(),
            strategy_program: EVIL_ID,
            market_state: e.state,
            trader_input: victim_a,
            trader_output: victim_b,
            input_vault: victim_a,
            input_mint: e.mint_a,
            output_mint: e.mint_b,
            input_token_program: TOKEN,
            output_token_program: TOKEN,
            integrator_fee_account: None,
            strategy_accounts,
            input_hook_accounts: vec![],
        };
        let mut args = exact_in(1_000, 0);
        args.extra = vec![0]; // honest behaviour
        let _ = e.market;
        let result = e.env.send(&[client::swap(&accounts, &args)], &[&victim]);
        assert!(result.is_err(), "H033 {spl:?}: aliased swap succeeded");
        assert_fails_with(result, "OutputWithoutInput");
        assert_eq!(e.env.token_balance(&victim_a), 10_000_000, "{spl:?}");
        assert_eq!(e.env.token_balance(&victim_b), 0, "{spl:?}");
    }
}

// ------------------------------------------------------------------------------------
// 3. native (wrapped SOL) input measures and charges identically
// ------------------------------------------------------------------------------------

/// wSOL as the input mint under both SPL Token binaries: the trader's account falls by
/// `amount_in` plus the 25 bps fee (rounded up), the fee account holds exactly the fee,
/// and the native account's lamports back its token balance.
#[test]
fn wsol_input_leg_charges_the_same_fee_under_either_legacy_binary() {
    use client::wallet::NATIVE_MINT;
    let amount: u64 = 123_456_789;
    let fee = (amount * 25).div_ceil(10_000);
    for spl in BOTH {
        let mut env = Env::new();
        install(&mut env, spl);
        let mint_b = env.create_mint(&TOKEN, 9, None, None);
        let cp = setup_cp_market_with_mints(
            &mut env,
            NATIVE_MINT,
            TOKEN,
            mint_b,
            TOKEN,
            RESERVE,
            RESERVE,
            30,
            vec![],
        );
        let trader = env.funded_keypair();
        let trader_in = env.create_ata(&trader.pubkey(), &NATIVE_MINT, &TOKEN);
        let trader_out = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
        env.mint_to(&NATIVE_MINT, &TOKEN, &trader_in, amount * 2);
        let accounts = cp_swap_accounts(
            &cp,
            &trader.pubkey(),
            &NATIVE_MINT,
            &trader_in,
            &mint_b,
            &trader_out,
        );
        let fee_account = pda::fee_account(&cp.market, &NATIVE_MINT, &TOKEN);
        env.send(&[client::swap(&accounts, &exact_in(amount, 0))], &[&trader])
            .unwrap_or_else(|e| {
                panic!(
                    "wsol swap under {spl:?}: {:?}\n{}",
                    e.err,
                    e.meta.logs.join("\n")
                )
            });
        assert_eq!(
            env.token_balance(&trader_in),
            amount * 2 - amount - fee,
            "{spl:?}"
        );
        assert_eq!(env.token_balance(&fee_account), fee, "{spl:?}");
        let fee_lamports = env.lamports(&fee_account);
        let rent = env.svm.minimum_balance_for_rent_exemption(165);
        assert_eq!(
            fee_lamports,
            rent + fee,
            "{spl:?}: native fee account lamports"
        );
    }
}
