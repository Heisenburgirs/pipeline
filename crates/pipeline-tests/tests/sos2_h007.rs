//! SOS audit #2, H007: signers of the outer transaction other than the transfer authority
//! (the fee payer, a `sweep_fees` caller, an integrator or multisig co-signer) must never
//! reach a Token-2022 transfer hook as signers through any of the router's four
//! `transfer_checked` calls: the swap input leg, the Pipeline fee leg, the integrator fee
//! leg and `sweep_fees`.
//!
//! The hostile hook is `test-hook-refill`: on every transfer of its mint it moves the whole
//! balance of extra account 0 ("stash") to extra account 2 ("sink") with an SPL Token
//! `Transfer` whose authority is extra account 1. Its `ExtraAccountMetaList` is written with
//! `set_account` so that
//! - the authority entry can be a co-signer's wallet with the signer flag set, and
//! - the stash is a PDA seeded by the transfer's destination (account index 2), so a
//!   different stash is used at each router leg and only the leg under test holds tokens.
//!   The other legs' hooks see an empty stash and do nothing.
//!
//! Asserted here:
//! - control: with the hook's own PDA as the authority, the hook moves the stash at each
//!   of the four legs, so a refusal below is about the signature and not about the hook
//!   not running at that leg;
//! - the fee payer's and a separate co-signer's signatures, both passed as signers in the
//!   hook tail and named as signers by the meta list, never reach the hook at any leg
//!   (duplicates included). The co-signer's own legacy-SPL account is untouched;
//! - writability in the tail is not escalated beyond the caller's flags;
//! - the router itself drops the tail's signer flags: a multisig co-signer passed as a
//!   signer in the tail arrives at the token program unsigned (legacy SPL Token, no
//!   Token-2022 de-escalation involved);
//! - characterisation: the bundled Token-2022 (11.0.0) also clears every extra's signer
//!   flag, even for a key that signed in its own frame.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const REFILL_HOOK_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ4");
const EXECUTE: [u8; 8] = [105, 37, 101, 197, 75, 251, 102, 26];
const TRANSFER_CHECKED_TAG: u8 = 12;

const RESERVE: u64 = 1_000_000_000;
const TRADER_A: u64 = 10_000_000;
const AMOUNT_IN: u64 = 1_000_000;
const INTEGRATOR_BPS: u16 = 100;
/// What the leg under test's stash holds (in legacy-SPL mint B).
const STASH: u64 = 4_242;
const FEES_A: u64 = 3_000;

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

/// The stash the hook uses for a transfer into `destination`.
fn stash_for(destination: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[destination.as_ref()], &REFILL_HOOK_ID).0
}

fn stash_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"stash"], &REFILL_HOOK_ID).0
}

/// Writes the refill hook's `ExtraAccountMetaList` for `mint`:
/// 0. stash = hook PDA `[destination]` (seed `AccountKey { index: 2 }`), writable as given;
/// 1. `authority`, with `authority_signs` as its signer flag;
/// 2. `sink`, writable;
/// 3. SPL Token.
fn set_validation(
    env: &mut Env,
    mint: &Pubkey,
    stash_writable: bool,
    authority: &Pubkey,
    authority_signs: bool,
    sink: &Pubkey,
) {
    let mut stash_seeds = [0u8; 32];
    stash_seeds[0] = 3;
    stash_seeds[1] = 2;
    let entries: [(u8, [u8; 32], bool, bool); 4] = [
        (1, stash_seeds, false, stash_writable),
        (0, authority.to_bytes(), authority_signs, false),
        (0, sink.to_bytes(), false, true),
        (0, TOKEN.to_bytes(), false, false),
    ];
    write_meta_list(env, mint, &entries);
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

/// Writes an initialized legacy SPL Token account at `address`.
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

/// Owner, delegate option tag and amount of a legacy SPL account.
fn spl_owner_delegate_amount(env: &Env, account: &Pubkey) -> (Pubkey, u32, u64) {
    let data = env.account_data(account);
    let owner = Pubkey::try_from(&data[32..64]).unwrap();
    let delegate_tag = u32::from_le_bytes(data[72..76].try_into().unwrap());
    let amount = u64::from_le_bytes(data[64..72].try_into().unwrap());
    (owner, delegate_tag, amount)
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
    /// How many times the hook runs up to and including this leg.
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
    /// Token-2022 mint whose transfer hook is the hostile refill hook (the swap's input).
    mint_a: Pubkey,
    /// Legacy SPL Token mint (the swap's output and the hook's loot).
    mint_b: Pubkey,
    trader: Keypair,
    trader_a: Pubkey,
    trader_b: Pubkey,
    integrator_a: Pubkey,
    fee_a: Pubkey,
    treasury_a: Pubkey,
    /// The hook issuer's account in mint B.
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

    /// Writes empty stashes owned by `owner` for every swap leg, then funds the stash of
    /// `funded` (if any) with `STASH`.
    fn arm_swap_stashes(&mut self, owner: &Pubkey, funded: Option<Leg>) {
        for leg in LEGS {
            let amount = if Some(leg) == funded { STASH } else { 0 };
            let stash = stash_for(&self.leg_destination(leg));
            let mint_b = self.mint_b;
            put_spl_account(&mut self.env, &stash, &mint_b, owner, amount);
        }
    }
}

/// A CP market between A (Token-2022, hostile hook, empty meta list during setup) and B
/// (legacy SPL), a trader holding A, an integrator account in A, the treasury's account in
/// A and the hook issuer's sink in B.
fn world() -> World {
    let mut env = Env::new();
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

/// The hook tail for a transfer of A: hook program, validation account, the given stashes
/// (writable unless `stash_readonly`), the authority (as a signer if `authority_signs`),
/// any `extra` metas, the sink and SPL Token.
fn hook_tail(
    w: &World,
    stashes: &[Pubkey],
    stash_readonly: bool,
    authority: &Pubkey,
    authority_signs: bool,
    extra: &[AccountMeta],
) -> Vec<AccountMeta> {
    let mut tail = vec![
        AccountMeta::new_readonly(REFILL_HOOK_ID, false),
        AccountMeta::new_readonly(validation_address(&w.mint_a), false),
    ];
    for stash in stashes {
        tail.push(if stash_readonly {
            AccountMeta::new_readonly(*stash, false)
        } else {
            AccountMeta::new(*stash, false)
        });
    }
    tail.push(AccountMeta::new_readonly(*authority, authority_signs));
    tail.extend_from_slice(extra);
    tail.push(AccountMeta::new(w.sink, false));
    tail.push(AccountMeta::new_readonly(TOKEN, false));
    tail
}

/// Swaps `AMOUNT_IN` of A for B through the router with an integrator fee, so all three
/// input-mint legs run, with `tail` as the input hook accounts.
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

fn swap_stashes(w: &World) -> Vec<Pubkey> {
    LEGS.iter()
        .map(|leg| stash_for(&w.leg_destination(*leg)))
        .collect()
}

fn print_result(label: &str, result: &TxResult) {
    match result {
        Ok(meta) => println!("[{label}] SUCCEEDED\n{}", meta.logs.join("\n")),
        Err(e) => println!("[{label}] failed: {:?}\n{}", e.err, e.meta.logs.join("\n")),
    }
}

/// Control: with the hook's own PDA as the stash owner and authority, the hook moves the
/// targeted leg's stash at each swap leg and in `sweep_fees`, and the call completes. So
/// the hook runs at every router transfer, receives the caller's tail, and the per-leg
/// stash targeting works.
#[test]
fn control_hook_runs_at_every_router_transfer() {
    for leg in LEGS {
        let mut w = world();
        let authority = stash_authority();
        let (mint_a, sink) = (w.mint_a, w.sink);
        set_validation(&mut w.env, &mint_a, true, &authority, false, &sink);
        w.arm_swap_stashes(&authority, Some(leg));
        let tail = hook_tail(&w, &swap_stashes(&w), false, &authority, false, &[]);
        let result = swap_a(&mut w, tail, &[]);
        print_result(&format!("control {leg:?}"), &result);
        let meta = result.expect("control swap");
        assert_eq!(
            hook_invocations(&meta.logs),
            3,
            "the hook runs at all three legs"
        );
        assert_eq!(
            w.env.token_balance(&w.sink),
            STASH,
            "hook did not run at {leg:?}"
        );
        assert_eq!(w.env.token_balance(&stash_for(&w.leg_destination(leg))), 0);
        assert!(w.env.token_balance(&w.fee_a) > 0);
        assert!(w.env.token_balance(&w.integrator_a) > 0);
    }

    let mut w = world();
    let authority = stash_authority();
    let (mint_a, mint_b, sink, fee_a, treasury_a) =
        (w.mint_a, w.mint_b, w.sink, w.fee_a, w.treasury_a);
    set_validation(&mut w.env, &mint_a, true, &authority, false, &sink);
    w.env.mint_to(&mint_a, &TOKEN_2022, &fee_a, FEES_A);
    let stash = stash_for(&treasury_a);
    put_spl_account(&mut w.env, &stash, &mint_b, &authority, STASH);
    let tail = hook_tail(&w, &[stash], false, &authority, false, &[]);
    let result = sweep_a(&mut w, tail, &[]);
    print_result("control sweep", &result);
    result.expect("control sweep");
    assert_eq!(
        w.env.token_balance(&w.sink),
        STASH,
        "hook did not run in sweep_fees"
    );
    assert_eq!(w.env.token_balance(&w.treasury_a), FEES_A);
}

/// The probe for the three swap legs: the meta list names a co-signer as the hook's
/// transfer authority with the signer flag set, the co-signer signs the transaction and
/// is passed as a signer in the hook tail, and the targeted leg's stash is the
/// co-signer's own legacy-SPL account. The co-signer is either the transaction's fee
/// payer or a separate signer (an integrator co-signing, say). At every leg the hook gets
/// the co-signer unsigned, its CPI is refused, and the co-signer's account is untouched.
#[test]
fn cosigner_signature_never_reaches_the_hook_at_any_swap_leg() {
    for separate_cosigner in [false, true] {
        for leg in LEGS {
            let mut w = world();
            let cosigner = if separate_cosigner {
                w.env.funded_keypair()
            } else {
                w.env.admin.insecure_clone()
            };
            let s = cosigner.pubkey();
            let (mint_a, sink) = (w.mint_a, w.sink);
            set_validation(&mut w.env, &mint_a, true, &s, true, &sink);
            w.arm_swap_stashes(&s, Some(leg));
            let target = stash_for(&w.leg_destination(leg));
            let before = spl_owner_delegate_amount(&w.env, &target);
            assert_eq!(before, (s, 0, STASH));

            let tail = hook_tail(&w, &swap_stashes(&w), false, &s, true, &[]);
            let cosigners: Vec<&Keypair> = if separate_cosigner {
                vec![&cosigner]
            } else {
                vec![]
            };
            let label = format!("separate cosigner {separate_cosigner}, {leg:?}");
            let result = swap_a(&mut w, tail, &cosigners);
            print_result(&label, &result);

            assert_eq!(
                spl_owner_delegate_amount(&w.env, &target),
                before,
                "H007: the input mint's hook used a co-signer's signature on the co-signer's \
                 legacy-SPL account at the {leg:?} leg ({label})"
            );
            assert_eq!(
                w.env.token_balance(&w.sink),
                0,
                "H007: the co-signer's tokens reached the hook issuer ({label})"
            );
            let failed = result.as_ref().expect_err("the hostile swap must fail");
            assert_eq!(
                hook_invocations(&failed.meta.logs),
                leg.hook_runs(),
                "the failure must come from the hook at the {leg:?} leg ({label})"
            );
            assert_fails_with(result, &format!("{s}'s signer privilege escalated"));
            // The whole swap reverted.
            assert_eq!(w.env.token_balance(&w.trader_a), TRADER_A);
            assert_eq!(w.env.token_balance(&w.fee_a), 0);
        }
    }
}

/// The same probe with the co-signer repeated in the tail (writable and as a signer, three
/// times in all), so it appears several times in Token-2022's instruction. The runtime's
/// OR-merge of duplicate metas can't restore a signer flag the router never set.
#[test]
fn duplicated_cosigner_in_the_tail_gains_no_signature() {
    for leg in LEGS {
        let mut w = world();
        let cosigner = w.env.funded_keypair();
        let s = cosigner.pubkey();
        let (mint_a, sink) = (w.mint_a, w.sink);
        set_validation(&mut w.env, &mint_a, true, &s, true, &sink);
        w.arm_swap_stashes(&s, Some(leg));
        let target = stash_for(&w.leg_destination(leg));
        let extra = [AccountMeta::new(s, true), AccountMeta::new(s, false)];
        let tail = hook_tail(&w, &swap_stashes(&w), false, &s, true, &extra);
        let result = swap_a(&mut w, tail, &[&cosigner]);
        print_result(&format!("duplicate {leg:?}"), &result);
        assert_eq!(
            w.env.token_balance(&target),
            STASH,
            "H007: a duplicated co-signer let the hook move its tokens at {leg:?}"
        );
        assert_eq!(w.env.token_balance(&w.sink), 0);
        assert_fails_with(result, &format!("{s}'s signer privilege escalated"));
    }
}

/// `sweep_fees`: the sweeper (the fee payer, or a separate co-signer) passed as a signer
/// in the tail and named as a signer by the meta list. The hook gets it unsigned, and
/// neither the sweeper's account nor the fees move.
#[test]
fn sweeper_signature_never_reaches_the_hook() {
    for separate_cosigner in [false, true] {
        let mut w = world();
        let cosigner = if separate_cosigner {
            w.env.funded_keypair()
        } else {
            w.env.admin.insecure_clone()
        };
        let s = cosigner.pubkey();
        let (mint_a, mint_b, sink, fee_a, treasury_a) =
            (w.mint_a, w.mint_b, w.sink, w.fee_a, w.treasury_a);
        set_validation(&mut w.env, &mint_a, true, &s, true, &sink);
        w.env.mint_to(&mint_a, &TOKEN_2022, &fee_a, FEES_A);
        let stash = stash_for(&treasury_a);
        put_spl_account(&mut w.env, &stash, &mint_b, &s, STASH);
        let before = spl_owner_delegate_amount(&w.env, &stash);

        let tail = hook_tail(&w, &[stash], false, &s, true, &[]);
        let cosigners: Vec<&Keypair> = if separate_cosigner {
            vec![&cosigner]
        } else {
            vec![]
        };
        let result = sweep_a(&mut w, tail, &cosigners);
        print_result(
            &format!("sweep, separate cosigner {separate_cosigner}"),
            &result,
        );
        assert_eq!(
            spl_owner_delegate_amount(&w.env, &stash),
            before,
            "H007: the swept mint's hook used the sweeper's signature \
             (separate cosigner {separate_cosigner})"
        );
        assert_eq!(w.env.token_balance(&w.sink), 0);
        assert_fails_with(result, &format!("{s}'s signer privilege escalated"));
        assert_eq!(w.env.token_balance(&w.fee_a), FEES_A);
        assert_eq!(w.env.token_balance(&w.treasury_a), 0);
    }
}

/// Writability: the meta list asks for the stash writable but the caller passes it
/// read-only. The router copies the caller's flag, so Token-2022's CPI to the hook is
/// refused and the stash keeps its balance (control-style stash owned by the hook's PDA,
/// which would otherwise be emptied).
#[test]
fn tail_writability_is_not_escalated_beyond_the_callers_flags() {
    let mut w = world();
    let authority = stash_authority();
    let (mint_a, sink) = (w.mint_a, w.sink);
    set_validation(&mut w.env, &mint_a, true, &authority, false, &sink);
    w.arm_swap_stashes(&authority, Some(Leg::Input));
    let target = stash_for(&w.cp.vault_a);
    let tail = hook_tail(&w, &swap_stashes(&w), true, &authority, false, &[]);
    let result = swap_a(&mut w, tail, &[]);
    print_result("read-only stash", &result);
    assert_eq!(
        w.env.token_balance(&target),
        STASH,
        "H007: the hook wrote an account the caller passed read-only"
    );
    assert_eq!(w.env.token_balance(&w.sink), 0);
    assert_fails_with(result, &format!("{target}'s writable privilege escalated"));
}

/// The router's own layer, observed without Token-2022's de-escalation: legacy SPL Token
/// reads a multisig owner's co-signers from the accounts after the authority, which is
/// exactly where the router puts the hook tail. A multisig co-signer that signed the
/// transaction and is passed as a signer in the tail arrives at the token program
/// unsigned, so the router's input transfer is refused. Control: the same co-signer
/// authorises a direct `transfer_checked` from the multisig-owned account.
#[test]
fn router_drops_a_multisig_cosigner_signature_in_the_tail() {
    let mut env = Env::new();
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

    // Control: the multisig's co-signer can move the tokens directly.
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
        .expect("control: the co-signer authorises a direct multisig transfer");
    assert_eq!(env.token_balance(&sink), 1);
    assert_eq!(env.token_balance(&input), TRADER_A - 1);

    // The router: the multisig signs as the trader and the co-signer signs and is in the
    // hook tail as a signer.
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
    print_result("multisig swap", &result);
    assert_eq!(
        env.token_balance(&input),
        TRADER_A - 1,
        "H007: the router forwarded a tail co-signer's signature to the token program"
    );
    assert_eq!(env.token_balance(&output), 0);
    assert_fails_with(result, "MissingRequiredSignature");
}

/// Characterisation of the bundled Token-2022 (11.0.0, tlv-account-resolution 0.11.4):
/// called directly, with a co-signer that really signed in Token-2022's frame and that the
/// meta list names as a signer, Token-2022 still passes it to the hook unsigned. So on this
/// version the router's stripping is a second, independent layer; on a version with the
/// older de-escalation (tlv 0.10.0) the router's stripping is what keeps co-signers'
/// signatures away from the hook (see the multisig test for that layer).
#[test]
fn bundled_token_2022_clears_extra_signer_flags_itself() {
    let mut w = world();
    let cosigner = w.env.funded_keypair();
    let s = cosigner.pubkey();
    let (mint_a, mint_b, sink, trader_a, integrator_a) =
        (w.mint_a, w.mint_b, w.sink, w.trader_a, w.integrator_a);
    set_validation(&mut w.env, &mint_a, true, &s, true, &sink);
    let stash = stash_for(&integrator_a);
    put_spl_account(&mut w.env, &stash, &mint_b, &s, STASH);

    let mut data = vec![TRANSFER_CHECKED_TAG];
    data.extend_from_slice(&1u64.to_le_bytes());
    data.push(6);
    let mut metas = vec![
        AccountMeta::new(trader_a, false),
        AccountMeta::new_readonly(mint_a, false),
        AccountMeta::new(integrator_a, false),
        AccountMeta::new_readonly(w.trader.pubkey(), true),
    ];
    metas.extend(hook_tail(&w, &[stash], false, &s, true, &[]));
    let direct = Instruction::new_with_bytes(TOKEN_2022, &data, metas);
    let trader = w.trader.insecure_clone();
    let result = w
        .env
        .send(&[compute(400_000), direct], &[&trader, &cosigner]);
    print_result("direct Token-2022", &result);
    assert_eq!(w.env.token_balance(&stash), STASH);
    assert_eq!(w.env.token_balance(&w.sink), 0);
    assert_fails_with(result, &format!("{s}'s signer privilege escalated"));
}
