//! BOK LiteSVM tests — market creation, signer forwarding and the market registry
//! (INV-ADM-4, -5, -6, -7), run against the real router with the hostile test strategy.

mod common;

use common::*;
use litesvm::types::TransactionMetadata;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_events::PipelineEvent;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");

/// `8 + Market::INIT_SPACE`.
const MARKET_SPACE: usize = 8 + 32 + 32 + 1 + 4 * 32 + 4 * 32 + 1 + 32 + 8 + 1 + 1;
/// The evil strategy's market state (`STATE_LEN`), which its setup pays rent for.
const STATE_LEN: usize = 32 * 4 + 2;

/// Evil strategy `init_market` modes (programs/test-strategy-evil/src/lib.rs).
const HONEST: u8 = 0;
const INFO_DUPLICATE_MINTS: u8 = 1;
const INFO_ONE_MINT: u8 = 2;
const INFO_VAULT_MISMATCH: u8 = 3;
const INIT_DRAINS_PAYER: u8 = 4;
const INIT_ASSIGNS_PAYER: u8 = 5;
const INIT_ALLOCATES_PAYER: u8 = 6;
const INIT_APPROVES_CREATOR_TOKENS: u8 = 7;
/// Evil strategy swap behaviours.
const TAKE_MORE: u8 = 1;
const TAKE_LAMPORTS: u8 = 9;

struct Evil {
    mint_a: Pubkey,
    mint_b: Pubkey,
    state: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
    market: Pubkey,
}

impl Evil {
    fn setup_payer(&self) -> Pubkey {
        pda::setup_payer(&self.market)
    }
}

/// New mints and vaults for an evil market (not yet created).
fn evil_accounts(env: &mut Env) -> Evil {
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    Evil {
        mint_a,
        mint_b,
        state,
        vault_a,
        vault_b,
        market: pda::market(&EVIL_ID, &state),
    }
}

fn evil_create_ix(
    creator: &Pubkey,
    evil: &Evil,
    mode: u8,
    max_creator_lamports: u64,
    extra: Vec<AccountMeta>,
) -> Instruction {
    let mut strategy_accounts = vec![
        AccountMeta::new_readonly(evil.mint_a, false),
        AccountMeta::new_readonly(evil.mint_b, false),
        AccountMeta::new(evil.vault_a, false),
        AccountMeta::new(evil.vault_b, false),
    ];
    strategy_accounts.extend(extra);
    client::create_market(&client::CreateMarket {
        creator: *creator,
        strategy_program: EVIL_ID,
        market_state: evil.state,
        market_state_signs: false,
        init_data: vec![mode],
        max_creator_lamports,
        strategy_accounts,
    })
}

fn rent(env: &Env, len: usize) -> u64 {
    env.svm.minimum_balance_for_rent_exemption(len)
}

fn market_count(env: &Env, program: &Pubkey) -> u64 {
    client::decode_strategy(&env.account_data(&pda::strategy(program)))
        .unwrap()
        .market_count
}

/// The setup payer after a successful `create_market`: gone, or empty, system-owned and
/// holding nothing.
fn assert_setup_payer_empty(env: &Env, evil: &Evil) {
    if let Some(account) = env.svm.get_account(&evil.setup_payer()) {
        assert_eq!(account.lamports, 0, "setup payer keeps lamports");
        assert_eq!(
            account.owner,
            client::SYSTEM_PROGRAM_ID,
            "setup payer reassigned"
        );
        assert!(account.data.is_empty(), "setup payer allocated");
    }
}

/// Like `Env::send`, also returning the message's account keys.
fn send_with_keys(
    env: &mut Env,
    instructions: &[Instruction],
    signers: &[&Keypair],
) -> (TxResult, Vec<Pubkey>) {
    env.svm.expire_blockhash();
    let mut all: Vec<&Keypair> = vec![&env.admin];
    all.extend_from_slice(signers);
    let tx = Transaction::new_signed_with_payer(
        instructions,
        Some(&env.admin.pubkey()),
        &all,
        env.svm.latest_blockhash(),
    );
    let keys = tx.message.account_keys.clone();
    (env.svm.send_transaction(tx), keys)
}

/// System-program transfers among the inner instructions, as (from, to, lamports).
fn inner_system_transfers(
    meta: &TransactionMetadata,
    keys: &[Pubkey],
) -> Vec<(Pubkey, Pubkey, u64)> {
    meta.inner_instructions
        .iter()
        .flatten()
        .map(|inner| &inner.instruction)
        .filter(|ix| keys[ix.program_id_index as usize] == client::SYSTEM_PROGRAM_ID)
        .filter(|ix| ix.data.len() == 12 && ix.data[..4] == 2u32.to_le_bytes())
        .map(|ix| {
            (
                keys[ix.accounts[0] as usize],
                keys[ix.accounts[1] as usize],
                u64::from_le_bytes(ix.data[4..12].try_into().unwrap()),
            )
        })
        .collect()
}

fn registered_evil_env() -> Env {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    env
}

// ---------------------------------------------------------------------------------------
// INV-ADM-4 — creator loses at most the allowance plus Market rent
// ---------------------------------------------------------------------------------------

/// INV-ADM-4: an honest setup costs the creator exactly the Market rent plus what the
/// strategy spent (its state's rent), whatever the allowance; the unused allowance comes
/// back and the setup payer ends empty.
#[test]
fn bok_inv_adm_4_honest_setup_exact_creator_debit() {
    let mut env = registered_evil_env();
    let market_rent = rent(&env, MARKET_SPACE);
    let state_rent = rent(&env, STATE_LEN);
    for max in [state_rent, SOL / 100, MAX_SETUP_LAMPORTS, 50 * SOL] {
        let creator = env.funded_keypair();
        let evil = evil_accounts(&mut env);
        let c0 = env.lamports(&creator.pubkey());
        let ix = evil_create_ix(&creator.pubkey(), &evil, HONEST, max, vec![]);
        env.send(&[ix], &[&creator]).unwrap();
        let debit = c0 - env.lamports(&creator.pubkey());
        assert_eq!(debit, market_rent + state_rent, "allowance {max}");
        assert!(debit <= max + market_rent);
        let market = env.svm.get_account(&evil.market).unwrap();
        assert_eq!(market.lamports, market_rent);
        assert_eq!(market.data.len(), MARKET_SPACE);
        assert_setup_payer_empty(&env, &evil);
    }
}

/// INV-ADM-4: a setup that spends 1 SOL more than it needs is bounded by the allowance:
/// it fails (moving nothing) when the allowance is short, and otherwise costs exactly
/// what it spent, never more than allowance + Market rent.
#[test]
fn bok_inv_adm_4_draining_setup_is_bounded_by_the_allowance() {
    let mut env = registered_evil_env();
    let market_rent = rent(&env, MARKET_SPACE);
    let state_rent = rent(&env, STATE_LEN);
    let needed = state_rent + SOL;
    for max in [SOL / 100, SOL, needed - 1, needed, 2 * SOL] {
        let creator = env.funded_keypair();
        let evil = evil_accounts(&mut env);
        let c0 = env.lamports(&creator.pubkey());
        let ix = evil_create_ix(&creator.pubkey(), &evil, INIT_DRAINS_PAYER, max, vec![]);
        let result = env.send(&[ix], &[&creator]);
        if max < needed {
            assert!(result.is_err(), "allowance {max} can't cover the drain");
            assert_eq!(env.lamports(&creator.pubkey()), c0);
            assert!(env.svm.get_account(&evil.market).is_none());
        } else {
            result.unwrap();
            let debit = c0 - env.lamports(&creator.pubkey());
            assert_eq!(debit, market_rent + needed, "allowance {max}");
            assert!(debit <= max + market_rent);
            assert_setup_payer_empty(&env, &evil);
        }
    }
}

/// INV-ADM-4: with `max_creator_lamports = 0` the router makes no creator → setup payer
/// transfer. A setup that needs SOL then fails without moving anything, unless someone
/// pre-funded the setup payer, in which case what the strategy leaves goes to the
/// creator.
#[test]
fn bok_inv_adm_4_zero_allowance_moves_nothing_from_the_creator() {
    let mut env = registered_evil_env();
    let market_rent = rent(&env, MARKET_SPACE);
    let state_rent = rent(&env, STATE_LEN);

    let creator = env.funded_keypair();
    let evil = evil_accounts(&mut env);
    let c0 = env.lamports(&creator.pubkey());
    let ix = evil_create_ix(&creator.pubkey(), &evil, HONEST, 0, vec![]);
    assert!(env.send(&[ix], &[&creator]).is_err());
    assert_eq!(env.lamports(&creator.pubkey()), c0);

    let evil = evil_accounts(&mut env);
    let prefund = state_rent + rent(&env, 0) + 777;
    env.svm.airdrop(&evil.setup_payer(), prefund).unwrap();
    let c0 = env.lamports(&creator.pubkey());
    let ix = evil_create_ix(&creator.pubkey(), &evil, HONEST, 0, vec![]);
    let (result, keys) = send_with_keys(&mut env, &[ix], &[&creator]);
    let meta = result.unwrap();
    let transfers = inner_system_transfers(&meta, &keys);
    assert!(
        !transfers
            .iter()
            .any(|(from, to, _)| *from == creator.pubkey() && *to == evil.setup_payer()),
        "no creator → setup payer transfer with a zero allowance: {transfers:?}"
    );
    let leftover = prefund - state_rent;
    assert!(transfers.contains(&(evil.setup_payer(), creator.pubkey(), leftover)));
    assert_eq!(
        env.lamports(&creator.pubkey()) as i128 - c0 as i128,
        leftover as i128 - market_rent as i128
    );
    assert_setup_payer_empty(&env, &evil);
}

/// INV-ADM-4: lamports someone pre-sent to the (deterministic) setup payer or Market PDA
/// are not stranded: the setup payer's go to the creator and a pre-funded Market only
/// needs its rent topped up.
#[test]
fn bok_inv_adm_4_prefunded_pdas_reduce_the_creators_cost() {
    let mut env = registered_evil_env();
    let market_rent = rent(&env, MARKET_SPACE);
    let state_rent = rent(&env, STATE_LEN);
    let creator = env.funded_keypair();

    let evil = evil_accounts(&mut env);
    let setup_prefund = rent(&env, 0) + 4_242;
    env.svm.airdrop(&evil.setup_payer(), setup_prefund).unwrap();
    let c0 = env.lamports(&creator.pubkey());
    let ix = evil_create_ix(&creator.pubkey(), &evil, HONEST, SOL, vec![]);
    env.send(&[ix], &[&creator]).unwrap();
    assert_eq!(
        c0 - env.lamports(&creator.pubkey()),
        market_rent + state_rent - setup_prefund
    );
    assert_setup_payer_empty(&env, &evil);

    let evil = evil_accounts(&mut env);
    let market_prefund = rent(&env, 0) + 1_000;
    env.svm.airdrop(&evil.market, market_prefund).unwrap();
    let c0 = env.lamports(&creator.pubkey());
    let ix = evil_create_ix(&creator.pubkey(), &evil, HONEST, SOL, vec![]);
    env.send(&[ix], &[&creator]).unwrap();
    assert_eq!(
        c0 - env.lamports(&creator.pubkey()),
        market_rent - market_prefund + state_rent
    );
    assert_eq!(env.lamports(&evil.market), market_rent);
    assert_setup_payer_empty(&env, &evil);
}

/// INV-ADM-4: a failing `create_market` (setup payer reassigned or allocated, or bad
/// market info) changes no lamport balance: creator, pre-funded setup payer and Market
/// PDA all stay as they were.
#[test]
fn bok_inv_adm_4_failed_create_market_moves_no_lamports() {
    let mut env = registered_evil_env();
    let creator = env.funded_keypair();
    for (mode, error) in [
        (INIT_ASSIGNS_PAYER, "SetupPayerMisused"),
        (INIT_ALLOCATES_PAYER, "SetupPayerMisused"),
        (INFO_DUPLICATE_MINTS, "BadMarketInfo"),
        (INFO_ONE_MINT, "BadMarketInfo"),
        (INFO_VAULT_MISMATCH, "BadMarketInfo"),
    ] {
        let evil = evil_accounts(&mut env);
        let prefund = rent(&env, 0) + 99;
        env.svm.airdrop(&evil.setup_payer(), prefund).unwrap();
        let before = (
            env.lamports(&creator.pubkey()),
            env.lamports(&evil.setup_payer()),
            env.lamports(&evil.market),
            env.lamports(&evil.state),
        );
        let ix = evil_create_ix(&creator.pubkey(), &evil, mode, SOL, vec![]);
        assert_fails_with(env.send(&[ix], &[&creator]), error);
        let after = (
            env.lamports(&creator.pubkey()),
            env.lamports(&evil.setup_payer()),
            env.lamports(&evil.market),
            env.lamports(&evil.state),
        );
        assert_eq!(after, before, "mode {mode}");
        assert_eq!(
            env.svm.get_account(&evil.setup_payer()).unwrap().owner,
            client::SYSTEM_PROGRAM_ID
        );
    }
}

// ---------------------------------------------------------------------------------------
// INV-ADM-5 — only fresh, non-creator signers keep their signature
// ---------------------------------------------------------------------------------------

fn delegate_of(env: &Env, token_account: &Pubkey) -> Option<Pubkey> {
    let data = env.account_data(token_account);
    (data[72] == 1).then(|| Pubkey::new_from_array(data[76..108].try_into().unwrap()))
}

/// INV-ADM-5: the evil setup tries to `Approve` a token account using a wallet passed
/// among its setup accounts. It works only when that wallet is a fresh signer that is not
/// the creator; every other wallet's signature is stripped. The writable flag is copied:
/// a read-only token account can't be modified even with a fresh signer.
#[test]
fn bok_inv_adm_5_only_fresh_signers_reach_the_strategy() {
    let mut env = registered_evil_env();
    let market_rent = rent(&env, MARKET_SPACE);
    let mint = env.create_mint(&TOKEN, 6, None, None);

    #[derive(Debug, Clone, Copy)]
    enum Wallet {
        Fresh,
        FreshButTokensReadOnly,
        OneLamport,
        FundedThirdParty,
        Creator,
        CreatorTwice,
        CreatorDrainedToZero,
    }
    for wallet in [
        Wallet::Fresh,
        Wallet::FreshButTokensReadOnly,
        Wallet::OneLamport,
        Wallet::FundedThirdParty,
        Wallet::Creator,
        Wallet::CreatorTwice,
        Wallet::CreatorDrainedToZero,
    ] {
        let allowance = SOL / 10;
        let creator = Keypair::new();
        let creator_lamports = match wallet {
            Wallet::CreatorDrainedToZero => market_rent + allowance,
            _ => 10 * SOL,
        };
        env.svm
            .airdrop(&creator.pubkey(), creator_lamports)
            .unwrap();
        let other = Keypair::new();
        match wallet {
            Wallet::OneLamport => env
                .svm
                .set_account(
                    other.pubkey(),
                    solana_account::Account {
                        lamports: 1,
                        data: vec![],
                        owner: client::SYSTEM_PROGRAM_ID,
                        executable: false,
                        rent_epoch: 0,
                    },
                )
                .unwrap(),
            Wallet::FundedThirdParty => {
                env.svm.airdrop(&other.pubkey(), SOL).unwrap();
            }
            _ => {}
        }
        let uses_creator = matches!(
            wallet,
            Wallet::Creator | Wallet::CreatorTwice | Wallet::CreatorDrainedToZero
        );
        let signer = if uses_creator { &creator } else { &other };
        let tokens = env.create_ata(&signer.pubkey(), &mint, &TOKEN);
        env.mint_to(&mint, &TOKEN, &tokens, 1_000);

        let token_meta = match wallet {
            Wallet::FreshButTokensReadOnly => AccountMeta::new_readonly(tokens, false),
            _ => AccountMeta::new(tokens, false),
        };
        let mut extra = vec![
            token_meta,
            AccountMeta::new_readonly(signer.pubkey(), true),
            AccountMeta::new_readonly(TOKEN, false),
        ];
        if matches!(wallet, Wallet::CreatorTwice) {
            extra.push(AccountMeta::new(creator.pubkey(), true));
        }
        let evil = evil_accounts(&mut env);
        let ix = evil_create_ix(
            &creator.pubkey(),
            &evil,
            INIT_APPROVES_CREATOR_TOKENS,
            allowance,
            extra,
        );
        let c0 = env.lamports(&creator.pubkey());
        let signers: Vec<&Keypair> = if uses_creator {
            vec![&creator]
        } else {
            vec![&creator, &other]
        };
        let result = env.send(&[ix], &signers);
        let delegate = delegate_of(&env, &tokens);
        match wallet {
            Wallet::Fresh => {
                result.unwrap_or_else(|e| panic!("{wallet:?}: {:?}", e.err));
                assert_eq!(
                    delegate,
                    Some(evil.state),
                    "a fresh signer keeps its signature"
                );
            }
            Wallet::FreshButTokensReadOnly => {
                assert_fails_with(result, "writable privilege escalated");
                assert_eq!(delegate, None);
                assert_eq!(env.lamports(&creator.pubkey()), c0);
            }
            _ => {
                assert_fails_with(result, "signer privilege escalated");
                assert_eq!(delegate, None, "{wallet:?}: signature reached the strategy");
                assert_eq!(env.lamports(&creator.pubkey()), c0);
            }
        }
    }
}

/// An evil market with liquidity and fee accounts, and a trader holding mint A.
fn evil_market_and_trader(env: &mut Env) -> (Evil, Keypair, Pubkey, Pubkey) {
    let creator = env.funded_keypair();
    let evil = evil_accounts(env);
    let ix = evil_create_ix(&creator.pubkey(), &evil, HONEST, SOL, vec![]);
    env.send(&[ix], &[&creator]).unwrap();
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &evil.market, &evil.mint_a, &TOKEN),
            client::init_fee_account(&payer, &evil.market, &evil.mint_b, &TOKEN),
        ],
        &[],
    )
    .unwrap();
    env.mint_to(&evil.mint_a, &TOKEN, &evil.vault_a, 1_000_000_000);
    env.mint_to(&evil.mint_b, &TOKEN, &evil.vault_b, 1_000_000_000);
    let (trader, trader_a) = env.trader_with(&evil.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &evil.mint_b, &TOKEN);
    (evil, trader, trader_a, trader_b)
}

fn evil_swap_ix(
    evil: &Evil,
    trader: &Pubkey,
    trader_a: &Pubkey,
    trader_b: &Pubkey,
    behaviour: u8,
    extra: Vec<AccountMeta>,
) -> Instruction {
    let mut strategy_accounts = vec![AccountMeta::new(evil.vault_b, false)];
    strategy_accounts.extend(extra);
    let accounts = client::SwapAccounts {
        trader: *trader,
        strategy_program: EVIL_ID,
        market_state: evil.state,
        trader_input: *trader_a,
        trader_output: *trader_b,
        input_vault: evil.vault_a,
        input_mint: evil.mint_a,
        output_mint: evil.mint_b,
        input_token_program: TOKEN,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts,
        input_hook_accounts: vec![],
    };
    let mut args = exact_in(1_000, 0);
    args.extra = vec![behaviour];
    client::swap(&accounts, &args)
}

/// INV-ADM-5 (swap side): the trader's signature never reaches the strategy, even when
/// the trader is also passed as a signing, writable strategy account.
#[test]
fn bok_inv_adm_5_swap_forwards_no_signature() {
    let mut env = registered_evil_env();
    let (evil, trader, trader_a, trader_b) = evil_market_and_trader(&mut env);
    let cases = [
        (
            TAKE_LAMPORTS,
            vec![
                AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
                AccountMeta::new(trader.pubkey(), true),
            ],
        ),
        (TAKE_MORE, vec![AccountMeta::new(trader.pubkey(), true)]),
    ];
    for (behaviour, extra) in cases {
        let lamports = env.lamports(&trader.pubkey());
        let ix = evil_swap_ix(
            &evil,
            &trader.pubkey(),
            &trader_a,
            &trader_b,
            behaviour,
            extra,
        );
        assert_fails_with(env.send(&[ix], &[&trader]), "signer privilege escalated");
        assert_eq!(env.lamports(&trader.pubkey()), lamports);
        assert_eq!(env.token_balance(&trader_a), 10_000_000);
        assert_eq!(env.token_balance(&trader_b), 0);
    }
}

// ---------------------------------------------------------------------------------------
// INV-ADM-6 — accepted market info
// ---------------------------------------------------------------------------------------

/// An evil market state written directly (as if the strategy had created it itself),
/// for `register_existing_market`.
fn plant_evil_state(env: &mut Env, owner: Pubkey, info_mode: u8) -> Evil {
    let evil = evil_accounts(env);
    let mut data = Vec::with_capacity(STATE_LEN);
    for key in [evil.mint_a, evil.mint_b, evil.vault_a, evil.vault_b] {
        data.extend_from_slice(key.as_ref());
    }
    data.push(255);
    data.push(info_mode);
    env.svm
        .set_account(
            evil.state,
            solana_account::Account {
                lamports: rent(env, STATE_LEN),
                data,
                owner,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    evil
}

/// INV-ADM-6: market info with duplicate mints, one mint, or a vault count that doesn't
/// match is refused by both `create_market` and `register_existing_market`; nothing is
/// recorded and the strategy's market count doesn't move.
#[test]
fn bok_inv_adm_6_bad_market_info_is_refused_on_both_paths() {
    let mut env = registered_evil_env();
    let payer = env.funded_keypair();
    for mode in [INFO_DUPLICATE_MINTS, INFO_ONE_MINT, INFO_VAULT_MISMATCH] {
        let count = market_count(&env, &EVIL_ID);
        let evil = evil_accounts(&mut env);
        let c0 = env.lamports(&payer.pubkey());
        let ix = evil_create_ix(&payer.pubkey(), &evil, mode, SOL, vec![]);
        assert_fails_with(env.send(&[ix], &[&payer]), "BadMarketInfo");
        assert!(env.svm.get_account(&evil.market).is_none());
        assert_eq!(env.lamports(&payer.pubkey()), c0);

        let planted = plant_evil_state(&mut env, EVIL_ID, mode);
        let ix =
            client::register_existing_market(&payer.pubkey(), &EVIL_ID, &planted.state, vec![]);
        assert_fails_with(env.send(&[ix], &[&payer]), "BadMarketInfo");
        assert!(env.svm.get_account(&planted.market).is_none());
        assert_eq!(market_count(&env, &EVIL_ID), count);
    }
    // Control: the same planted state with honest info registers.
    let planted = plant_evil_state(&mut env, EVIL_ID, HONEST);
    let ix = client::register_existing_market(&payer.pubkey(), &EVIL_ID, &planted.state, vec![]);
    env.send(&[ix], &[&payer]).unwrap();
}

/// INV-ADM-6: market info is only asked of the program that owns the market state.
/// A state owned by another program (system, token, another registered strategy) is
/// refused before the strategy is called.
#[test]
fn bok_inv_adm_6_market_state_must_belong_to_the_strategy() {
    let mut env = registered_evil_env();
    env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    let payer = env.funded_keypair();
    for owner in [client::SYSTEM_PROGRAM_ID, TOKEN, CP_PROGRAM_ID] {
        let planted = plant_evil_state(&mut env, owner, HONEST);
        let ix =
            client::register_existing_market(&payer.pubkey(), &EVIL_ID, &planted.state, vec![]);
        assert_fails_with(env.send(&[ix], &[&payer]), "MarketStateNotOwnedByStrategy");
        assert!(env.svm.get_account(&planted.market).is_none());
    }
    // An evil-owned state presented as a CP market.
    let planted = plant_evil_state(&mut env, EVIL_ID, HONEST);
    let ix =
        client::register_existing_market(&payer.pubkey(), &CP_PROGRAM_ID, &planted.state, vec![]);
    assert_fails_with(env.send(&[ix], &[&payer]), "MarketStateNotOwnedByStrategy");
}

// ---------------------------------------------------------------------------------------
// INV-ADM-7 — recorded markets are well-formed and written once
// ---------------------------------------------------------------------------------------

fn assert_recorded(env: &Env, evil: &Evil, created_by: &Pubkey, meta: &TransactionMetadata) {
    let market = env.market(&evil.market);
    assert_eq!(market.strategy_program, EVIL_ID);
    assert_eq!(market.market_state, evil.state);
    assert_eq!(market.mint_count, 2);
    assert_eq!(market.mints[..2], [evil.mint_a, evil.mint_b]);
    assert_eq!(market.vaults[..2], [evil.vault_a, evil.vault_b]);
    assert_eq!(market.mints[2..], [Pubkey::default(); 2]);
    assert_eq!(market.vaults[2..], [Pubkey::default(); 2]);
    assert!(!market.supports_exact_out);
    assert_eq!(market.created_by, *created_by);
    let (_, bump) = Pubkey::find_program_address(
        &[b"market", EVIL_ID.as_ref(), evil.state.as_ref()],
        &client::PROGRAM_ID,
    );
    assert_eq!(market.bump, bump);
    let (fee_authority, fee_bump) =
        Pubkey::find_program_address(&[b"fees", evil.market.as_ref()], &client::PROGRAM_ID);
    assert_eq!(fee_authority, pda::fee_authority(&evil.market));
    assert_eq!(market.fee_authority_bump, fee_bump);
    let created: Vec<_> = events(meta)
        .into_iter()
        .filter_map(|e| match e {
            PipelineEvent::MarketCreated(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].market, evil.market);
    assert_eq!(created[0].mints, vec![evil.mint_a, evil.mint_b]);
    assert_eq!(created[0].created_by, *created_by);
}

/// INV-ADM-7: both recording paths store exactly the strategy's info (default slots
/// beyond the mint count), bump the strategy's market count by exactly one, and a market
/// can't be recorded again by either path.
#[test]
fn bok_inv_adm_7_recorded_market_matches_info_and_is_written_once() {
    let mut env = registered_evil_env();
    let creator = env.funded_keypair();

    let evil = evil_accounts(&mut env);
    let count = market_count(&env, &EVIL_ID);
    let ix = evil_create_ix(&creator.pubkey(), &evil, HONEST, SOL, vec![]);
    let meta = env.send(&[ix], &[&creator]).unwrap();
    assert_recorded(&env, &evil, &creator.pubkey(), &meta);
    assert_eq!(market_count(&env, &EVIL_ID), count + 1);

    let planted = plant_evil_state(&mut env, EVIL_ID, HONEST);
    let ix = client::register_existing_market(&creator.pubkey(), &EVIL_ID, &planted.state, vec![]);
    let meta = env.send(&[ix], &[&creator]).unwrap();
    assert_recorded(&env, &planted, &creator.pubkey(), &meta);
    assert_eq!(market_count(&env, &EVIL_ID), count + 2);

    // Neither path can write either market again.
    let intruder = env.funded_keypair();
    for target in [&evil, &planted] {
        let bytes = env.account_data(&target.market);
        let again =
            client::register_existing_market(&intruder.pubkey(), &EVIL_ID, &target.state, vec![]);
        assert!(env.send(&[again], &[&intruder]).is_err());
        let again = evil_create_ix(&intruder.pubkey(), target, HONEST, SOL, vec![]);
        assert!(env.send(&[again], &[&intruder]).is_err());
        assert_eq!(env.account_data(&target.market), bytes);
    }
    assert_eq!(market_count(&env, &EVIL_ID), count + 2);
}

/// INV-ADM-7: mint lookup accepts exactly the recorded mints. A mint of another market
/// and the all-zero key (what the unused slots hold) are refused with `MintNotInMarket`.
#[test]
fn bok_inv_adm_7_mint_lookup_accepts_exactly_the_recorded_mints() {
    let mut env = registered_evil_env();
    let creator = env.funded_keypair();
    let evil = evil_accounts(&mut env);
    let ix = evil_create_ix(&creator.pubkey(), &evil, HONEST, SOL, vec![]);
    env.send(&[ix], &[&creator]).unwrap();
    let payer = env.admin.pubkey();

    let stranger_mint = env.create_mint(&TOKEN, 6, None, None);
    for mint in [stranger_mint, Pubkey::default()] {
        let ix = client::init_fee_account(&payer, &evil.market, &mint, &TOKEN);
        assert_fails_with(env.send(&[ix], &[]), "MintNotInMarket");
    }
    for mint in [evil.mint_a, evil.mint_b] {
        let ix = client::init_fee_account(&payer, &evil.market, &mint, &TOKEN);
        env.send(&[ix], &[]).unwrap();
        let fee_account = pda::fee_account(&evil.market, &mint, &TOKEN);
        assert_eq!(&env.account_data(&fee_account)[..32], mint.as_ref());
    }
}
