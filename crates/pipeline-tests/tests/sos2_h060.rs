//! SOS audit 2, H060: event semantics. This file settles the one runtime claim in the
//! hypothesis that the existing tests do not show: `lift_pause` on an unpaused, frozen
//! router succeeds and still emits `ConfigChanged` (a no-op), so anyone can produce that
//! event. `init_fee_account` emitting nothing is settled by code reading (no `emit_cpi!`
//! in its handler, `lib.rs:359-377`).

mod common;

use common::*;
use pipeline_client::{self as client};
use pipeline_events::PipelineEvent;
use solana_signer::Signer;

/// A frozen router with no pause set: `lift_pause` is a permissionless no-op. It succeeds,
/// leaves the config unchanged, and still emits exactly one `ConfigChanged` carrying the
/// unchanged state.
#[test]
fn h060_lift_pause_on_unpaused_frozen_router_emits_config_changed() {
    let mut env = Env::new();
    set_upgrade_authority(&mut env.svm, &client::PROGRAM_ID, None);
    let before = env.svm.get_account(&client::pda::config()).expect("config");

    let meta = env
        .send(&[client::lift_pause()], &[])
        .expect("lift_pause on an unpaused frozen router succeeds");

    let emitted = events(&meta);
    assert!(
        matches!(
            emitted[..],
            [PipelineEvent::ConfigChanged(ref c)] if !c.paused && c.admin == env.admin.pubkey()
        ),
        "H060: expected exactly one ConfigChanged with paused=false, got {emitted:?}"
    );
    let after = env.svm.get_account(&client::pda::config()).expect("config");
    assert_eq!(
        before.data, after.data,
        "H060: a no-op lift_pause changed the config"
    );
}
