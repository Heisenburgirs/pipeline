//! BOK Proptest suites — signer forwarding and market registry (INV-ADM-5, -6, -7).
//!
//! MODEL EVIDENCE. `is_fresh`, `meta_like`, the `keeps_signature` closure, the shape
//! checks of `fetch_market_info`, `record_market` and `mint_index` are private functions
//! of the Anchor router. They are tested here through VERBATIM copies; the only
//! adaptation is `AccountInfo`/`Pubkey`/`Account<Market>` → plain structs and
//! `[u8; 32]`, and `require!` → early `Err(())`. Each copy names its source lines (git ref
//! a31a5c5) so a reviewer can diff it. The real instructions are exercised by LiteSVM in
//! `bok_markets.rs`.

use std::collections::HashSet;

use borsh::BorshDeserialize;
use pipeline_client::{self as client, AccountMeta, Pubkey};
use pipeline_strategy_interface as psi;
use proptest::prelude::*;

const CASES: u32 = 10_000;
const SYSTEM: [u8; 32] = client::SYSTEM_PROGRAM_ID.to_bytes();

// ---------------------------------------------------------------------------------------
// Verbatim copies (programs/pipeline/src/lib.rs @ a31a5c5)
// ---------------------------------------------------------------------------------------

/// The parts of an `AccountInfo` the router's signer logic reads.
#[derive(Clone, Debug)]
struct Info {
    key: [u8; 32],
    is_signer: bool,
    is_writable: bool,
    lamports: u64,
    data_len: usize,
    owner: [u8; 32],
}

/// Copy of `is_fresh` (lib.rs:825-827).
fn is_fresh(account: &Info) -> bool {
    account.lamports == 0 && account.data_len == 0 && account.owner == SYSTEM
}

/// Copy of `meta_like` (lib.rs:830-837).
fn meta_like(account: &Info, keep_signer: bool) -> AccountMeta {
    let signer = keep_signer && account.is_signer;
    if account.is_writable {
        AccountMeta::new(Pubkey::new_from_array(account.key), signer)
    } else {
        AccountMeta::new_readonly(Pubkey::new_from_array(account.key), signer)
    }
}

/// Copy of the `keeps_signature` closure in `create_market` (lib.rs:231-232).
fn keeps_signature(creator: &[u8; 32], account: &Info) -> bool {
    account.key != *creator && is_fresh(account)
}

/// Copy of the shape checks in `fetch_market_info` (lib.rs:763-783), from the return
/// data onwards. `return_data` is what `get_return_data()` gave.
fn validate_market_info(
    return_data: Option<([u8; 32], Vec<u8>)>,
    strategy_program: &[u8; 32],
) -> Result<psi::MarketInfo, ()> {
    let (program, data) = return_data.ok_or(())?;
    if program != *strategy_program {
        return Err(());
    }
    let info = psi::MarketInfo::try_from_slice(&data).map_err(|_| ())?;

    let count = info.mints.len();
    if !((psi::MIN_MARKET_MINTS..=psi::MAX_MARKET_MINTS).contains(&count)
        && info.vaults.len() == count)
    {
        return Err(());
    }
    for (i, mint) in info.mints.iter().enumerate() {
        if info.mints[..i].contains(mint) {
            return Err(());
        }
    }
    Ok(info)
}

/// The fields of `Market` that `record_market` and `mint_index` touch.
#[derive(Clone, Debug, Default, PartialEq)]
struct Market {
    mint_count: u8,
    mints: [[u8; 32]; psi::MAX_MARKET_MINTS],
    vaults: [[u8; 32]; psi::MAX_MARKET_MINTS],
    supports_exact_out: bool,
}

/// Copy of the slot-filling part of `record_market` (lib.rs:796-801) and the
/// `market_count` increment (lib.rs:808-811).
fn record_market(
    market: &mut Market,
    market_count: &mut u64,
    info: psi::MarketInfo,
) -> Result<(), ()> {
    market.mint_count = info.mints.len() as u8;
    for (i, (mint, vault)) in info.mints.iter().zip(&info.vaults).enumerate() {
        market.mints[i] = *mint;
        market.vaults[i] = *vault;
    }
    market.supports_exact_out = info.supports_exact_out;
    *market_count = market_count.checked_add(1).ok_or(())?;
    Ok(())
}

/// Copy of `mint_index` (lib.rs:815-820).
fn mint_index(market: &Market, mint: &[u8; 32]) -> Result<usize, ()> {
    market.mints[..market.mint_count as usize]
        .iter()
        .position(|m| m == mint)
        .ok_or(())
}

// ---------------------------------------------------------------------------------------
// INV-ADM-5 — signer forwarding truth table
// ---------------------------------------------------------------------------------------

fn info() -> impl Strategy<Value = Info> {
    (
        any::<[u8; 32]>(),
        any::<bool>(),
        any::<bool>(),
        prop_oneof![Just(0u64), Just(1), any::<u64>()],
        prop_oneof![Just(0usize), Just(1), 0usize..10_240],
        prop_oneof![Just(SYSTEM), any::<[u8; 32]>()],
    )
        .prop_map(
            |(key, is_signer, is_writable, lamports, data_len, owner)| Info {
                key,
                is_signer,
                is_writable,
                lamports,
                data_len,
                owner,
            },
        )
}

/// INV-ADM-5 (model): exhaustive over the six boolean conditions the forwarding depends
/// on (signer, writable, is the creator, has lamports, has data, system-owned).
#[test]
fn bok_inv_adm_5_model_truth_table_exhaustive() {
    let creator = [9u8; 32];
    for bits in 0u8..64 {
        let bit = |i: u8| bits & (1 << i) != 0;
        let account = Info {
            key: if bit(2) { creator } else { [3u8; 32] },
            is_signer: bit(0),
            is_writable: bit(1),
            lamports: if bit(3) { 1 } else { 0 },
            data_len: if bit(4) { 1 } else { 0 },
            owner: if bit(5) { SYSTEM } else { [4u8; 32] },
        };
        let expected_signer = bit(0) && !bit(2) && !bit(3) && !bit(4) && bit(5);
        let create = meta_like(&account, keeps_signature(&creator, &account));
        assert_eq!(create.is_signer, expected_signer, "bits {bits:06b}");
        assert_eq!(create.is_writable, account.is_writable, "bits {bits:06b}");
        assert_eq!(create.pubkey.to_bytes(), account.key);
        // Swaps forward every remaining account with `keep_signer = false`.
        let swap = meta_like(&account, false);
        assert!(!swap.is_signer, "bits {bits:06b}");
        assert_eq!(swap.is_writable, account.is_writable);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    /// INV-ADM-5 (model): with arbitrary lamports and data lengths, a forwarded signer is
    /// exactly a signing, fresh, non-creator account; writability is copied; swaps never
    /// forward a signature.
    #[test]
    fn bok_inv_adm_5_model_signer_out_formula(
        account in info(),
        creator in prop_oneof![any::<[u8; 32]>(), Just([0u8; 32])],
        account_is_creator in any::<bool>(),
    ) {
        let mut account = account;
        if account_is_creator {
            account.key = creator;
        }
        let expected = account.is_signer
            && account.key != creator
            && account.lamports == 0
            && account.data_len == 0
            && account.owner == SYSTEM;
        let meta = meta_like(&account, keeps_signature(&creator, &account));
        prop_assert_eq!(meta.is_signer, expected);
        prop_assert_eq!(meta.is_writable, account.is_writable);
        prop_assert!(!meta.is_signer || account.is_signer, "never escalates");
        prop_assert!(!meta_like(&account, false).is_signer);
    }
}

// ---------------------------------------------------------------------------------------
// INV-ADM-6 — market info shape
// ---------------------------------------------------------------------------------------

/// Independent reference validator (set-based) for the accepted shape.
fn reference_ok(info: &psi::MarketInfo) -> bool {
    let n = info.mints.len();
    let distinct = info.mints.iter().collect::<HashSet<_>>().len() == n;
    (2..=4).contains(&n) && info.vaults.len() == n && distinct
}

/// Keys from a small alphabet so duplicate mints are common.
fn small_key() -> impl Strategy<Value = [u8; 32]> {
    prop_oneof![
        3 => (0u8..6).prop_map(|b| [b; 32]),
        1 => any::<[u8; 32]>(),
    ]
}

prop_compose! {
    fn market_info()(
        mints in proptest::collection::vec(small_key(), 0..7),
        extra_vaults in 0usize..3,
        fewer_vaults in 0usize..3,
        vault_seed in any::<[u8; 32]>(),
        supports_exact_out in any::<bool>(),
        exact_vaults in any::<bool>(),
    ) -> psi::MarketInfo {
        let n = if exact_vaults {
            mints.len()
        } else {
            (mints.len() + extra_vaults).saturating_sub(fewer_vaults)
        };
        let vaults = (0..n)
            .map(|i| {
                let mut v = vault_seed;
                v[0] = i as u8;
                v
            })
            .collect();
        psi::MarketInfo { mints, vaults, supports_exact_out }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    /// INV-ADM-6 (model): structured return data. Accepted ⇔ it came from the strategy,
    /// is exact Borsh (no trailing bytes), has 2–4 distinct mints and one vault each.
    #[test]
    fn bok_inv_adm_6_model_shape_acceptance_is_exact(
        info in market_info(),
        trailing in proptest::collection::vec(any::<u8>(), 0..3),
        from_strategy in prop_oneof![3 => Just(true), 1 => Just(false)],
        strategy in any::<[u8; 32]>(),
        other_program in any::<[u8; 32]>(),
        no_return_data in prop_oneof![9 => Just(false), 1 => Just(true)],
    ) {
        let mut data = borsh::to_vec(&info).unwrap();
        data.extend_from_slice(&trailing);
        let program = if from_strategy { strategy } else { other_program };
        let return_data = (!no_return_data).then(|| (program, data.clone()));
        let result = validate_market_info(return_data, &strategy);
        let expected = !no_return_data
            && program == strategy
            && trailing.is_empty()
            && reference_ok(&info);
        prop_assert_eq!(result.is_ok(), expected, "info {:?}", info);
        if let Ok(accepted) = result {
            prop_assert_eq!(&accepted, &info);
            prop_assert_eq!(&borsh::to_vec(&accepted).unwrap(), &data);
        }
    }

    /// INV-ADM-6 (model): arbitrary bytes as return data never panic, and anything
    /// accepted is exact Borsh of a well-shaped `MarketInfo`.
    #[test]
    fn bok_inv_adm_6_model_arbitrary_return_data_never_panics(
        data in prop_oneof![
            proptest::collection::vec(any::<u8>(), 0..400),
            // A plausible prefix (mint count 0..=6) followed by noise.
            (0u32..7, proptest::collection::vec(any::<u8>(), 0..400)).prop_map(|(n, rest)| {
                let mut d = n.to_le_bytes().to_vec();
                d.extend(rest);
                d
            }),
        ],
        strategy in any::<[u8; 32]>(),
    ) {
        if let Ok(info) = validate_market_info(Some((strategy, data.clone())), &strategy) {
            prop_assert!(reference_ok(&info));
            prop_assert_eq!(&borsh::to_vec(&info).unwrap(), &data);
        }
        // Decoding stored router accounts from arbitrary bytes never panics either.
        let _ = client::decode_market(&data);
        let _ = client::decode_strategy(&data);
        let _ = client::decode_config(&data);
    }
}

// ---------------------------------------------------------------------------------------
// INV-ADM-7 — recorded market and mint lookup
// ---------------------------------------------------------------------------------------

prop_compose! {
    /// Market infos that `fetch_market_info` accepts: 2–4 distinct mints (from a small
    /// alphabet, so probes often hit them), one vault each.
    fn accepted_market_info()(
        mints in proptest::collection::hash_set(small_key(), 2..=4),
        vault_seed in any::<[u8; 32]>(),
        supports_exact_out in any::<bool>(),
    ) -> psi::MarketInfo {
        let mints: Vec<[u8; 32]> = mints.into_iter().collect();
        let vaults = (0..mints.len())
            .map(|i| {
                let mut v = vault_seed;
                v[0] = i as u8;
                v
            })
            .collect();
        psi::MarketInfo { mints, vaults, supports_exact_out }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    /// INV-ADM-7 (model): recording an accepted market info stores exactly its mints and
    /// vaults, leaves the other slots default, and `mint_index` then finds a mint iff it
    /// is one of the recorded mints, at its unique position.
    #[test]
    fn bok_inv_adm_7_model_record_then_lookup(
        info in accepted_market_info(),
        probes in proptest::collection::vec(small_key(), 1..8),
        market_count in prop_oneof![Just(0u64), any::<u64>(), Just(u64::MAX)],
    ) {
        prop_assert!(reference_ok(&info));
        let data = borsh::to_vec(&info).unwrap();
        let strategy = [1u8; 32];
        let accepted = validate_market_info(Some((strategy, data)), &strategy).unwrap();

        let mut market = Market::default();
        let mut count = market_count;
        let recorded = record_market(&mut market, &mut count, accepted);
        if market_count == u64::MAX {
            prop_assert!(recorded.is_err(), "market_count must not wrap");
            return Ok(());
        }
        prop_assert!(recorded.is_ok());
        prop_assert_eq!(count, market_count + 1);

        let n = info.mints.len();
        prop_assert_eq!(market.mint_count as usize, n);
        prop_assert!((2..=4).contains(&n));
        prop_assert_eq!(&market.mints[..n], &info.mints[..]);
        prop_assert_eq!(&market.vaults[..n], &info.vaults[..]);
        for k in n..psi::MAX_MARKET_MINTS {
            prop_assert_eq!(market.mints[k], [0u8; 32]);
            prop_assert_eq!(market.vaults[k], [0u8; 32]);
        }
        prop_assert_eq!(market.supports_exact_out, info.supports_exact_out);

        let mut all_probes = probes;
        all_probes.extend(info.mints.iter().copied());
        all_probes.push([0u8; 32]);
        for probe in all_probes {
            let found = mint_index(&market, &probe).ok();
            let positions: Vec<usize> =
                (0..n).filter(|&i| info.mints[i] == probe).collect();
            prop_assert!(positions.len() <= 1, "mints are distinct");
            prop_assert_eq!(found, positions.first().copied());
            if let Some(i) = found {
                prop_assert_eq!(market.vaults[i], info.vaults[i]);
            }
        }
    }
}
