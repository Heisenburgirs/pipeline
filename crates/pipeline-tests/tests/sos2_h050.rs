//! SOS audit 2, H050: registering the router itself (or another non-strategy program) as a
//! strategy is inert. Only the router's upgrade authority can register it, and every
//! strategy call from the router back into the router uses `pipeline_*` discriminators that
//! match no router instruction, so `create_market` and `register_existing_market` fail.

mod common;

use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use solana_account::Account;
use solana_signer::Signer;

/// A router-owned account standing in for a market state that the router itself could
/// claim to own. The router's own accounts are owned by the router, so this is the shape
/// `fetch_market_info` would accept if the dispatch were not refused.
fn router_owned_state(env: &mut Env) -> Pubkey {
    let state = Pubkey::new_unique();
    env.svm
        .set_account(
            state,
            Account {
                lamports: SOL,
                data: vec![0u8; 64],
                owner: client::PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    state
}

/// Only the router's upgrade authority can register the router. A stranger's registration
/// is refused with `NotStrategyOwner`, and no strategy account is created.
#[test]
fn h050_stranger_cannot_register_router() {
    let mut env = Env::new();
    let attacker = env.funded_keypair();
    let ix = client::register_strategy(
        &attacker.pubkey(),
        &attacker.pubkey(),
        &client::PROGRAM_ID,
        &env.listing_fee_recipient,
        "https://example.com/router.json",
        u64::MAX,
    );
    assert_fails_with(env.send(&[ix], &[&attacker]), "NotStrategyOwner");
    assert!(
        env.svm
            .get_account(&pda::strategy(&client::PROGRAM_ID))
            .is_none(),
        "H050: a refused registration created the strategy account"
    );
}

/// The upgrade authority can register the router as a strategy, which is no more than it
/// could already do by upgrading the program. Markets against it cannot be created or
/// recorded: the router refuses the self-CPI because `pipeline_init_market` and
/// `pipeline_market_info` match no router instruction, so the router fails with
/// `InstructionFallbackNotFound`.
#[test]
fn h050_router_as_strategy_creates_no_market() {
    let mut env = Env::new();
    let admin = env.admin.pubkey();
    let ix = client::register_strategy(
        &admin,
        &admin,
        &client::PROGRAM_ID,
        &env.listing_fee_recipient,
        "https://example.com/router.json",
        u64::MAX,
    );
    env.send(&[ix], &[])
        .expect("authority registers the router");

    let state = router_owned_state(&mut env);
    let create = client::create_market(&client::CreateMarket {
        creator: admin,
        strategy_program: client::PROGRAM_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![],
        max_creator_lamports: 0,
        strategy_accounts: vec![],
    });
    let result = env.send(&[create], &[]);
    assert_fails_with(result, "InstructionFallbackNotFound");
    assert!(
        env.svm
            .get_account(&pda::market(&client::PROGRAM_ID, &state))
            .is_none(),
        "H050: a refused create_market left a market account"
    );

    let record = client::register_existing_market(&admin, &client::PROGRAM_ID, &state, vec![]);
    let result = env.send(&[record], &[]);
    assert_fails_with(result, "InstructionFallbackNotFound");
}
