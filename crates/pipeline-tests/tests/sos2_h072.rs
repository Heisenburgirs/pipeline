//! SOS audit #2, H072: the wire format and discriminators match what the docs say and
//! do not collide.
//!
//! Properties:
//! 1. every strategy discriminator in `pipeline_strategy_interface::discriminator` equals
//!    `sha256("global:pipeline_<name>")[..8]`, the rule the strategy guide documents;
//! 2. the router's instruction discriminators, computed from the `pub fn` names in
//!    `programs/pipeline/src/lib.rs`, are pairwise distinct and collide with neither a
//!    strategy discriminator nor Anchor's event instruction tag;
//! 3. Anchor's event tag is the little-endian bytes of `sha256("anchor:event")[..8]`;
//! 4. the token-program and associated-token-program tags the router sends are the ones
//!    those programs define (`TransferChecked` = 12, `CreateIdempotent` = 1);
//! 5. `SwapRequest` encodes as discriminator + Borsh fields in the documented order, and
//!    decodes only under its own discriminator.

use pipeline_client::{Pubkey, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID};
use pipeline_strategy_interface::{self as psi, discriminator as d};
use sha2::{Digest, Sha256};

const ROUTER_SOURCE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../programs/pipeline/src/lib.rs"
);
const TOKEN_SOURCE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../programs/pipeline/src/token.rs"
);
const STRATEGY_GUIDE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/STRATEGY-GUIDE.md");

fn anchor_global(name: &str) -> [u8; 8] {
    let digest = Sha256::digest(format!("global:{name}").as_bytes());
    digest[..8].try_into().unwrap()
}

/// Anchor's `EVENT_IX_TAG_LE`: the little-endian bytes of `sha256("anchor:event")[..8]`.
fn event_ix_tag_wire() -> [u8; 8] {
    let digest = Sha256::digest(b"anchor:event");
    let mut tag: [u8; 8] = digest[..8].try_into().unwrap();
    tag.reverse();
    tag
}

/// Names of the router's instructions: the `pub fn` items at module level in the
/// `#[program]` module. Helper functions in the file are not indented, so they are skipped.
fn router_instruction_names() -> Vec<String> {
    let source = std::fs::read_to_string(ROUTER_SOURCE).unwrap();
    source
        .lines()
        .filter_map(|line| line.strip_prefix("    pub fn "))
        .map(|rest| {
            rest.chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect()
        })
        .collect()
}

fn strategy_discriminators() -> Vec<(&'static str, [u8; 8])> {
    vec![
        ("pipeline_init_market", d::INIT_MARKET),
        ("pipeline_market_info", d::MARKET_INFO),
        ("pipeline_resolve_accounts", d::RESOLVE_ACCOUNTS),
        ("pipeline_quote", d::QUOTE),
        ("pipeline_swap", d::SWAP),
        ("pipeline_seed_liquidity", d::SEED_LIQUIDITY),
    ]
}

#[test]
fn h072_strategy_discriminators_are_anchor_global_names() {
    for (name, value) in strategy_discriminators() {
        assert_eq!(value, anchor_global(name), "{name} discriminator");
    }
}

#[test]
fn h072_router_discriminators_are_distinct_and_avoid_strategy_and_event_tags() {
    let names = router_instruction_names();
    assert_eq!(
        names.len(),
        10,
        "router instructions parsed from source: {names:?}"
    );

    let router: Vec<([u8; 8], &str)> = names
        .iter()
        .map(|n| (anchor_global(n), n.as_str()))
        .collect();
    for (i, (a, name_a)) in router.iter().enumerate() {
        for (b, name_b) in &router[i + 1..] {
            assert_ne!(
                a, b,
                "router instructions {name_a} and {name_b} share a discriminator"
            );
        }
    }

    let event = event_ix_tag_wire();
    for (value, name) in &router {
        assert_ne!(
            *value, event,
            "router instruction {name} equals the event tag"
        );
        for (strategy, strategy_value) in strategy_discriminators() {
            assert_ne!(
                *value, strategy_value,
                "router instruction {name} equals strategy instruction {strategy}"
            );
        }
    }
}

#[test]
fn h072_event_tag_is_the_little_endian_anchor_constant() {
    assert_eq!(event_ix_tag_wire(), [228, 69, 165, 46, 81, 203, 154, 29]);
}

#[test]
fn h072_token_tags_match_the_token_programs() {
    let source = std::fs::read_to_string(TOKEN_SOURCE).unwrap();
    assert!(
        source.contains("const TRANSFER_CHECKED_TAG: u8 = 12;"),
        "router must send TransferChecked = 12"
    );
    assert!(
        source.contains("data: vec![1],"),
        "router must send the ATA CreateIdempotent instruction (1)"
    );

    let source_account = Pubkey::new_unique();
    let mint = Pubkey::new_unique();
    let destination = Pubkey::new_unique();
    let authority = Pubkey::new_unique();

    let spl = spl_token_interface::instruction::transfer_checked(
        &TOKEN_PROGRAM_ID,
        &source_account,
        &mint,
        &destination,
        &authority,
        &[],
        1,
        6,
    )
    .unwrap();
    assert_eq!(spl.data[0], 12, "SPL Token TransferChecked tag");
    assert_eq!(spl.data.len(), 10, "SPL Token TransferChecked data length");

    let token_2022 = spl_token_2022_interface::instruction::transfer_checked(
        &TOKEN_2022_PROGRAM_ID,
        &source_account,
        &mint,
        &destination,
        &authority,
        &[],
        1,
        6,
    )
    .unwrap();
    assert_eq!(token_2022.data[0], 12, "Token-2022 TransferChecked tag");
    assert_eq!(
        token_2022.data.len(),
        10,
        "Token-2022 TransferChecked data length"
    );
}

#[test]
fn h072_swap_request_layout_is_documented_order_and_discriminator_gated() {
    let request = psi::SwapRequest {
        trade: psi::TradeRequest {
            mode: psi::TradeMode::ExactOut,
            input_index: 1,
            output_index: 0,
            amount: 0x0102_0304_0506_0708,
            trader: Some([9u8; 32]),
            extra: vec![7, 8, 9],
        },
        amount_sent: 11,
        min_amount_out: 13,
    };
    let data = psi::instruction_data(d::SWAP, &request);

    // Discriminator, then mode, input index, output index, amount, trader option tag and
    // key, extra length and bytes, amount sent, minimum out. Borsh is little-endian.
    let mut expected = d::SWAP.to_vec();
    expected.extend_from_slice(&[1, 1, 0]);
    expected.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
    expected.push(1);
    expected.extend_from_slice(&[9u8; 32]);
    expected.extend_from_slice(&3u32.to_le_bytes());
    expected.extend_from_slice(&[7, 8, 9]);
    expected.extend_from_slice(&11u64.to_le_bytes());
    expected.extend_from_slice(&13u64.to_le_bytes());
    assert_eq!(data, expected);

    let decoded: Option<psi::SwapRequest> = psi::parse_instruction(d::SWAP, &data);
    assert_eq!(decoded, Some(request));
    let as_quote: Option<psi::TradeRequest> = psi::parse_instruction(d::QUOTE, &data);
    assert_eq!(
        as_quote, None,
        "swap data must not decode under the quote discriminator"
    );
}

#[test]
fn h072_strategy_guide_names_every_strategy_instruction() {
    let guide = std::fs::read_to_string(STRATEGY_GUIDE).unwrap();
    for (name, _) in strategy_discriminators() {
        if name == "pipeline_seed_liquidity" {
            continue;
        }
        assert!(
            guide.contains(&format!("`{name}`")),
            "STRATEGY-GUIDE does not name {name}"
        );
    }
    assert!(
        guide.contains("`sha256(\"global:pipeline_seed_liquidity\")[..8]`"),
        "STRATEGY-GUIDE documents the seed discriminator rule"
    );
}
