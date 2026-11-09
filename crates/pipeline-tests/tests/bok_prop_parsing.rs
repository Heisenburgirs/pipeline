//! BOK Proptest suites — router byte-layout parsing (INV-ADM-10, INV-ADM-11 model part).
//!
//! MODEL EVIDENCE. The router's parsers are private functions of an Anchor program, so
//! they are tested here through VERBATIM copies. The only adaptation is
//! `Pubkey`/`AccountInfo` → `[u8; 32]`/byte slices and `require!` → early `Err(())`.
//! Each copy names its source lines (git ref a31a5c5) so a reviewer can diff it. If the
//! router's code changes, these copies must be refreshed, or they prove nothing.
//!
//! Canonical references:
//! - loader-v3: `bincode` 1 encoding of `UpgradeableLoaderState` (u32 variant index,
//!   then the variant's fields). The enum type itself isn't a dependency of this crate,
//!   so it is encoded as the equivalent serde tuples, and cross-checked against
//!   accounts written by the real loader in LiteSVM.
//! - token accounts and mints: `spl-token-2022-interface` `StateWithExtensions::unpack`
//!   (which for 165-byte accounts is the SPL Token layout and validation) and
//!   `spl-token-interface`'s `GenericTokenAccount`.

// The copies stay verbatim, so lints that would reshape them are allowed.
#![allow(clippy::needless_question_mark, clippy::field_reassign_with_default)]

mod common;

use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use proptest::prelude::*;
use solana_keypair::Keypair;
use solana_signer::Signer;
use spl_token_2022_interface::extension::{
    cpi_guard::CpiGuard, immutable_owner::ImmutableOwner, memo_transfer::MemoTransfer,
    mint_close_authority::MintCloseAuthority, non_transferable::NonTransferable,
    permanent_delegate::PermanentDelegate, transfer_fee::TransferFeeAmount,
    transfer_fee::TransferFeeConfig, transfer_hook::TransferHook,
    transfer_hook::TransferHookAccount, BaseStateWithExtensionsMut, ExtensionType,
    StateWithExtensions, StateWithExtensionsMut,
};
use spl_token_2022_interface::state::{Account, AccountState, Mint};
use spl_token_interface::state::GenericTokenAccount;

const CASES: u32 = 10_000;

// ---------------------------------------------------------------------------------------
// Verbatim copies (programs/pipeline/src/lib.rs, programs/pipeline/src/token.rs @ a31a5c5)
// ---------------------------------------------------------------------------------------

/// Copy of `upgradeable_programdata_address` (lib.rs:879-885).
fn upgradeable_programdata_address(data: &[u8]) -> Option<[u8; 32]> {
    if data.len() < 36 || data[..4] != 2u32.to_le_bytes() {
        return None;
    }
    Some(data[4..36].try_into().ok()?)
}

/// Copy of `upgrade_authority` (lib.rs:887-898).
fn upgrade_authority(data: &[u8]) -> Option<Option<[u8; 32]>> {
    if data.len() < 13 || data[..4] != 3u32.to_le_bytes() {
        return None;
    }
    match data[12] {
        0 => Some(None),
        1 if data.len() >= 45 => Some(Some(data[13..45].try_into().ok()?)),
        _ => None,
    }
}

/// Constants from token.rs:66-75.
const ACCOUNT_LEN: usize = 165;
const MULTISIG_LEN: usize = 355;
const ACCOUNT_TYPE_ACCOUNT: u8 = 2;
const MINT_DECIMALS_OFFSET: usize = 44;
const MINT_LEN: usize = 82;

/// Copy of `token::checked_balance` (token.rs:78-105). `owner` is the account's owner
/// program; `Err(())` stands for `BadTokenAccount`.
fn checked_balance(
    owner: &[u8; 32],
    data: &[u8],
    mint: &[u8; 32],
    token_program: &[u8; 32],
) -> Result<u64, ()> {
    if *owner != *token_program {
        return Err(());
    }
    if !(data.len() >= ACCOUNT_LEN && data.len() != MULTISIG_LEN) {
        return Err(());
    }
    if !(data.len() == ACCOUNT_LEN || data[ACCOUNT_LEN] == ACCOUNT_TYPE_ACCOUNT) {
        return Err(());
    }
    if data[..32] != *mint {
        return Err(());
    }
    Ok(u64::from_le_bytes(data[64..72].try_into().unwrap()))
}

/// Copy of `token::balance` (token.rs:107-112).
fn balance(data: &[u8]) -> Result<u64, ()> {
    if data.len() < ACCOUNT_LEN {
        return Err(());
    }
    Ok(u64::from_le_bytes(data[64..72].try_into().unwrap()))
}

/// Copy of `token::decimals` (token.rs:114-118).
fn decimals(data: &[u8]) -> Result<u8, ()> {
    if data.len() < MINT_LEN {
        return Err(());
    }
    Ok(data[MINT_DECIMALS_OFFSET])
}

// ---------------------------------------------------------------------------------------
// Canonical references
// ---------------------------------------------------------------------------------------

/// What the loader-v3 state decodes to, via bincode, for the two variants the router
/// reads. `None` = bincode rejects it or it is another variant.
#[derive(Debug, PartialEq)]
enum LoaderState {
    Program([u8; 32]),
    ProgramData(u64, Option<[u8; 32]>),
    Other,
}

fn bincode_decode(data: &[u8]) -> Option<LoaderState> {
    let tag: u32 = bincode::deserialize(data).ok()?;
    match tag {
        2 => {
            let (_, key): (u32, [u8; 32]) = bincode::deserialize(data).ok()?;
            Some(LoaderState::Program(key))
        }
        3 => {
            let (_, slot, authority): (u32, u64, Option<[u8; 32]>) =
                bincode::deserialize(data).ok()?;
            Some(LoaderState::ProgramData(slot, authority))
        }
        _ => Some(LoaderState::Other),
    }
}

/// The canonical token-account reading: Token-2022's unpack (initialised base state,
/// not multisig length, account type `Account` if longer than 165 bytes).
fn canonical_account(data: &[u8]) -> Option<Account> {
    StateWithExtensions::<Account>::unpack(data)
        .ok()
        .map(|state| state.base)
}

fn canonical_mint(data: &[u8]) -> Option<Mint> {
    StateWithExtensions::<Mint>::unpack(data)
        .ok()
        .map(|state| state.base)
}

// ---------------------------------------------------------------------------------------
// Generators for token-program-shaped data
// ---------------------------------------------------------------------------------------

fn key(bytes: [u8; 32]) -> Pubkey {
    Pubkey::new_from_array(bytes)
}

#[derive(Clone, Debug)]
struct AccountFields {
    mint: [u8; 32],
    owner: [u8; 32],
    amount: u64,
    delegate: Option<([u8; 32], u64)>,
    state: u8,
    is_native: Option<u64>,
    close_authority: Option<[u8; 32]>,
}

prop_compose! {
    fn account_fields()(
        mint in any::<[u8; 32]>(),
        owner in any::<[u8; 32]>(),
        amount in prop_oneof![Just(0u64), Just(1), Just(u64::MAX), any::<u64>()],
        delegate in proptest::option::of((any::<[u8; 32]>(), any::<u64>())),
        state in 1u8..=2,
        is_native in proptest::option::of(any::<u64>()),
        close_authority in proptest::option::of(any::<[u8; 32]>()),
    ) -> AccountFields {
        AccountFields { mint, owner, amount, delegate, state, is_native, close_authority }
    }
}

/// The extension sets a Token-2022 token account can carry, by index.
fn account_extensions(choice: u8) -> Vec<ExtensionType> {
    match choice % 6 {
        0 => vec![],
        1 => vec![ExtensionType::ImmutableOwner],
        2 => vec![
            ExtensionType::ImmutableOwner,
            ExtensionType::TransferFeeAmount,
        ],
        3 => vec![ExtensionType::TransferHookAccount],
        4 => vec![ExtensionType::MemoTransfer, ExtensionType::CpiGuard],
        _ => vec![
            ExtensionType::ImmutableOwner,
            ExtensionType::TransferFeeAmount,
            ExtensionType::TransferHookAccount,
        ],
    }
}

/// Packs a token account with the interface's own writer. `extensions` empty gives the
/// 165-byte SPL Token layout.
fn pack_account(fields: &AccountFields, extensions: &[ExtensionType]) -> Vec<u8> {
    let len = ExtensionType::try_calculate_account_len::<Account>(extensions).unwrap();
    let mut data = vec![0u8; len];
    let mut state = StateWithExtensionsMut::<Account>::unpack_uninitialized(&mut data).unwrap();
    state.base = Account {
        mint: key(fields.mint),
        owner: key(fields.owner),
        amount: fields.amount,
        delegate: fields.delegate.map(|(k, _)| key(k)).into(),
        state: AccountState::try_from(fields.state).unwrap(),
        is_native: fields.is_native.into(),
        delegated_amount: fields.delegate.map(|(_, a)| a).unwrap_or(0),
        close_authority: fields.close_authority.map(key).into(),
    };
    state.pack_base();
    state.init_account_type().unwrap();
    for extension in extensions {
        match extension {
            ExtensionType::ImmutableOwner => {
                state.init_extension::<ImmutableOwner>(true).unwrap();
            }
            ExtensionType::TransferFeeAmount => {
                state.init_extension::<TransferFeeAmount>(true).unwrap();
            }
            ExtensionType::TransferHookAccount => {
                state.init_extension::<TransferHookAccount>(true).unwrap();
            }
            ExtensionType::MemoTransfer => {
                state.init_extension::<MemoTransfer>(true).unwrap();
            }
            ExtensionType::CpiGuard => {
                state.init_extension::<CpiGuard>(true).unwrap();
            }
            other => panic!("unexpected extension {other:?}"),
        }
    }
    data
}

fn mint_extensions(choice: u8) -> Vec<ExtensionType> {
    match choice % 5 {
        0 => vec![],
        1 => vec![ExtensionType::MintCloseAuthority],
        2 => vec![ExtensionType::TransferFeeConfig],
        3 => vec![
            ExtensionType::TransferHook,
            ExtensionType::PermanentDelegate,
        ],
        _ => vec![
            ExtensionType::NonTransferable,
            ExtensionType::TransferFeeConfig,
        ],
    }
}

/// Packs a mint (82 bytes without extensions, account type 1 at byte 165 with them).
fn pack_mint(decimals: u8, supply: u64, extensions: &[ExtensionType]) -> Vec<u8> {
    let len = ExtensionType::try_calculate_account_len::<Mint>(extensions).unwrap();
    let mut data = vec![0u8; len];
    let mut state = StateWithExtensionsMut::<Mint>::unpack_uninitialized(&mut data).unwrap();
    let mut base = Mint::default();
    base.decimals = decimals;
    base.supply = supply;
    base.is_initialized = true;
    state.base = base;
    state.pack_base();
    state.init_account_type().unwrap();
    for extension in extensions {
        match extension {
            ExtensionType::MintCloseAuthority => {
                state.init_extension::<MintCloseAuthority>(true).unwrap();
            }
            ExtensionType::TransferFeeConfig => {
                state.init_extension::<TransferFeeConfig>(true).unwrap();
            }
            ExtensionType::TransferHook => {
                state.init_extension::<TransferHook>(true).unwrap();
            }
            ExtensionType::PermanentDelegate => {
                state.init_extension::<PermanentDelegate>(true).unwrap();
            }
            ExtensionType::NonTransferable => {
                state.init_extension::<NonTransferable>(true).unwrap();
            }
            other => panic!("unexpected extension {other:?}"),
        }
    }
    data
}

/// A 355-byte SPL multisig: m, n, is_initialized, then 11 signer keys.
fn pack_multisig(m: u8, n: u8, signers: &[[u8; 32]; 11]) -> Vec<u8> {
    let mut data = vec![m, n, 1];
    for signer in signers {
        data.extend_from_slice(signer);
    }
    assert_eq!(data.len(), MULTISIG_LEN);
    data
}

/// Token-program-shaped data: what the token programs can actually write into an
/// account they own.
#[derive(Clone, Debug)]
enum Shaped {
    Account(AccountFields, u8),
    /// Allocated to a token-account size but never initialised (all zeros).
    UninitializedAccount(u8),
    Mint(u8, u64, u8),
    Multisig(u8, u8, [[u8; 32]; 11]),
}

fn shaped() -> impl Strategy<Value = Shaped> {
    prop_oneof![
        4 => (account_fields(), any::<u8>()).prop_map(|(f, e)| Shaped::Account(f, e)),
        1 => any::<u8>().prop_map(Shaped::UninitializedAccount),
        2 => (any::<u8>(), any::<u64>(), any::<u8>()).prop_map(|(d, s, e)| Shaped::Mint(d, s, e)),
        1 => (1u8..=11, 1u8..=11, any::<[[u8; 32]; 11]>()).prop_map(|(m, n, s)| Shaped::Multisig(m, n, s)),
    ]
}

fn shaped_bytes(shape: &Shaped) -> Vec<u8> {
    match shape {
        Shaped::Account(fields, e) => pack_account(fields, &account_extensions(*e)),
        Shaped::UninitializedAccount(e) => {
            let len = ExtensionType::try_calculate_account_len::<Account>(&account_extensions(*e))
                .unwrap();
            vec![0u8; len]
        }
        Shaped::Mint(d, s, e) => pack_mint(*d, *s, &mint_extensions(*e)),
        Shaped::Multisig(m, n, signers) => {
            let mut signers = *signers;
            // Byte 165 is inside signer 5; make it look like Token-2022's "account" type.
            signers[5][2] = ACCOUNT_TYPE_ACCOUNT;
            pack_multisig(*m, *n, &signers)
        }
    }
}

const TOKEN: [u8; 32] = client::TOKEN_PROGRAM_ID.to_bytes();
const TOKEN_2022: [u8; 32] = client::TOKEN_2022_PROGRAM_ID.to_bytes();

/// The mint a caller asks about: the account's own first 32 bytes, or something else.
fn probe_mint(data: &[u8], own: bool, other: [u8; 32]) -> [u8; 32] {
    if own && data.len() >= 32 {
        data[..32].try_into().unwrap()
    } else {
        other
    }
}

// ---------------------------------------------------------------------------------------
// INV-ADM-10 — loader-v3 parsing
// ---------------------------------------------------------------------------------------

/// Byte strings biased towards the interesting shapes: right tags, option bytes 0/1/2,
/// and lengths around 13, 36 and 45.
fn loader_bytes() -> impl Strategy<Value = Vec<u8>> {
    (
        prop_oneof![Just(0u32), Just(1), Just(2), Just(3), Just(4), any::<u32>()],
        prop_oneof![Just(0u8), Just(1), Just(2), any::<u8>()],
        proptest::collection::vec(any::<u8>(), 0..80),
    )
        .prop_map(|(tag, option, mut rest)| {
            let mut data = tag.to_le_bytes().to_vec();
            data.append(&mut rest);
            if data.len() > 12 {
                data[12] = option;
            }
            data
        })
        .boxed()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    /// INV-ADM-10 (model): `upgradeable_programdata_address` is exactly the formula, for
    /// any bytes, and never panics.
    #[test]
    fn bok_inv_adm_10_model_programdata_address_formula(
        data in prop_oneof![loader_bytes(), proptest::collection::vec(any::<u8>(), 0..64)],
    ) {
        let expected = (data.len() >= 36 && data[0..4] == 2u32.to_le_bytes())
            .then(|| <[u8; 32]>::try_from(&data[4..36]).unwrap());
        prop_assert_eq!(upgradeable_programdata_address(&data), expected);
    }

    /// INV-ADM-10 (model): `upgrade_authority` is exactly the formula, for any bytes.
    #[test]
    fn bok_inv_adm_10_model_upgrade_authority_formula(
        data in prop_oneof![loader_bytes(), proptest::collection::vec(any::<u8>(), 0..64)],
    ) {
        let tag3 = data.len() >= 13 && data[0..4] == 3u32.to_le_bytes();
        let expected = if tag3 && data[12] == 0 {
            Some(None)
        } else if tag3 && data[12] == 1 && data.len() >= 45 {
            Some(Some(<[u8; 32]>::try_from(&data[13..45]).unwrap()))
        } else {
            None
        };
        prop_assert_eq!(upgrade_authority(&data), expected);
    }

    /// INV-ADM-10 (model, differential): both parsers agree with bincode's decoding of
    /// `UpgradeableLoaderState` on every byte string, and the slot never matters.
    #[test]
    fn bok_inv_adm_10_model_parsers_match_bincode(data in loader_bytes()) {
        let decoded = bincode_decode(&data);
        let program = match decoded {
            Some(LoaderState::Program(key)) => Some(key),
            _ => None,
        };
        let authority = match decoded {
            Some(LoaderState::ProgramData(_, authority)) => Some(authority),
            _ => None,
        };
        prop_assert_eq!(upgradeable_programdata_address(&data), program);
        prop_assert_eq!(upgrade_authority(&data), authority);
        if data.len() >= 12 {
            let mut other_slot = data.clone();
            for byte in &mut other_slot[4..12] {
                *byte = !*byte;
            }
            prop_assert_eq!(upgrade_authority(&other_slot), upgrade_authority(&data));
        }
    }

    /// INV-ADM-10 (model, round trip): bincode-serialised `Program { programdata_address }`
    /// and `ProgramData { slot, upgrade_authority_address }` headers, followed by any
    /// bytes (the ELF), parse back to exactly what was written.
    #[test]
    fn bok_inv_adm_10_model_round_trip(
        programdata in any::<[u8; 32]>(),
        slot in any::<u64>(),
        authority in proptest::option::of(any::<[u8; 32]>()),
        tail in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut program = bincode::serialize(&(2u32, programdata)).unwrap();
        prop_assert_eq!(program.len(), 36);
        program.extend_from_slice(&tail);
        prop_assert_eq!(upgradeable_programdata_address(&program), Some(programdata));
        prop_assert_eq!(upgrade_authority(&program), None);

        let mut header = bincode::serialize(&(3u32, slot, authority)).unwrap();
        prop_assert_eq!(header.len(), if authority.is_some() { 45 } else { 13 });
        // The loader always reserves the full 45-byte header, authority or not.
        header.resize(45, 0);
        header.extend_from_slice(&tail);
        prop_assert_eq!(upgrade_authority(&header), Some(authority));
        prop_assert_eq!(upgradeable_programdata_address(&header), None);
    }
}

/// INV-ADM-10 (real loader): the copies read what LiteSVM's loader-v3 actually wrote,
/// including after real `SetAuthority` instructions (to a new key, then to none).
#[test]
fn bok_inv_adm_10_real_loader_accounts_parse_as_expected() {
    let mut env = Env::new();
    let owner = env.funded_keypair();
    env.deploy_strategy(&CP_PROGRAM_ID, "test_strategy_cp", &owner.pubkey());
    let program_data = pda::program_data(&CP_PROGRAM_ID);

    let program = env.svm.get_account(&CP_PROGRAM_ID).unwrap();
    assert_eq!(program.owner, client::BPF_LOADER_UPGRADEABLE_ID);
    assert_eq!(
        upgradeable_programdata_address(&program.data),
        Some(program_data.to_bytes())
    );
    assert_eq!(upgrade_authority(&program.data), None);

    let read_authority =
        |env: &Env| upgrade_authority(&env.svm.get_account(&program_data).unwrap().data);
    assert_eq!(read_authority(&env), Some(Some(owner.pubkey().to_bytes())));

    // Loader-v3 `SetAuthority` (instruction 4): [programdata (w), current (s), new].
    let set_authority = |current: &Pubkey, new: Option<&Pubkey>| {
        let mut accounts = vec![
            client::AccountMeta::new(program_data, false),
            client::AccountMeta::new_readonly(*current, true),
        ];
        if let Some(new) = new {
            accounts.push(client::AccountMeta::new_readonly(*new, false));
        }
        client::Instruction::new_with_bytes(
            client::BPF_LOADER_UPGRADEABLE_ID,
            &4u32.to_le_bytes(),
            accounts,
        )
    };
    let next = Keypair::new();
    env.send(
        &[set_authority(&owner.pubkey(), Some(&next.pubkey()))],
        &[&owner],
    )
    .expect("loader SetAuthority to a new key");
    assert_eq!(read_authority(&env), Some(Some(next.pubkey().to_bytes())));
    env.send(&[set_authority(&next.pubkey(), None)], &[&next])
        .expect("loader SetAuthority to none");
    assert_eq!(read_authority(&env), Some(None));
}

// ---------------------------------------------------------------------------------------
// INV-ADM-11 — token-account and mint parsing
// ---------------------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    /// INV-ADM-11 (model, differential on token-program-shaped data): `checked_balance`
    /// accepts exactly the accounts canonical unpack accepts for that mint, with the
    /// same amount, apart from the documented gap (an uninitialised 165-byte buffer reads
    /// as amount 0 for the all-zero mint).
    #[test]
    fn bok_inv_adm_11_model_checked_balance_matches_canonical_unpack(
        shape in shaped(),
        own_mint in any::<bool>(),
        other in any::<[u8; 32]>(),
        program_2022 in any::<bool>(),
    ) {
        let data = shaped_bytes(&shape);
        let mint = probe_mint(&data, own_mint, other);
        let tp = if program_2022 { TOKEN_2022 } else { TOKEN };
        let router = checked_balance(&tp, &data, &mint, &tp).ok();
        let canonical = canonical_account(&data)
            .filter(|account| account.mint.to_bytes() == mint)
            .map(|account| account.amount);
        // Longer uninitialised buffers have account type 0 at byte 165, so only the
        // 165-byte one slips through.
        let gap = matches!(shape, Shaped::UninitializedAccount(_))
            && data.len() == ACCOUNT_LEN
            && mint == [0u8; 32];
        if gap {
            prop_assert_eq!(canonical, None);
            prop_assert_eq!(router, Some(0));
        } else {
            prop_assert_eq!(router, canonical, "shape {:?}", shape);
        }
        // Owner check: the same bytes under any other program are rejected.
        prop_assert!(checked_balance(&other, &data, &mint, &tp).is_err() || other == tp);
        // SPL Token's own validity check agrees for the 165-byte layout.
        if data.len() == ACCOUNT_LEN {
            prop_assert_eq!(
                spl_token_interface::state::Account::valid_account_data(&data),
                canonical_account(&data).is_some()
            );
        }
    }

    /// INV-ADM-11 (model): a 355-byte account is rejected whatever its byte 165 or its
    /// first 32 bytes say.
    #[test]
    fn bok_inv_adm_11_model_multisig_length_always_rejected(
        mut data in proptest::collection::vec(any::<u8>(), MULTISIG_LEN..=MULTISIG_LEN),
        byte_165 in prop_oneof![Just(ACCOUNT_TYPE_ACCOUNT), any::<u8>()],
    ) {
        data[ACCOUNT_LEN] = byte_165;
        let mint: [u8; 32] = data[..32].try_into().unwrap();
        prop_assert!(checked_balance(&TOKEN, &data, &mint, &TOKEN).is_err());
        prop_assert!(checked_balance(&TOKEN_2022, &data, &mint, &TOKEN_2022).is_err());
        prop_assert!(canonical_account(&data).is_none());
    }

    /// INV-ADM-11 (model): mints (82 bytes, or Token-2022 mints with extensions, byte
    /// 165 = 1) never pass as token accounts, even when asked about their own address
    /// bytes.
    #[test]
    fn bok_inv_adm_11_model_mints_rejected(
        dec in any::<u8>(),
        supply in any::<u64>(),
        extensions in any::<u8>(),
        own_mint in any::<bool>(),
        other in any::<[u8; 32]>(),
    ) {
        let data = pack_mint(dec, supply, &mint_extensions(extensions));
        let mint = probe_mint(&data, own_mint, other);
        if data.len() > ACCOUNT_LEN {
            prop_assert_eq!(data[ACCOUNT_LEN], 1);
        }
        prop_assert!(checked_balance(&TOKEN_2022, &data, &mint, &TOKEN_2022).is_err());
        prop_assert!(checked_balance(&TOKEN, &data, &mint, &TOKEN).is_err());
    }

    /// INV-ADM-11 (model): `decimals` reads the canonical mint decimals, for SPL and
    /// Token-2022 mints with or without extensions.
    #[test]
    fn bok_inv_adm_11_model_decimals_match_canonical_mint(
        dec in any::<u8>(),
        supply in any::<u64>(),
        extensions in any::<u8>(),
    ) {
        let data = pack_mint(dec, supply, &mint_extensions(extensions));
        let mint = canonical_mint(&data).expect("canonical mint");
        prop_assert_eq!(mint.decimals, dec);
        prop_assert_eq!(decimals(&data), Ok(mint.decimals));
    }

    /// INV-ADM-11 (model): on arbitrary bytes of any length the three readers never
    /// panic, `checked_balance` is exactly the documented formula, and anything canonical
    /// unpack accepts for the mint is accepted with the same amount (completeness).
    #[test]
    fn bok_inv_adm_11_model_arbitrary_bytes(
        mut data in proptest::collection::vec(any::<u8>(), 0..400),
        own_mint in any::<bool>(),
        other in any::<[u8; 32]>(),
        force_type in any::<bool>(),
    ) {
        if force_type && data.len() > ACCOUNT_LEN {
            data[ACCOUNT_LEN] = ACCOUNT_TYPE_ACCOUNT;
        }
        let mint = probe_mint(&data, own_mint, other);
        let router = checked_balance(&TOKEN_2022, &data, &mint, &TOKEN_2022).ok();
        let formula = (data.len() >= ACCOUNT_LEN
            && data.len() != MULTISIG_LEN
            && (data.len() == ACCOUNT_LEN || data[ACCOUNT_LEN] == ACCOUNT_TYPE_ACCOUNT)
            && data[..32] == mint)
            .then(|| u64::from_le_bytes(data[64..72].try_into().unwrap()));
        prop_assert_eq!(router, formula);
        if let Some(account) = canonical_account(&data) {
            if account.mint.to_bytes() == mint {
                prop_assert_eq!(router, Some(account.amount));
            }
            prop_assert_eq!(balance(&data), Ok(account.amount));
        }
        let _ = balance(&data);
        let _ = decimals(&data);
        if let Some(m) = canonical_mint(&data) {
            prop_assert_eq!(decimals(&data), Ok(m.decimals));
        }
    }
}

/// INV-ADM-11 (model, characterisation of a known gap): the router does not validate the
/// `COption` tags or the state byte, so a 165-byte buffer with an invalid delegate tag
/// passes `checked_balance` although canonical unpack rejects it. Token-program-owned
/// accounts can't hold such bytes (the owner check comes first and only the token
/// program writes its accounts), so this is unreachable on chain. It is pinned here so a
/// change in either direction is noticed.
#[test]
fn bok_inv_adm_11_model_gap_invalid_option_tag_is_unreachable_only() {
    let fields = AccountFields {
        mint: [7; 32],
        owner: [8; 32],
        amount: 42,
        delegate: None,
        state: 1,
        is_native: None,
        close_authority: None,
    };
    let mut data = pack_account(&fields, &[]);
    assert_eq!(checked_balance(&TOKEN, &data, &[7; 32], &TOKEN), Ok(42));
    data[72] = 7; // delegate COption tag: only 0 or 1 is valid
    assert!(canonical_account(&data).is_none());
    assert_eq!(checked_balance(&TOKEN, &data, &[7; 32], &TOKEN), Ok(42));
}
