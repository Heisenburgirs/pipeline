//! SOS audit #2, H053: `metadata_uri` bounds, content and update semantics.
//!
//! The router caps `metadata_uri` at 200 bytes (`String::len`, so UTF-8 bytes, not
//! characters) on both `register_strategy` and `update_metadata`. The `Strategy` account
//! is sized for that maximum, so no update should need a reallocation. Content is not
//! validated (the integrator guide treats it as untrusted), and `update_metadata` is not
//! gated by the pause (H001 relies on that). These tests check the bound at both entry
//! points, the account size after multi-byte URIs, and the accepted content and pause
//! behaviour.

mod common;

use common::*;
use pipeline_client::{self as client, pda};
use solana_signer::Signer;

const MAX_LEN: usize = 200;

fn strategy_account_len(env: &Env) -> usize {
    env.account_data(&pda::strategy(&CP_PROGRAM_ID)).len()
}

fn stored_uri(env: &Env) -> String {
    client::decode_strategy(&env.account_data(&pda::strategy(&CP_PROGRAM_ID)))
        .expect("strategy")
        .metadata_uri
}

#[test]
fn h053_register_rejects_201_bytes_and_accepts_200_multibyte_bytes() {
    let mut env = Env::new();
    let owner = env.funded_keypair();
    env.deploy_strategy(&CP_PROGRAM_ID, "test_strategy_cp", &owner.pubkey());

    let too_long = "a".repeat(MAX_LEN + 1);
    let ix = client::register_strategy(
        &owner.pubkey(),
        &owner.pubkey(),
        &CP_PROGRAM_ID,
        &env.listing_fee_recipient,
        &too_long,
        u64::MAX,
    );
    assert_fails_with(env.send(&[ix], &[&owner]), "MetadataUriTooLong");

    // 50 four-byte characters: 50 characters, exactly 200 bytes, so it must fit.
    let exact = "\u{1F600}".repeat(MAX_LEN / 4);
    assert_eq!(exact.len(), MAX_LEN);
    let ix = client::register_strategy(
        &owner.pubkey(),
        &owner.pubkey(),
        &CP_PROGRAM_ID,
        &env.listing_fee_recipient,
        &exact,
        u64::MAX,
    );
    env.send(&[ix], &[&owner])
        .expect("H053: 200-byte URI must register");
    assert_eq!(stored_uri(&env), exact);
    // Account is sized for the maximum: 8 discriminator + Strategy::INIT_SPACE.
    assert_eq!(
        strategy_account_len(&env),
        8 + 32 + 4 + MAX_LEN + 1 + 8 + 8 + 1
    );
}

#[test]
fn h053_update_rejects_201_bytes_and_keeps_account_size() {
    let mut env = Env::new();
    let owner = env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    let before_len = strategy_account_len(&env);
    let before_uri = stored_uri(&env);

    let too_long = "b".repeat(MAX_LEN + 1);
    let ix = client::update_metadata(&owner.pubkey(), &CP_PROGRAM_ID, &too_long);
    assert_fails_with(env.send(&[ix], &[&owner]), "MetadataUriTooLong");
    assert_eq!(stored_uri(&env), before_uri);

    // 100 two-byte characters: exactly 200 bytes. No realloc is needed, so the size stays.
    let exact = "\u{00E9}".repeat(MAX_LEN / 2);
    assert_eq!(exact.len(), MAX_LEN);
    let ix = client::update_metadata(&owner.pubkey(), &CP_PROGRAM_ID, &exact);
    env.send(&[ix], &[&owner])
        .expect("H053: 200-byte update must succeed");
    assert_eq!(stored_uri(&env), exact);
    assert_eq!(strategy_account_len(&env), before_len);
}

#[test]
fn h053_update_is_not_gated_by_pause_and_content_is_not_validated() {
    let mut env = Env::new();
    let owner = env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    let admin = env.admin.insecure_clone();

    let pause = client::update_config(
        &admin.pubkey(),
        &client::UpdateConfigArgs {
            paused: Some(true),
            ..Default::default()
        },
    );
    env.send(&[pause], &[&admin]).expect("pause");

    // Accepted design: the URI is changeable while paused and its content is not checked.
    // A `javascript:` URI with control characters is stored as given; front-ends must
    // treat it as untrusted (see docs/INTEGRATOR-GUIDE.md).
    let hostile = "javascript:alert(1)\u{202E}\u{0007}";
    let ix = client::update_metadata(&owner.pubkey(), &CP_PROGRAM_ID, hostile);
    env.send(&[ix], &[&owner])
        .expect("H053: update while paused must succeed (accepted, H001)");
    assert_eq!(stored_uri(&env), hostile);
}
