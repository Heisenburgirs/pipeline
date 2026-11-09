//! SOS audit #2, H017: a strategy can relay its market-PDA signature into another
//! strategy that trusts "a Pipeline market signer".
//!
//! The router signs `["market", strategy, market_state]` toward the strategy of that one
//! market. A hostile strategy (M1) could forward that signature to the CP strategy (M2).
//! The test fixtures contain no behaviour that relays the market PDA (the evil strategy's
//! `FORWARD_TO_ROUTER` signs with its `pipeline-register` PDA, not the market PDA), so
//! the relay itself is settled by code reading plus these tests of what the victim
//! checks:
//!   * the victim stores and compares the exact market PDA of its own pool;
//!   * M1's market PDA is a different address from every CP market PDA, and a foreign
//!     signer (an attacker keypair) at the market-signer slot is refused by both
//!     `pipeline_swap` and `pipeline_init_market`;
//!   * the market PDA is a router-owned data account, so a strategy that checked only
//!     "owned by the router" would accept any market (the documented footgun).
//!
//! All tests pass on the code as reviewed.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_strategy_interface::{self as psi, TradeMode};
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
const FEE_BPS: u16 = 30;
const RESERVE: u64 = 1_000_000_000;

/// `Pool` layout in the CP strategy: 8-byte Anchor discriminator, four keys, two u64
/// reserves, a u16 fee, then the stored market signer.
const CP_MARKET_SIGNER_OFFSET: usize = 8 + 32 * 4 + 8 * 2 + 2;

fn cp_stored_signer(env: &Env, pool: &Pubkey) -> Pubkey {
    let data = env.account_data(pool);
    Pubkey::try_from(&data[CP_MARKET_SIGNER_OFFSET..CP_MARKET_SIGNER_OFFSET + 32]).unwrap()
}

#[allow(dead_code)]
struct EvilMarket {
    state: Pubkey,
    market: Pubkey,
}

/// A hostile market M1: the evil strategy, registered and created through the router.
fn setup_evil_market(env: &mut Env) -> EvilMarket {
    env.register(&EVIL_ID, "test_strategy_evil");
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let (state, _) =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID);
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
    env.send(&[ix], &[&creator])
        .expect("setup: create the evil market M1");
    EvilMarket {
        state,
        market: pda::market(&EVIL_ID, &state),
    }
}

/// A direct call to the CP strategy's `pipeline_swap` with `signer` in the market-signer
/// slot (signing). Nothing about the router is involved.
fn direct_cp_swap_ix(
    signer: &Pubkey,
    m: &CpMarket,
    trader: &Pubkey,
    trader_a: &Pubkey,
    trader_b: &Pubkey,
    amount: u64,
) -> Instruction {
    let request = psi::SwapRequest {
        trade: psi::TradeRequest {
            mode: TradeMode::ExactIn,
            input_index: 0,
            output_index: 1,
            amount,
            trader: None,
            extra: vec![],
        },
        amount_sent: amount,
        min_amount_out: 0,
    };
    Instruction::new_with_bytes(
        CP_PROGRAM_ID,
        &psi::instruction_data(psi::discriminator::SWAP, &request),
        vec![
            AccountMeta::new_readonly(*signer, true),
            AccountMeta::new(m.pool, false),
            AccountMeta::new_readonly(*trader, false),
            AccountMeta::new(*trader_a, false),
            AccountMeta::new(*trader_b, false),
            AccountMeta::new(m.vault_a, false),
            AccountMeta::new_readonly(m.mint_a, false),
            AccountMeta::new_readonly(m.mint_b, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new(m.vault_b, false),
        ],
    )
}

/// M1 (hostile) and M2 (CP, with liquidity) side by side, plus a trader holding mint A.
struct World {
    env: Env,
    m1: EvilMarket,
    m2: CpMarket,
    trader: Keypair,
    trader_a: Pubkey,
    trader_b: Pubkey,
}

fn world() -> World {
    let mut env = Env::new();
    let m1 = setup_evil_market(&mut env);
    let m2 = setup_cp_market(&mut env, TOKEN, TOKEN, RESERVE, RESERVE, FEE_BPS);
    let (trader, trader_a) = env.trader_with(&m2.mint_a, &TOKEN, 100_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &m2.mint_b, &TOKEN);
    World {
        env,
        m1,
        m2,
        trader,
        trader_a,
        trader_b,
    }
}

/// Setup check plus identity facts: the routed CP swap works, the CP pool stores exactly
/// its own market PDA, and M1's market PDA is a different, router-owned account.
#[test]
fn h017_victim_stores_exact_market_pda_and_it_differs_from_hostile_market_pda() {
    let mut w = world();

    // Setup: an honest routed swap on M2 works (the router signs M2's PDA toward CP).
    let accounts = cp_swap_accounts(
        &w.m2,
        &w.trader.pubkey(),
        &w.m2.mint_a,
        &w.trader_a,
        &w.m2.mint_b,
        &w.trader_b,
    );
    let ix = client::swap(&accounts, &exact_in(1_000_000, 1));
    w.env
        .send(&[ix], &[&w.trader])
        .expect("setup: routed swap on the CP market M2 succeeds");

    let m2_pda = pda::market(&CP_PROGRAM_ID, &w.m2.pool);
    assert_eq!(m2_pda, w.m2.market, "setup: helper derivation");
    let stored = cp_stored_signer(&w.env, &w.m2.pool);
    assert_eq!(
        stored, m2_pda,
        "H017: the victim pool must store exactly PDA([market, CP, pool])"
    );
    assert_ne!(
        w.m1.market, stored,
        "H017: the hostile market's PDA must not equal the victim's stored market signer"
    );

    // Both PDAs are router-owned Market records: an owner-only check ("signer is owned
    // by the router") could not tell M1 from M2.
    let owner = |k: &Pubkey| w.env.svm.get_account(k).expect("market account").owner;
    assert_eq!(owner(&w.m1.market), client::PROGRAM_ID);
    assert_eq!(owner(&w.m2.market), client::PROGRAM_ID);
}

/// The victim's swap accepts only the stored market PDA: a signer that is anything else
/// (here an attacker keypair; M1's PDA cannot sign outside a router CPI) is refused, and
/// nothing moves.
#[test]
fn h017_victim_swap_refuses_any_signer_other_than_its_own_market_pda() {
    let mut w = world();
    let attacker = w.env.funded_keypair();
    let amount = 1_000_000;

    let before_trader = w.env.token_balance(&w.trader_a);
    let before_vault_b = w.env.token_balance(&w.m2.vault_b);

    // Setup/control: the instruction is well-formed; only the signer differs from what
    // the router would supply. (The router-signed version is covered by the test above.)
    let ix = direct_cp_swap_ix(
        &attacker.pubkey(),
        &w.m2,
        &w.trader.pubkey(),
        &w.trader_a,
        &w.trader_b,
        amount,
    );
    assert_fails_with(w.env.send(&[ix], &[&attacker]), "NotPipeline");

    // M1's market PDA in the signer slot, unsigned (the only way to name it outside a
    // router CPI): refused too, and not as an authorised call.
    let ix = direct_cp_swap_ix(
        &w.m1.market,
        &w.m2,
        &w.trader.pubkey(),
        &w.trader_a,
        &w.trader_b,
        amount,
    );
    // A signing meta without a signature cannot be sent, so mark it unsigned.
    let mut ix = ix;
    ix.accounts[0] = AccountMeta::new_readonly(w.m1.market, false);
    let result = w.env.send(&[ix], &[]);
    assert!(
        result.is_err(),
        "H017 VIOLATED: the victim accepted hostile market M1's PDA as its market signer"
    );

    assert_eq!(w.env.token_balance(&w.trader_a), before_trader);
    assert_eq!(w.env.token_balance(&w.m2.vault_b), before_vault_b);
}

/// The victim's market creation also binds the exact PDA: calling `pipeline_init_market`
/// directly with an attacker signer for a fresh pool is refused and creates nothing; the
/// router's own `create_market` for the same mints then still works.
#[test]
fn h017_victim_init_refuses_foreign_market_signer_and_leaves_no_state() {
    let mut w = world();
    let attacker = w.env.funded_keypair();
    let mint_a = w.env.create_mint(&TOKEN, 6, None, None);
    let mint_b = w.env.create_mint(&TOKEN, 6, None, None);
    let pool = cp_pool(&mint_a, &mint_b, FEE_BPS);
    let vault_a = pda::associated_token(&pool, &mint_a, &TOKEN);
    let vault_b = pda::associated_token(&pool, &mint_b, &TOKEN);

    let init_data = borsh::to_vec(&FEE_BPS).unwrap();
    let direct_init = Instruction::new_with_bytes(
        CP_PROGRAM_ID,
        &psi::instruction_data(psi::discriminator::INIT_MARKET, &init_data),
        vec![
            AccountMeta::new_readonly(attacker.pubkey(), true),
            AccountMeta::new(pool, false),
            AccountMeta::new(attacker.pubkey(), true),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(client::ASSOCIATED_TOKEN_PROGRAM_ID, false),
        ],
    );
    assert_fails_with(w.env.send(&[direct_init], &[&attacker]), "NotPipeline");
    assert!(
        w.env.svm.get_account(&pool).is_none(),
        "H017: the refused init must not have created the pool"
    );

    // The router path for the same mints still works (no one is blocked).
    let creator = w.env.funded_keypair();
    let ix = cp_create_market_ix(&creator.pubkey(), &mint_a, &TOKEN, &mint_b, &TOKEN, FEE_BPS);
    w.env
        .send(&[ix], &[&creator])
        .expect("control: router create_market for the same mints succeeds");
    assert_eq!(
        cp_stored_signer(&w.env, &pool),
        pda::market(&CP_PROGRAM_ID, &pool),
        "the new pool binds its own exact market PDA"
    );
}
