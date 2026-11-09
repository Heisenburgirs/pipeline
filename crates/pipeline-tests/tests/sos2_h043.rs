//! SOS audit #2, H043: a market's fee account has no repair path under issuer actions.
//!
//! Issuer actions (freeze, thaw, pause, transfer hooks) are accepted design, so the
//! question is blast radius and failure mode, not whether the issuer may act.
//!
//! Asserted here, against the real router and Token-2022:
//! - freezing one market's fee account for one mint makes swaps that sell that mint on
//!   that market fail, and the trader keeps the input and the vault is untouched (the
//!   failure is atomic, no fee is lost);
//! - sweeping that fee account fails while it is frozen, and the fee stays in it;
//! - the same mint on another market, and the other mint on the same market, keep
//!   trading and collecting fees;
//! - thawing the account (an issuer action) restores swaps and lets the sweep succeed.
//!   The router needs no repair path for this: the fee account is pinned, and the
//!   issuer, which holds the freeze authority, is the only party who can act.

mod common;

use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use pipeline_strategy_interface::fees;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const RESERVE: u64 = 1_000_000_000;
const SWAP: u64 = 1_000_000;

/// A Token-2022 mint with no extensions whose freeze authority is the admin.
fn freezable_mint(env: &mut Env) -> Pubkey {
    use spl_token_2022_interface::extension::ExtensionType;
    let mint = Keypair::new();
    let space =
        ExtensionType::try_calculate_account_len::<spl_token_2022_interface::state::Mint>(&[])
            .unwrap();
    let rent = env.svm.minimum_balance_for_rent_exemption(space);
    let admin = env.admin.pubkey();
    let ixs = [
        solana_system_interface::instruction::create_account(
            &admin,
            &mint.pubkey(),
            rent,
            space as u64,
            &TOKEN_2022,
        ),
        spl_token_2022_interface::instruction::initialize_mint2(
            &TOKEN_2022,
            &mint.pubkey(),
            &admin,
            Some(&admin),
            6,
        )
        .unwrap(),
    ];
    env.send(&ixs, &[&mint]).expect("create freezable mint");
    mint.pubkey()
}

fn set_fee_account_frozen(env: &mut Env, fee_account: &Pubkey, mint: &Pubkey, freeze: bool) {
    let admin = env.admin.pubkey();
    let ix = if freeze {
        spl_token_2022_interface::instruction::freeze_account(
            &TOKEN_2022,
            fee_account,
            mint,
            &admin,
            &[],
        )
    } else {
        spl_token_2022_interface::instruction::thaw_account(
            &TOKEN_2022,
            fee_account,
            mint,
            &admin,
            &[],
        )
    }
    .unwrap();
    env.send(&[ix], &[]).expect("freeze or thaw");
}

#[test]
fn frozen_fee_account_blocks_only_its_market_and_mint_until_thawed() {
    let mut env = Env::new();
    let mint_a = freezable_mint(&mut env);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let mint_c = env.create_mint(&TOKEN, 6, None, None);
    let market1 = setup_cp_market_with_mints(
        &mut env,
        mint_a,
        TOKEN_2022,
        mint_b,
        TOKEN,
        RESERVE,
        RESERVE,
        30,
        vec![],
    );
    let market2 = setup_cp_market_with_mints(
        &mut env,
        mint_a,
        TOKEN_2022,
        mint_c,
        TOKEN,
        RESERVE,
        RESERVE,
        30,
        vec![],
    );
    let treasury = env.treasury_owner;
    let treasury_a = env.create_ata(&treasury, &mint_a, &TOKEN_2022);

    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN_2022, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN);
    let trader_c = env.create_ata(&trader.pubkey(), &mint_c, &TOKEN);
    env.mint_to(&mint_b, &TOKEN, &trader_b, 10_000_000);
    let fee_a1 = pda::fee_account(&market1.market, &mint_a, &TOKEN_2022);
    let fee_a2 = pda::fee_account(&market2.market, &mint_a, &TOKEN_2022);

    // One sale of A on market 1 puts a fee in its fee account before the issuer freezes it.
    let sell_a1 = cp_swap_accounts(
        &market1,
        &trader.pubkey(),
        &mint_a,
        &trader_a,
        &mint_b,
        &trader_b,
    );
    env.send(&[client::swap(&sell_a1, &exact_in(SWAP, 1))], &[&trader])
        .expect("sell A on market 1 before freeze");
    assert_eq!(env.token_balance(&fee_a1), fees::pipeline_fee(SWAP));

    // The issuer freezes market 1's fee account for mint A.
    set_fee_account_frozen(&mut env, &fee_a1, &mint_a, true);

    // Selling A on market 1 fails. The failure is atomic: the trader keeps the input,
    // the vault is untouched and no fee is recorded anywhere.
    let sell_a1 = cp_swap_accounts(
        &market1,
        &trader.pubkey(),
        &mint_a,
        &trader_a,
        &mint_b,
        &trader_b,
    );
    let trader_a_before = env.token_balance(&trader_a);
    let vault_before = env.token_balance(&market1.vault_a);
    assert_fails_with(
        env.send(&[client::swap(&sell_a1, &exact_in(SWAP, 1))], &[&trader]),
        "frozen",
    );
    assert_eq!(env.token_balance(&trader_a), trader_a_before);
    assert_eq!(env.token_balance(&market1.vault_a), vault_before);
    assert_eq!(env.token_balance(&fee_a1), fees::pipeline_fee(SWAP));

    // Buying A on market 1 (the fee is in B) still works.
    let buy_a1 = cp_swap_accounts(
        &market1,
        &trader.pubkey(),
        &mint_b,
        &trader_b,
        &mint_a,
        &trader_a,
    );
    env.send(&[client::swap(&buy_a1, &exact_in(SWAP, 1))], &[&trader])
        .expect("buy A on market 1");

    // Selling A on market 2 (same mint, different fee account) still works and fees
    // land in market 2's fee account.
    let sell_a2 = cp_swap_accounts(
        &market2,
        &trader.pubkey(),
        &mint_a,
        &trader_a,
        &mint_c,
        &trader_c,
    );
    env.send(&[client::swap(&sell_a2, &exact_in(SWAP, 1))], &[&trader])
        .expect("sell A on market 2");
    assert_eq!(env.token_balance(&fee_a2), fees::pipeline_fee(SWAP));

    // Sweeping market 1's frozen fee account fails and leaves the fee in place.
    let sweep = client::sweep_fees(&market1.market, &mint_a, &TOKEN_2022, &treasury, vec![]);
    assert_fails_with(env.send(&[sweep], &[]), "frozen");
    assert_eq!(env.token_balance(&fee_a1), fees::pipeline_fee(SWAP));

    // Thawing (an issuer action) restores selling A on market 1, and the sweep then
    // moves the fee to the treasury.
    set_fee_account_frozen(&mut env, &fee_a1, &mint_a, false);
    let sell_a1 = cp_swap_accounts(
        &market1,
        &trader.pubkey(),
        &mint_a,
        &trader_a,
        &mint_b,
        &trader_b,
    );
    env.send(&[client::swap(&sell_a1, &exact_in(SWAP, 1))], &[&trader])
        .expect("sell A on market 1 after thaw");
    assert_eq!(env.token_balance(&fee_a1), 2 * fees::pipeline_fee(SWAP));
    let sweep = client::sweep_fees(&market1.market, &mint_a, &TOKEN_2022, &treasury, vec![]);
    env.send(&[sweep], &[]).expect("sweep after thaw");
    assert_eq!(env.token_balance(&fee_a1), 0);
    assert_eq!(env.token_balance(&treasury_a), 2 * fees::pipeline_fee(SWAP));
}
