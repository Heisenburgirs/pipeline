//! The Book of Knowledge model tests (`bok_prop_parsing.rs`, `bok_prop_markets.rs`) check
//! copies of private router functions. This test fails if the router's source no longer
//! contains the lines those copies were taken from, so the copies can't silently drift:
//! when it fails, refresh the copies from the router and update the lines below.

/// Collapses whitespace so formatting changes don't matter.
fn normalise(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_router_still_contains_the_copied_code() {
    let router = normalise(include_str!("../../../programs/pipeline/src/lib.rs"));
    let token = normalise(include_str!("../../../programs/pipeline/src/token.rs"));
    let router_lines = [
        // upgradeable_programdata_address
        "if data.len() < 36 || data[..4] != 2u32.to_le_bytes() {",
        "Some(Pubkey::new_from_array(data[4..36].try_into().ok()?))",
        // upgrade_authority
        "if data.len() < 13 || data[..4] != 3u32.to_le_bytes() {",
        "match data[12] {",
        "0 => Some(None),",
        // is_fresh, meta_like, and create_market's signer rule
        "account.lamports() == 0 && account.data_is_empty() && *account.owner == system_program::ID",
        "let signer = keep_signer && account.is_signer;",
        "*account.key != creator && is_fresh(account)",
        // fetch_market_info's shape checks, record_market, mint_index
        "(psi::MIN_MARKET_MINTS..=psi::MAX_MARKET_MINTS).contains(&count)",
        "!info.mints[..i].contains(mint)",
        "market.mints[i] = Pubkey::new_from_array(*mint);",
        "market.vaults[i] = Pubkey::new_from_array(*vault);",
        ".position(|m| m == mint)",
    ];
    let token_lines = [
        // checked_balance
        "data.len() >= ACCOUNT_LEN && data.len() != MULTISIG_LEN",
        "data.len() == ACCOUNT_LEN || data[ACCOUNT_LEN] == ACCOUNT_TYPE_ACCOUNT",
        "data[..32] == mint.to_bytes()",
        // decimals
        "require!(data.len() >= MINT_LEN, PipelineError::WrongTokenProgram);",
        // the constants the copies use, with their values
        "const ACCOUNT_LEN: usize = 165;",
        "const MULTISIG_LEN: usize = 355;",
        "const ACCOUNT_TYPE_ACCOUNT: u8 = 2;",
        "const MINT_DECIMALS_OFFSET: usize = 44;",
        "const MINT_LEN: usize = 82;",
    ];
    for line in router_lines {
        assert!(
            router.contains(&normalise(line)),
            "router no longer has: {line}"
        );
    }
    for line in token_lines {
        assert!(
            token.contains(&normalise(line)),
            "token.rs no longer has: {line}"
        );
    }
}
