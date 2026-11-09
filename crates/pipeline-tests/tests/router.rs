//! Router behaviour with the honest constant-product strategy and SPL tokens.

mod common;

use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use pipeline_events::PipelineEvent;
use pipeline_strategy_interface::{fees, TradeMode};
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;

#[test]
fn config_can_only_be_initialised_by_the_upgrade_authority() {
    let mut env = Env::new();
    let config = client::decode_config(&env.account_data(&pda::config())).unwrap();
    assert_eq!(config.admin, env.admin.pubkey());
    assert_eq!(config.listing_fee_lamports, LISTING_FEE);
    assert!(!config.paused);

    // A second initialisation fails because the account exists; a stranger also fails.
    let stranger = env.funded_keypair();
    let ix = client::initialize_config(
        &stranger.pubkey(),
        &client::ConfigArgs {
            listing_fee_lamports: 0,
            listing_fee_recipient: stranger.pubkey(),
            treasury_owner: stranger.pubkey(),
        },
    );
    assert!(env.send(&[ix], &[&stranger]).is_err());
}

#[test]
fn only_admin_updates_config_and_fee_is_capped() {
    let mut env = Env::new();
    let stranger = env.funded_keypair();
    let ix = client::update_config(
        &stranger.pubkey(),
        &client::UpdateConfigArgs {
            paused: Some(true),
            ..Default::default()
        },
    );
    assert_fails_with(env.send(&[ix], &[&stranger]), "NotAdmin");

    let admin = env.admin.pubkey();
    let too_high = client::update_config(
        &admin,
        &client::UpdateConfigArgs {
            listing_fee_lamports: Some(1_000_000_000_001),
            ..Default::default()
        },
    );
    assert_fails_with(env.send(&[too_high], &[]), "ListingFeeTooHigh");

    // A non-zero fee below the rent-exempt minimum would make registrations to an
    // unfunded recipient fail, so it's refused; 0 is fine.
    let dust = client::update_config(
        &admin,
        &client::UpdateConfigArgs {
            listing_fee_lamports: Some(5),
            ..Default::default()
        },
    );
    assert_fails_with(env.send(&[dust], &[]), "ListingFeeBelowRent");
    for fee in [0, 1_000_000] {
        let ok = client::update_config(
            &admin,
            &client::UpdateConfigArgs {
                listing_fee_lamports: Some(fee),
                ..Default::default()
            },
        );
        let meta = env.send(&[ok], &[]).unwrap();
        assert!(
            matches!(events(&meta)[..], [PipelineEvent::ConfigChanged(ref c)] if c.listing_fee_lamports == fee)
        );
    }
}

/// The pause power ends when the router is frozen. A frozen router can't be
/// paused, and anyone can lift a pause left in place, so a lost admin key can't freeze
/// every market for good.
#[test]
fn the_pause_power_ends_when_the_router_is_frozen() {
    let mut env = Env::new();
    let admin = env.admin.pubkey();
    let pause = |paused| {
        client::update_config(
            &admin,
            &client::UpdateConfigArgs {
                paused: Some(paused),
                ..Default::default()
            },
        )
    };
    // While upgradeable: the admin pauses, and `lift_pause` (which needs no signer) is
    // refused.
    env.send(&[pause(true)], &[]).expect("pause");
    assert_fails_with(env.send(&[client::lift_pause()], &[]), "RouterNotFrozen");

    // Frozen with the pause still on: anyone can lift it.
    set_upgrade_authority(&mut env.svm, &client::PROGRAM_ID, None);
    env.send(&[client::lift_pause()], &[])
        .expect("lift the pause");
    let config = client::decode_config(&env.account_data(&client::pda::config())).unwrap();
    assert!(!config.paused);

    // And the admin can no longer pause; other settings still work.
    assert_fails_with(env.send(&[pause(true)], &[]), "FrozenRouterCannotPause");
    env.send(&[pause(false)], &[])
        .expect("unpausing is always allowed");
}

/// A registration names the most it will pay, so raising the listing fee can't surprise
/// a registration already signed.
#[test]
fn registration_refuses_a_listing_fee_above_its_maximum() {
    let mut env = Env::new();
    let owner = env.funded_keypair();
    env.deploy_strategy(&CP_PROGRAM_ID, "test_strategy_cp", &owner.pubkey());
    let recipient = env.listing_fee_recipient;
    let register = |max| {
        client::register_strategy(
            &owner.pubkey(),
            &owner.pubkey(),
            &CP_PROGRAM_ID,
            &recipient,
            "https://example.com/strategy.json",
            max,
        )
    };
    let ix = register(LISTING_FEE - 1);
    assert_fails_with(env.send(&[ix], &[&owner]), "ListingFeeAboveMaximum");
    let ix = register(LISTING_FEE);
    env.send(&[ix], &[&owner])
        .expect("register at the expected fee");
}

#[test]
fn registration_requires_the_upgrade_authority_and_pays_the_listing_fee() {
    let mut env = Env::new();
    let owner = env.funded_keypair();
    env.deploy_strategy(&CP_PROGRAM_ID, "test_strategy_cp", &owner.pubkey());

    let impostor = env.funded_keypair();
    let ix = client::register_strategy(
        &impostor.pubkey(),
        &impostor.pubkey(),
        &CP_PROGRAM_ID,
        &env.listing_fee_recipient,
        "uri",
        u64::MAX,
    );
    assert_fails_with(env.send(&[ix], &[&impostor]), "NotStrategyOwner");

    let recipient_before = env.lamports(&env.listing_fee_recipient);
    let ix = client::register_strategy(
        &owner.pubkey(),
        &owner.pubkey(),
        &CP_PROGRAM_ID,
        &env.listing_fee_recipient,
        "https://example.com/cp.json",
        u64::MAX,
    );
    let meta = env.send(&[ix], &[&owner]).unwrap();
    assert_eq!(
        env.lamports(&env.listing_fee_recipient) - recipient_before,
        LISTING_FEE
    );
    let strategy =
        client::decode_strategy(&env.account_data(&pda::strategy(&CP_PROGRAM_ID))).unwrap();
    assert_eq!(strategy.program_id, CP_PROGRAM_ID);
    assert_eq!(strategy.metadata_uri, "https://example.com/cp.json");
    assert!(
        matches!(events(&meta)[..], [PipelineEvent::StrategyRegistered(ref r)] if r.fee_paid == LISTING_FEE)
    );

    // Registering twice fails; the metadata can be updated by the owner only.
    let again = client::register_strategy(
        &owner.pubkey(),
        &owner.pubkey(),
        &CP_PROGRAM_ID,
        &env.listing_fee_recipient,
        "x",
        u64::MAX,
    );
    assert!(env.send(&[again], &[&owner]).is_err());
    let bad_update = client::update_metadata(&impostor.pubkey(), &CP_PROGRAM_ID, "evil");
    assert_fails_with(env.send(&[bad_update], &[&impostor]), "NotStrategyOwner");
    let update = client::update_metadata(
        &owner.pubkey(),
        &CP_PROGRAM_ID,
        "https://example.com/v2.json",
    );
    env.send(&[update], &[&owner]).unwrap();
    let strategy =
        client::decode_strategy(&env.account_data(&pda::strategy(&CP_PROGRAM_ID))).unwrap();
    assert_eq!(strategy.metadata_uri, "https://example.com/v2.json");
}

#[test]
fn immutable_strategy_cannot_be_registered_by_anyone_but_its_pda() {
    let mut env = Env::new();
    let owner = env.funded_keypair();
    env.deploy_strategy(&CP_PROGRAM_ID, "test_strategy_cp", &owner.pubkey());
    set_upgrade_authority(&mut env.svm, &CP_PROGRAM_ID, None);
    let ix = client::register_strategy(
        &owner.pubkey(),
        &owner.pubkey(),
        &CP_PROGRAM_ID,
        &env.listing_fee_recipient,
        "uri",
        u64::MAX,
    );
    assert_fails_with(env.send(&[ix], &[&owner]), "NotStrategyOwner");
}

#[test]
fn create_market_records_strategy_market_info() {
    let mut env = Env::new();
    let market = setup_cp_market(
        &mut env,
        TOKEN,
        client::TOKEN_2022_PROGRAM_ID,
        1_000_000,
        2_000_000,
        30,
    );
    let account = env.market(&market.market);
    assert_eq!(account.strategy_program, CP_PROGRAM_ID);
    assert_eq!(account.market_state, market.pool);
    assert_eq!(account.mints(), &[market.mint_a, market.mint_b]);
    assert_eq!(account.vault_for(&market.mint_b), Some(market.vault_b));
    assert!(account.supports_exact_out);
    let strategy =
        client::decode_strategy(&env.account_data(&pda::strategy(&CP_PROGRAM_ID))).unwrap();
    assert_eq!(strategy.market_count, 1);
    // Fee accounts exist and are owned by the fee authority, not the market signer.
    let fee_account = pda::fee_account(&market.market, &market.mint_a, &TOKEN);
    let data = env.account_data(&fee_account);
    assert_eq!(&data[32..64], pda::fee_authority(&market.market).as_ref());
}

#[test]
fn exact_in_swap_charges_fee_on_input_and_emits_event() {
    let mut env = Env::new();
    let (ra, rb) = (1_000_000_000u64, 2_000_000_000u64);
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, ra, rb, 30);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);

    let amount_in = 1_000_000;
    let expected_out = cp_out(ra, rb, amount_in, 30);
    let expected_fee = fees::pipeline_fee(amount_in);
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    let ix = client::swap(&accounts, &exact_in(amount_in, expected_out));
    let meta = env.send(&[ix], &[&trader]).unwrap();

    assert_eq!(
        env.token_balance(&trader_a),
        10_000_000 - amount_in - expected_fee
    );
    assert_eq!(env.token_balance(&trader_b), expected_out);
    let fee_account = pda::fee_account(&market.market, &market.mint_a, &TOKEN);
    assert_eq!(env.token_balance(&fee_account), expected_fee);
    assert_eq!(env.token_balance(&market.vault_a), ra + amount_in);

    let swaps = swap_events(&meta);
    assert_eq!(swaps.len(), 1);
    let event = &swaps[0];
    assert_eq!(event.market, market.market);
    assert_eq!(event.strategy, CP_PROGRAM_ID);
    assert_eq!(event.trader, trader.pubkey());
    assert_eq!(
        (event.input_mint, event.output_mint),
        (market.mint_a, market.mint_b)
    );
    assert_eq!(
        (event.net_sent, event.received, event.pipeline_fee),
        (amount_in, expected_out, expected_fee)
    );
    assert_eq!(event.integrator, None);

    let result: client::SwapResult = borsh::from_slice(&meta.return_data.data).unwrap();
    assert_eq!(result.net_sent, amount_in);
    assert_eq!(result.received, expected_out);
    assert_eq!(meta.return_data.program_id, client::PROGRAM_ID);
}

#[test]
fn min_amount_out_is_enforced() {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000_000, 2_000_000_000, 30);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);
    let expected_out = cp_out(1_000_000_000, 2_000_000_000, 1_000_000, 30);
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    let ix = client::swap(&accounts, &exact_in(1_000_000, expected_out + 1));
    assert_fails_with(env.send(&[ix], &[&trader]), "InsufficientOutput");
    assert_eq!(env.token_balance(&trader_a), 10_000_000);
}

#[test]
fn exact_out_swap_refunds_unused_input_and_charges_only_what_was_used() {
    let mut env = Env::new();
    let (ra, rb) = (1_000_000_000u64, 2_000_000_000u64);
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, ra, rb, 30);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);

    let want_out = 1_500_000;
    let needed = cp_in(ra, rb, want_out, 30);
    let max_in = needed * 2;
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    let args = client::SwapArgs {
        mode: TradeMode::ExactOut,
        amount_in: max_in,
        amount_out: want_out,
        min_amount_out: want_out,
        integrator_fee_bps: 0,
        input_hook_accounts: 0,
        extra: vec![],
    };
    let meta = env
        .send(&[client::swap(&accounts, &args)], &[&trader])
        .unwrap();
    let fee = fees::pipeline_fee(needed);
    assert_eq!(env.token_balance(&trader_b), want_out);
    assert_eq!(env.token_balance(&trader_a), 10_000_000 - needed - fee);
    let event = &swap_events(&meta)[0];
    assert_eq!(
        (event.mode, event.net_sent, event.pipeline_fee),
        (1, needed, fee)
    );
}

#[test]
fn integrator_fee_is_charged_alongside_pipeline_fee() {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000_000, 2_000_000_000, 30);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);
    let integrator = Pubkey::new_unique();
    let integrator_a = env.create_ata(&integrator, &market.mint_a, &TOKEN);

    let amount_in = 1_000_001;
    let mut accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    // An integrator fee without an account is rejected.
    let mut args = exact_in(amount_in, 0);
    args.integrator_fee_bps = 75;
    assert_fails_with(
        env.send(&[client::swap(&accounts, &args)], &[&trader]),
        "MissingIntegratorFeeAccount",
    );

    accounts.integrator_fee_account = Some(integrator_a);
    let meta = env
        .send(&[client::swap(&accounts, &args)], &[&trader])
        .unwrap();
    let pipeline_fee = fees::pipeline_fee(amount_in);
    let integrator_fee = fees::ceil_bps(amount_in, 75).unwrap();
    assert_eq!(env.token_balance(&integrator_a), integrator_fee);
    assert_eq!(
        env.token_balance(&trader_a),
        10_000_000 - amount_in - pipeline_fee - integrator_fee
    );
    assert_eq!(
        fees::max_total_spend(amount_in, 75).unwrap(),
        amount_in + pipeline_fee + integrator_fee
    );
    let event = &swap_events(&meta)[0];
    assert_eq!(event.integrator, Some(integrator_a));
    assert_eq!(event.integrator_fee, integrator_fee);
}

#[test]
fn tiny_swaps_pay_at_least_one_unit() {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000_000, 2_000_000_000, 0);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 1_000);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    let meta = env
        .send(&[client::swap(&accounts, &exact_in(10, 0))], &[&trader])
        .unwrap();
    assert_eq!(swap_events(&meta)[0].pipeline_fee, 1);
    assert_eq!(env.token_balance(&trader_a), 1_000 - 10 - 1);
}

#[test]
fn pause_blocks_swaps_and_market_creation_until_unpaused() {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000_000, 2_000_000_000, 30);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);
    let admin = env.admin.pubkey();
    let pause = |paused| {
        client::update_config(
            &admin,
            &client::UpdateConfigArgs {
                paused: Some(paused),
                ..Default::default()
            },
        )
    };

    env.send(&[pause(true)], &[]).unwrap();
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    assert_fails_with(
        env.send(&[client::swap(&accounts, &exact_in(1_000, 0))], &[&trader]),
        "Paused",
    );

    let mint_c = env.create_mint(&TOKEN, 6, None, None);
    let creator = env.funded_keypair();
    let create = cp_create_market_ix(
        &creator.pubkey(),
        &market.mint_a,
        &TOKEN,
        &mint_c,
        &TOKEN,
        30,
    );
    assert_fails_with(
        env.send(std::slice::from_ref(&create), &[&creator]),
        "Paused",
    );

    env.send(&[pause(false)], &[]).unwrap();
    env.send(&[client::swap(&accounts, &exact_in(1_000, 0))], &[&trader])
        .unwrap();
    env.send(&[create], &[&creator]).unwrap();
}

#[test]
fn sweep_moves_fees_to_the_treasury() {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000_000, 2_000_000_000, 30);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    env.send(
        &[client::swap(&accounts, &exact_in(4_000_000, 0))],
        &[&trader],
    )
    .unwrap();

    let treasury_owner = env.treasury_owner;
    let treasury = env.create_ata(&treasury_owner, &market.mint_a, &TOKEN);
    let fee_account = pda::fee_account(&market.market, &market.mint_a, &TOKEN);
    let collected = env.token_balance(&fee_account);
    assert_eq!(collected, fees::pipeline_fee(4_000_000));

    // Sweeping needs no signer beyond the fee payer, so anyone can do it; the
    // destination is fixed by the config.
    let ix = client::sweep_fees(
        &market.market,
        &market.mint_a,
        &TOKEN,
        &treasury_owner,
        vec![],
    );
    assert!(ix.accounts.iter().all(|a| !a.is_signer));
    let meta = env.send(&[ix], &[]).unwrap();
    assert_eq!(env.token_balance(&treasury), collected);
    assert_eq!(env.token_balance(&fee_account), 0);
    assert!(events(&meta)
        .iter()
        .any(|e| matches!(e, PipelineEvent::FeesSwept(s) if s.amount == collected)));

    // Sweeping to someone else's account is rejected.
    let thief = Keypair::new();
    let thief_account = env.create_ata(&thief.pubkey(), &market.mint_a, &TOKEN);
    let bad = client::sweep_fees(
        &market.market,
        &market.mint_a,
        &TOKEN,
        &thief.pubkey(),
        vec![],
    );
    assert_eq!(bad.accounts[5].pubkey, thief_account);
    assert_fails_with(env.send(&[bad], &[]), "WrongTreasuryAccount");
}

#[test]
fn router_rejects_wrong_vault_and_fee_account() {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000_000, 2_000_000_000, 30);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);

    // Sending input to an account the trader's front-end controls instead of the vault.
    let attacker = Keypair::new();
    let attacker_a = env.create_ata(&attacker.pubkey(), &market.mint_a, &TOKEN);
    let mut accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    accounts.input_vault = attacker_a;
    assert_fails_with(
        env.send(&[client::swap(&accounts, &exact_in(1_000, 0))], &[&trader]),
        "WrongVault",
    );

    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    let mut ix = client::swap(&accounts, &exact_in(1_000, 0));
    ix.accounts[13].pubkey = attacker_a;
    assert_fails_with(env.send(&[ix], &[&trader]), "WrongFeeAccount");
    assert_eq!(env.token_balance(&trader_a), 10_000_000);
}

#[test]
fn two_hop_route_in_one_transaction_pays_each_hop() {
    let mut env = Env::new();
    let ab = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000_000, 1_000_000_000, 30);
    let mint_c = env.create_mint(&TOKEN, 6, None, None);
    let bc = setup_cp_market_with_mints(
        &mut env,
        ab.mint_b,
        TOKEN,
        mint_c,
        TOKEN,
        1_000_000_000,
        1_000_000_000,
        30,
        vec![],
    );

    let (trader, trader_a) = env.trader_with(&ab.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &ab.mint_b, &TOKEN);
    let trader_c = env.create_ata(&trader.pubkey(), &mint_c, &TOKEN);

    let first_out = cp_out(1_000_000_000, 1_000_000_000, 1_000_000, 30);
    // Spend everything received from the first hop, including the second hop's fee.
    let second_in = fees::amount_for_budget(first_out, 0);
    let second_out = cp_out(1_000_000_000, 1_000_000_000, second_in, 30);

    let hop1 = client::swap(
        &cp_swap_accounts(
            &ab,
            &trader.pubkey(),
            &ab.mint_a,
            &trader_a,
            &ab.mint_b,
            &trader_b,
        ),
        &exact_in(1_000_000, 0),
    );
    let hop2 = client::swap(
        &cp_swap_accounts(
            &bc,
            &trader.pubkey(),
            &ab.mint_b,
            &trader_b,
            &mint_c,
            &trader_c,
        ),
        &exact_in(second_in, second_out),
    );
    let meta = env.send(&[hop1.clone(), hop2], &[&trader]).unwrap();
    assert_eq!(swap_events(&meta).len(), 2);
    assert_eq!(env.token_balance(&trader_c), second_out);
    assert_eq!(env.token_balance(&trader_b), 0);

    // If the last hop's minimum fails, the whole route reverts.
    let hop2_strict = client::swap(
        &cp_swap_accounts(
            &bc,
            &trader.pubkey(),
            &ab.mint_b,
            &trader_b,
            &mint_c,
            &trader_c,
        ),
        &exact_in(1, u64::MAX),
    );
    let before_a = env.token_balance(&trader_a);
    assert!(env.send(&[hop1, hop2_strict], &[&trader]).is_err());
    assert_eq!(env.token_balance(&trader_a), before_a);
}

#[test]
fn strategy_upgrade_does_not_stop_routing() {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000_000, 2_000_000_000, 30);
    // Simulate a redeploy: the ProgramData slot changes.
    let program_data = pda::program_data(&CP_PROGRAM_ID);
    let mut account = env.svm.get_account(&program_data).unwrap();
    account.data[4..12].copy_from_slice(&999u64.to_le_bytes());
    env.svm.set_account(program_data, account).unwrap();

    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    env.send(&[client::swap(&accounts, &exact_in(1_000, 0))], &[&trader])
        .unwrap();
}

/// A swap that moves nothing either way is refused rather than recorded as an empty
/// trade.
#[test]
fn a_swap_that_moves_nothing_is_refused() {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000_000, 2_000_000_000, 30);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    let ix = client::swap(&accounts, &exact_in(0, 0));
    assert_fails_with(env.send(&[ix], &[&trader]), "NothingSwapped");
}

/// Pipeline's fee account, or the integrator's, can't be the trader's input account: a
/// "fee" paid to the account it came from would be recorded but never collected.
#[test]
fn fee_accounts_cannot_be_the_traders_input() {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000_000, 2_000_000_000, 30);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &market.mint_b, &TOKEN);
    let mut accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &trader_b,
    );
    // Integrator account = the trader's input account.
    accounts.integrator_fee_account = Some(trader_a);
    let mut args = exact_in(1_000_000, 0);
    args.integrator_fee_bps = 10;
    let ix = client::swap(&accounts, &args);
    assert_fails_with(env.send(&[ix], &[&trader]), "FeeToTraderInput");

    // Trader input = this market's fee account for the input mint.
    let fee_account = pda::fee_account(&market.market, &market.mint_a, &TOKEN);
    let mut accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &fee_account,
        &market.mint_b,
        &trader_b,
    );
    accounts.integrator_fee_account = None;
    let ix = client::swap(&accounts, &exact_in(1_000_000, 0));
    assert_fails_with(env.send(&[ix], &[&trader]), "FeeToTraderInput");
}

/// An SPL multisig account (355 bytes) can't pass as a token account, even with byte 165
/// set to Token-2022's "account" type.
#[test]
fn a_multisig_account_is_not_a_token_account() {
    let mut env = Env::new();
    let market = setup_cp_market(&mut env, TOKEN, TOKEN, 1_000_000_000, 2_000_000_000, 30);
    let (trader, trader_a) = env.trader_with(&market.mint_a, &TOKEN, 10_000_000);
    let fake_output = Pubkey::new_unique();
    let mut data = vec![0u8; 355];
    data[..32].copy_from_slice(market.mint_b.as_ref());
    data[32..64].copy_from_slice(trader.pubkey().as_ref());
    data[165] = 2;
    env.svm
        .set_account(
            fake_output,
            solana_account::Account {
                lamports: 10_000_000,
                data,
                owner: TOKEN,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    let accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &market.mint_a,
        &trader_a,
        &market.mint_b,
        &fake_output,
    );
    let ix = client::swap(&accounts, &exact_in(1_000_000, 0));
    assert_fails_with(env.send(&[ix], &[&trader]), "BadTokenAccount");
}
