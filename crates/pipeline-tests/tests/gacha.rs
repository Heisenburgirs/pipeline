//! The gacha bonding-curve strategy through the router: curve pricing, the four-way
//! fee split, packs of draws, their opening against ORAO VRF randomness (simulated here
//! by writing fulfilled randomness accounts), and fee claims.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_quoter::{MarketView, QuoteRequest, Quoter};
use pipeline_strategy_interface::{fees, TradeMode};
use sha2::{Digest, Sha256};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const GACHA_PROGRAM_ID: Pubkey = client::pubkey!("2TmcmquxnCB8AHoLmLJFYxZnVQdm9pREMEsmHqByQq7G");
const ORAO_VRF: Pubkey = client::pubkey!("VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y");
/// sha256("account:RandomnessV2")[..8].
const RANDOMNESS_V2_DISCRIMINATOR: [u8; 8] = [139, 239, 184, 215, 227, 86, 191, 226];
/// Draws a pack holds.
const PACK_DRAWS: usize = 16;

/// One billion tokens with 6 decimals.
const SUPPLY: u64 = 1_000_000_000_000_000;
/// 30 "SOL" of virtual quote reserve (the quote mint has 9 decimals).
const VIRTUAL_QUOTE: u64 = 30_000_000_000;
/// 280 million tokens of virtual token reserve.
const VIRTUAL_TOKEN: u64 = 280_000_000_000_000;
/// 1 in 100 wins 20% of the base output, capped at 10% of the pot: an expected 0.2%,
/// under the 0.3% every trade pays into the pot.
const WIN_ONE_IN: u32 = 100;
const BONUS_BPS: u16 = 2_000;
const MAX_WIN_POT_BPS: u16 = 1_000;
/// Draws opened within this many slots of their buy are capped by the pot as it is.
const FRESH_WINDOW: u64 = 400;
/// Every trade pays 0.5%: 0.3% to the pot, 0.05% left in the curve, 0.05% to the
/// market's creator and 0.1% to the program's upgrade authority (compiled in).
const POT_FEE_BPS: u16 = 30;
const CURVE_FEE_BPS: u16 = 5;
const CREATOR_FEE_BPS: u16 = 5;
const PROTOCOL_FEE_BPS: u16 = 10;
/// 1 in 100,000 hits the jackpot: 100x the base output, capped at half the pot. An
/// expected 0.1%, so both tiers together cost the 0.3% the pot takes in.
const JACKPOT_ONE_IN: u32 = 100_000;
const JACKPOT_BPS: u32 = 1_000_000;
const MAX_JACKPOT_POT_BPS: u16 = 5_000;

/// Mirrors the strategy's `InitParams`.
#[derive(borsh::BorshSerialize)]
struct InitParams {
    supply: u64,
    virtual_quote: u64,
    virtual_token: u64,
    win_one_in: u32,
    bonus_bps: u16,
    max_win_pot_bps: u16,
    fresh_window_slots: u64,
    pot_fee_bps: u16,
    curve_fee_bps: u16,
    creator_fee_bps: u16,
    creator: [u8; 32],
    jackpot_one_in: u32,
    jackpot_bps: u32,
    max_jackpot_pot_bps: u16,
}

fn params(creator: &Pubkey) -> InitParams {
    InitParams {
        supply: SUPPLY,
        virtual_quote: VIRTUAL_QUOTE,
        virtual_token: VIRTUAL_TOKEN,
        win_one_in: WIN_ONE_IN,
        bonus_bps: BONUS_BPS,
        max_win_pot_bps: MAX_WIN_POT_BPS,
        fresh_window_slots: FRESH_WINDOW,
        pot_fee_bps: POT_FEE_BPS,
        curve_fee_bps: CURVE_FEE_BPS,
        creator_fee_bps: CREATOR_FEE_BPS,
        creator: creator.to_bytes(),
        jackpot_one_in: JACKPOT_ONE_IN,
        jackpot_bps: JACKPOT_BPS,
        max_jackpot_pot_bps: MAX_JACKPOT_POT_BPS,
    }
}

struct GachaMarket {
    market: Pubkey,
    curve: Pubkey,
    /// Claims the creator fee.
    creator: Keypair,
    /// The program's upgrade authority, who claims the protocol fee.
    owner: Keypair,
    quote_mint: Pubkey,
    token_mint: Pubkey,
    quote_vault: Pubkey,
    token_vault: Pubkey,
    pot_vault: Pubkey,
}

fn curve_pda(token_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"curve", token_mint.as_ref()], &GACHA_PROGRAM_ID).0
}

fn curve_child(seed: &[u8], curve: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[seed, curve.as_ref()], &GACHA_PROGRAM_ID).0
}

fn pack_pda(curve: &Pubkey, trader: &Pubkey, index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"pack",
            curve.as_ref(),
            trader.as_ref(),
            &index.to_le_bytes(),
        ],
        &GACHA_PROGRAM_ID,
    )
    .0
}

/// Registers the program once, returning its upgrade authority; later markets in the
/// same environment get a fresh authority installed instead.
fn register(env: &mut Env) -> Keypair {
    if env
        .svm
        .get_account(&pda::strategy(&GACHA_PROGRAM_ID))
        .is_none()
    {
        return env.register(&GACHA_PROGRAM_ID, "gacha_curve");
    }
    let owner = env.funded_keypair();
    set_upgrade_authority(&mut env.svm, &GACHA_PROGRAM_ID, Some(&owner.pubkey()));
    owner
}

/// Registers the strategy and creates a market whose quote token is a fresh 9-decimal
/// SPL mint. The token mint is a new keypair signing `create_market`.
fn setup(env: &mut Env) -> GachaMarket {
    let owner = register(env);
    let creator = env.funded_keypair();
    let quote_mint = env.create_mint(&TOKEN, 9, None, None);
    let token_mint = Keypair::new();
    let curve = curve_pda(&token_mint.pubkey());
    let quote_vault = curve_child(b"quote_vault", &curve);
    let token_vault = curve_child(b"token_vault", &curve);
    let pot_vault = curve_child(b"pot", &curve);

    let ix = client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
        strategy_program: GACHA_PROGRAM_ID,
        market_state: curve,
        market_state_signs: false,
        init_data: borsh::to_vec(&params(&creator.pubkey())).unwrap(),
        max_creator_lamports: MAX_SETUP_LAMPORTS,
        strategy_accounts: vec![
            AccountMeta::new_readonly(quote_mint, false),
            AccountMeta::new(token_mint.pubkey(), true),
            AccountMeta::new(quote_vault, false),
            AccountMeta::new(token_vault, false),
            AccountMeta::new(pot_vault, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(TOKEN, false),
        ],
    });
    env.send(&[ix], &[&creator, &token_mint])
        .expect("create_market");

    let market = pda::market(&GACHA_PROGRAM_ID, &curve);
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &market, &quote_mint, &TOKEN),
            client::init_fee_account(&payer, &market, &token_mint.pubkey(), &TOKEN),
        ],
        &[],
    )
    .expect("init fee accounts");

    GachaMarket {
        market,
        curve,
        creator,
        owner,
        quote_mint,
        token_mint: token_mint.pubkey(),
        quote_vault,
        token_vault,
        pot_vault,
    }
}

/// `(quote_reserve, token_reserve)` recorded in the curve account.
fn reserves(env: &Env, m: &GachaMarket) -> (u64, u64) {
    let data = env.account_data(&m.curve);
    let at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
    (at(8 + 160), at(8 + 168))
}

/// Unclaimed fees: `(creator quote, creator token, protocol quote, protocol token)`.
fn owed(env: &Env, m: &GachaMarket) -> (u64, u64, u64, u64) {
    let data = env.account_data(&m.curve);
    let at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
    // ... fresh_window_slots ends at 216, three u16 fees and the creator key follow.
    (at(254), at(262), at(270), at(278))
}

/// How many packs the curve has opened: the next one gets this plus one as nonce.
fn curve_pack_nonce(env: &Env, m: &GachaMarket) -> u64 {
    let data = env.account_data(&m.curve);
    u64::from_le_bytes(data[296..304].try_into().unwrap())
}

/// `(index, nonce, draws, opened)` of a pack, or `None` if the account is closed.
fn pack(env: &Env, address: &Pubkey) -> Option<(u16, u64, u8, u8)> {
    let data = env.account_data(address);
    if data.len() < 84 {
        return None;
    }
    Some((
        u16::from_le_bytes(data[72..74].try_into().unwrap()),
        u64::from_le_bytes(data[74..82].try_into().unwrap()),
        data[82],
        data[83],
    ))
}

/// `(state, stake, pot_at_buy, slot, seed)` of draw `i` of a pack.
fn draw(env: &Env, address: &Pubkey, i: u8) -> (u8, u64, u64, u64, [u8; 32]) {
    let data = env.account_data(address);
    let base = 84 + 57 * i as usize;
    let at = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
    (
        data[base],
        at(base + 1),
        at(base + 9),
        at(base + 17),
        data[base + 25..base + 57].try_into().unwrap(),
    )
}

const DRAW_ARMED: u8 = 1;
const DRAW_OPENED: u8 = 2;

/// The strategy's seed for draw `index` of a pack opened with `nonce`, mirrored.
fn draw_seed(pack: &Pubkey, nonce: u64, index: u8) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"gacha-vrf")
        .chain_update(pack.as_ref())
        .chain_update(nonce.to_le_bytes())
        .chain_update([index])
        .finalize()
        .into()
}

/// ORAO's randomness request account for a seed.
fn randomness_pda(seed: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[b"orao-vrf-randomness-request", seed], &ORAO_VRF).0
}

/// The seed a buy by `trader` into pack `index` will arm: the pack's next draw, or draw
/// 0 of a pack about to be opened with the curve's next nonce.
fn next_seed(env: &Env, m: &GachaMarket, trader: &Pubkey, index: u16) -> [u8; 32] {
    let pack_key = pack_pda(&m.curve, trader, index);
    let (nonce, draws) = match pack(env, &pack_key) {
        Some((_, nonce, draws, _)) => (nonce, draws),
        None => (curve_pack_nonce(env, m) + 1, 0),
    };
    draw_seed(&pack_key, nonce, draws)
}

/// The randomness account a buy into pack `index` must pass (see `next_seed`).
fn next_randomness(env: &Env, m: &GachaMarket, trader: &Pubkey, index: u16) -> Pubkey {
    randomness_pda(&next_seed(env, m, trader, index))
}

/// Writes an ORAO `RandomnessV2` account for `seed`, pending or fulfilled with
/// `randomness`, as the oracles would.
fn write_randomness(env: &mut Env, seed: &[u8; 32], randomness: Option<&[u8; 64]>) {
    let mut data = RANDOMNESS_V2_DISCRIMINATOR.to_vec();
    match randomness {
        None => {
            data.push(0);
            data.extend_from_slice(&[0u8; 32]);
            data.extend_from_slice(seed);
            data.extend_from_slice(&0u32.to_le_bytes());
        }
        Some(randomness) => {
            data.push(1);
            data.extend_from_slice(&[0u8; 32]);
            data.extend_from_slice(seed);
            data.extend_from_slice(randomness);
        }
    }
    env.svm
        .set_account(
            randomness_pda(seed),
            Account {
                lamports: 10_000_000,
                data,
                owner: ORAO_VRF,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
}

/// Fulfils draw `i` of a pack with randomness of the wanted tier.
fn fulfil(env: &mut Env, pack_key: &Pubkey, i: u8, want: u8) {
    let (_, _, _, _, seed) = draw(env, pack_key, i);
    write_randomness(env, &seed, Some(&randomness_that(want)));
}

/// The curve's pricing: `floor(reserve_out × in / (reserve_in + in))`.
fn curve_out(reserve_in: u128, reserve_out: u128, amount_in: u64) -> u64 {
    (reserve_out * amount_in as u128 / (reserve_in + amount_in as u128)) as u64
}

/// The four fees on an input of `amount`: `(pot, curve, creator, protocol)`.
fn fees_on(amount: u64) -> (u64, u64, u64, u64) {
    let bps = |rate: u16| fees::ceil_bps(amount, rate as u64).unwrap();
    (
        bps(POT_FEE_BPS),
        bps(CURVE_FEE_BPS),
        bps(CREATOR_FEE_BPS),
        bps(PROTOCOL_FEE_BPS),
    )
}

/// What is left of an input after all four fees.
fn net_in(amount: u64) -> u64 {
    let (pot, curve, creator, protocol) = fees_on(amount);
    amount - pot - curve - creator - protocol
}

/// The creator's plus the protocol's fee: what leaves the reserves for claiming.
fn claimable(amount: u64) -> u64 {
    let (_, _, creator, protocol) = fees_on(amount);
    creator + protocol
}

/// What a buy of `amount` gives the trader, and what its pot fee buys for the pot. A
/// buy into a pack is priced exactly like a plain one.
fn price_buy(env: &Env, m: &GachaMarket, amount: u64) -> (u64, u64) {
    let (quote_reserve, token_reserve) = reserves(env, m);
    let x = VIRTUAL_QUOTE as u128 + quote_reserve as u128;
    let y = VIRTUAL_TOKEN as u128 + token_reserve as u128;
    let net = net_in(amount);
    let base = curve_out(x, y, net).min(token_reserve);
    let pot =
        curve_out(x + net as u128, y - base as u128, fees_on(amount).0).min(token_reserve - base);
    (base, pot)
}

/// What a sell of `amount` pays, and the pot fee in tokens. The curve fee in tokens
/// stays in the reserve; the creator's and protocol's accrue in the vault.
fn price_sell(env: &Env, m: &GachaMarket, amount: u64) -> (u64, u64) {
    let (quote_reserve, token_reserve) = reserves(env, m);
    let x = VIRTUAL_QUOTE as u128 + quote_reserve as u128;
    let y = VIRTUAL_TOKEN as u128 + token_reserve as u128;
    (
        curve_out(y, x, net_in(amount)).min(quote_reserve),
        fees_on(amount).0,
    )
}

/// Swap accounts; `pack` is `(index, randomness account of its next draw)` for a buy
/// into a pack (see `next_randomness`), `None` for a plain trade.
fn swap_accounts(
    m: &GachaMarket,
    trader: &Pubkey,
    buy: bool,
    pack: Option<(u16, Pubkey)>,
    trader_quote: &Pubkey,
    trader_token: &Pubkey,
) -> client::SwapAccounts {
    let mut strategy_accounts = vec![
        AccountMeta::new(if buy { m.token_vault } else { m.quote_vault }, false),
        AccountMeta::new(m.pot_vault, false),
    ];
    if let Some((index, randomness)) = pack {
        strategy_accounts.push(AccountMeta::new(pack_pda(&m.curve, trader, index), false));
        strategy_accounts.push(AccountMeta::new_readonly(randomness, false));
    }
    let (input_mint, output_mint, trader_input, trader_output, input_vault) = if buy {
        (
            m.quote_mint,
            m.token_mint,
            *trader_quote,
            *trader_token,
            m.quote_vault,
        )
    } else {
        (
            m.token_mint,
            m.quote_mint,
            *trader_token,
            *trader_quote,
            m.token_vault,
        )
    };
    client::SwapAccounts {
        trader: *trader,
        strategy_program: GACHA_PROGRAM_ID,
        market_state: m.curve,
        trader_input,
        trader_output,
        input_vault,
        input_mint,
        output_mint,
        input_token_program: TOKEN,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts,
        input_hook_accounts: vec![],
    }
}

/// Exact-in arguments for a buy into pack `index`.
fn pack_args(amount_in: u64, min_amount_out: u64, index: u16) -> client::SwapArgs {
    client::SwapArgs {
        extra: index.to_le_bytes().to_vec(),
        ..exact_in(amount_in, min_amount_out)
    }
}

fn open_pack_ix(m: &GachaMarket, trader: &Pubkey, index: u16) -> Instruction {
    let mut data = client::instruction_discriminator("open_pack").to_vec();
    data.extend_from_slice(&index.to_le_bytes());
    Instruction::new_with_bytes(
        GACHA_PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new(*trader, true),
            AccountMeta::new(m.curve, false),
            AccountMeta::new(pack_pda(&m.curve, trader, index), false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    )
}

fn close_pack_ix(m: &GachaMarket, trader: &Pubkey, index: u16) -> Instruction {
    Instruction::new_with_bytes(
        GACHA_PROGRAM_ID,
        &client::instruction_discriminator("close_pack"),
        vec![
            AccountMeta::new(*trader, true),
            AccountMeta::new(pack_pda(&m.curve, trader, index), false),
        ],
    )
}

/// `open_draw` for draw `i` of the trader's pack `index`, paid to `trader_token`.
fn open_draw_ix(
    env: &Env,
    m: &GachaMarket,
    trader: &Pubkey,
    index: u16,
    i: u8,
    trader_token: &Pubkey,
) -> Instruction {
    let pack_key = pack_pda(&m.curve, trader, index);
    let randomness = if pack(env, &pack_key).is_some() && (i as usize) < PACK_DRAWS {
        randomness_pda(&draw(env, &pack_key, i).4)
    } else {
        Pubkey::new_unique()
    };
    let mut data = client::instruction_discriminator("open_draw").to_vec();
    data.push(i);
    Instruction::new_with_bytes(
        GACHA_PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new(pack_key, false),
            AccountMeta::new_readonly(m.curve, false),
            AccountMeta::new_readonly(*trader, false),
            AccountMeta::new(m.pot_vault, false),
            AccountMeta::new(*trader_token, false),
            AccountMeta::new_readonly(m.token_mint, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(randomness, false),
        ],
    )
}

/// The associated token account, created if it does not exist yet.
fn ata_for(env: &mut Env, owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    let address = pda::associated_token(owner, mint, &TOKEN);
    if env.svm.get_account(&address).is_none() {
        env.create_ata(owner, mint, &TOKEN);
    }
    address
}

/// `claim_fees` by `claimant`, paid to their associated token accounts.
fn claim_fees_ix(env: &mut Env, m: &GachaMarket, claimant: &Pubkey) -> Instruction {
    let claimant_quote = ata_for(env, claimant, &m.quote_mint);
    let claimant_token = ata_for(env, claimant, &m.token_mint);
    Instruction::new_with_bytes(
        GACHA_PROGRAM_ID,
        &client::instruction_discriminator("claim_fees"),
        vec![
            AccountMeta::new_readonly(*claimant, true),
            AccountMeta::new(m.curve, false),
            AccountMeta::new(m.quote_vault, false),
            AccountMeta::new(m.token_vault, false),
            AccountMeta::new(claimant_quote, false),
            AccountMeta::new(claimant_token, false),
            AccountMeta::new_readonly(m.quote_mint, false),
            AccountMeta::new_readonly(m.token_mint, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(pda::program_data(&GACHA_PROGRAM_ID), false),
        ],
    )
}

/// A funded trader holding `quote` of the quote token, with an empty token account.
fn trader(env: &mut Env, m: &GachaMarket, quote: u64) -> (Keypair, Pubkey, Pubkey) {
    let (trader, trader_quote) = env.trader_with(&m.quote_mint, &TOKEN, quote);
    let trader_token = env.create_ata(&trader.pubkey(), &m.token_mint, &TOKEN);
    (trader, trader_quote, trader_token)
}

/// Buys `amount` into the trader's pack `index`, opening the pack first in the same
/// transaction if it does not exist. Returns the trader's base output and what the pot
/// received.
fn pack_buy(
    env: &mut Env,
    m: &GachaMarket,
    t: &(Keypair, Pubkey, Pubkey),
    index: u16,
    amount: u64,
) -> (u64, u64) {
    let trader_key = t.0.pubkey();
    let (base, pot) = price_buy(env, m, amount);
    let randomness = next_randomness(env, m, &trader_key, index);
    let accounts = swap_accounts(m, &trader_key, true, Some((index, randomness)), &t.1, &t.2);
    let mut instructions = vec![];
    if pack(env, &pack_pda(&m.curve, &trader_key, index)).is_none() {
        instructions.push(open_pack_ix(m, &trader_key, index));
    }
    instructions.push(client::swap(&accounts, &pack_args(amount, base, index)));
    env.send(&instructions, &[&t.0]).expect("buy into a pack");
    (base, pot)
}

fn current_slot(env: &Env) -> u64 {
    env.svm.get_sysvar::<solana_clock::Clock>().slot
}

const LOSE: u8 = 0;
const WIN: u8 = 1;
const JACKPOT: u8 = 2;

/// The strategy's roll, mirrored: the first 8 bytes of the randomness as a little-
/// endian u64; a multiple of `JACKPOT_ONE_IN` is a jackpot, else a multiple of
/// `WIN_ONE_IN` is a win.
fn tier(randomness: &[u8; 64]) -> u8 {
    let value = u64::from_le_bytes(randomness[..8].try_into().unwrap());
    if value % JACKPOT_ONE_IN as u64 == 0 {
        JACKPOT
    } else if value % WIN_ONE_IN as u64 == 0 {
        WIN
    } else {
        LOSE
    }
}

/// Randomness of the wanted tier.
fn randomness_that(want: u8) -> [u8; 64] {
    (0u64..)
        .map(|nonce| {
            let mut randomness = [7u8; 64];
            randomness[..8].copy_from_slice(&nonce.to_le_bytes());
            randomness
        })
        .find(|randomness| tier(randomness) == want)
        .unwrap()
}

// ------------------------------------------------------------------- the quoter

fn quoter_for(env: &Env, m: &GachaMarket, extra_keys: &[Pubkey]) -> Quoter {
    let mut quoter = Quoter::new();
    for (id, name) in [
        (client::PROGRAM_ID, "pipeline"),
        (GACHA_PROGRAM_ID, "gacha_curve"),
    ] {
        let elf = std::fs::read(sbf_dir().join(format!("{name}.so"))).unwrap();
        quoter.load_program(id, &elf).unwrap();
    }
    let mut keys = vec![
        pda::config(),
        m.market,
        m.curve,
        m.quote_mint,
        m.token_mint,
        m.quote_vault,
        m.token_vault,
        m.pot_vault,
        pda::fee_account(&m.market, &m.quote_mint, &TOKEN),
        pda::fee_account(&m.market, &m.token_mint, &TOKEN),
    ];
    keys.extend_from_slice(extra_keys);
    for key in keys {
        if let Some(account) = env.svm.get_account(&key) {
            quoter.set_account(key, account).unwrap();
        }
    }
    quoter
}

fn view(env: &Env, m: &GachaMarket) -> MarketView {
    MarketView::from_market(m.market, &env.market(&m.market), vec![TOKEN, TOKEN]).unwrap()
}

fn quote_request(
    m: &GachaMarket,
    buy: bool,
    amount: u64,
    trader: Option<Pubkey>,
    extra: Vec<u8>,
) -> QuoteRequest {
    let (input_mint, output_mint) = if buy {
        (m.quote_mint, m.token_mint)
    } else {
        (m.token_mint, m.quote_mint)
    };
    QuoteRequest {
        input_mint,
        output_mint,
        mode: TradeMode::ExactIn,
        amount,
        trader,
        integrator_fee_bps: 0,
        extra,
    }
}

// ------------------------------------------------------------------------ tests

#[test]
fn market_creation_mints_a_fixed_supply_into_the_curve() {
    let mut env = Env::new();
    let m = setup(&mut env);

    assert_eq!(env.token_balance(&m.token_vault), SUPPLY);
    assert_eq!(env.token_balance(&m.quote_vault), 0);
    assert_eq!(env.token_balance(&m.pot_vault), 0);
    assert_eq!(reserves(&env, &m), (0, SUPPLY));
    assert_eq!(curve_pack_nonce(&env, &m), 0);

    // The mint authority is gone and the supply is exactly what the curve holds.
    let mint = env.account_data(&m.token_mint);
    assert_eq!(
        u32::from_le_bytes(mint[..4].try_into().unwrap()),
        0,
        "mint authority"
    );
    assert_eq!(u64::from_le_bytes(mint[36..44].try_into().unwrap()), SUPPLY);
    assert_eq!(mint[44], 6);

    let market = env.market(&m.market);
    assert_eq!(market.mints(), &[m.quote_mint, m.token_mint]);
    assert_eq!(&market.vaults[..2], &[m.quote_vault, m.token_vault]);
    assert!(!market.supports_exact_out);
}

#[test]
fn plain_buys_and_sells_follow_the_curve_and_match_quotes() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 20_000_000_000);
    let (trader_key, trader_quote, trader_token) = (t.0.pubkey(), t.1, t.2);

    // Buy 1 quote unit (1e9) on the fresh curve: 0.3% of it buys for the pot, 0.05%
    // stays in the curve, 0.05% and 0.1% accrue for the creator and the protocol.
    let amount = 1_000_000_000;
    let (expected, pot) = price_buy(&env, &m, amount);
    let (pot_fee, curve_fee, creator_fee, protocol_fee) = fees_on(amount);
    assert_eq!(
        (pot_fee, curve_fee, creator_fee, protocol_fee),
        (3_000_000, 500_000, 500_000, 1_000_000)
    );
    assert_eq!(
        expected,
        curve_out(
            VIRTUAL_QUOTE as u128,
            (VIRTUAL_TOKEN + SUPPLY) as u128,
            net_in(amount)
        )
    );
    assert!(pot > 0 && pot < expected / 100);

    let mut quoter = quoter_for(&env, &m, &[]);
    let request = quote_request(&m, true, amount, None, vec![]);
    let resolved = quoter
        .resolve_accounts(&view(&env, &m), &request, |_, _| {})
        .unwrap();
    assert_eq!(
        resolved,
        vec![
            AccountMeta::new(m.token_vault, false),
            AccountMeta::new(m.pot_vault, false)
        ]
    );
    let quote = quoter.quote(&view(&env, &m), &request, &resolved).unwrap();
    assert_eq!(quote.amount_out, expected);
    assert_eq!(quote.total_spend, amount + fees::pipeline_fee(amount));

    let accounts = swap_accounts(&m, &trader_key, true, None, &trader_quote, &trader_token);
    let meta = env
        .send(
            &[client::swap(&accounts, &exact_in(amount, expected))],
            &[&t.0],
        )
        .expect("buy");
    assert_eq!(env.token_balance(&trader_token), expected);
    assert_eq!(
        env.token_balance(&trader_quote),
        20_000_000_000 - amount - fees::pipeline_fee(amount)
    );
    assert_eq!(
        reserves(&env, &m),
        (amount - claimable(amount), SUPPLY - expected - pot)
    );
    assert_eq!(owed(&env, &m), (creator_fee, 0, protocol_fee, 0));
    assert_eq!(env.token_balance(&m.quote_vault), amount);
    assert_eq!(env.token_balance(&m.pot_vault), pot);
    let event = &swap_events(&meta)[0];
    assert_eq!((event.net_sent, event.received), (amount, expected));

    // Sell half back; the quote reserve is what pays, 0.3% of the tokens go to the
    // pot, and the quoter agrees.
    let sell = expected / 2;
    let (expected_quote, sell_fee) = price_sell(&env, &m, sell);
    assert!(expected_quote > 0 && expected_quote < amount);
    assert_eq!(sell_fee, fees_on(sell).0);
    let mut quoter = quoter_for(&env, &m, &[]);
    let request = quote_request(&m, false, sell, None, vec![]);
    let resolved = quoter
        .resolve_accounts(&view(&env, &m), &request, |_, _| {})
        .unwrap();
    assert_eq!(
        resolved,
        vec![
            AccountMeta::new(m.quote_vault, false),
            AccountMeta::new(m.pot_vault, false)
        ]
    );
    let quote = quoter.quote(&view(&env, &m), &request, &resolved).unwrap();
    assert_eq!(quote.amount_out, expected_quote);

    let accounts = swap_accounts(&m, &trader_key, false, None, &trader_quote, &trader_token);
    env.send(
        &[client::swap(&accounts, &exact_in(sell, expected_quote))],
        &[&t.0],
    )
    .expect("sell");
    // Pipeline's fee on a sell is taken in the token.
    assert_eq!(
        env.token_balance(&trader_token),
        expected - sell - fees::pipeline_fee(sell)
    );
    assert_eq!(
        reserves(&env, &m),
        (
            amount - claimable(amount) - expected_quote,
            SUPPLY - expected - pot + sell - sell_fee - claimable(sell)
        )
    );
    let (_, _, sell_creator, sell_protocol) = fees_on(sell);
    assert_eq!(
        owed(&env, &m),
        (creator_fee, sell_creator, protocol_fee, sell_protocol)
    );
    assert_eq!(env.token_balance(&m.pot_vault), pot + sell_fee);
    assert_eq!(
        env.token_balance(&m.token_vault),
        SUPPLY - expected - pot + sell - sell_fee
    );

    // Exact-out is refused by the router because the market does not support it.
    let accounts = swap_accounts(&m, &trader_key, true, None, &trader_quote, &trader_token);
    let args = client::SwapArgs {
        mode: TradeMode::ExactOut,
        amount_in: amount,
        amount_out: 1_000,
        ..exact_in(amount, 0)
    };
    assert_fails_with(
        env.send(&[client::swap(&accounts, &args)], &[&t.0]),
        "ExactOutUnsupported",
    );
}

#[test]
fn a_buy_into_a_pack_pays_the_same_and_drops_a_draw() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 20_000_000_000);
    let (trader_key, trader_quote, trader_token) = (t.0.pubkey(), t.1, t.2);
    let pack_key = pack_pda(&m.curve, &trader_key, 0);

    let amount = 2_000_000_000;
    let (base, pot) = price_buy(&env, &m, amount);
    assert!(pot > 0 && pot < base);
    // A pack buy costs nothing extra: the same four fees as a plain buy.
    assert_eq!(
        fees_on(amount),
        (6_000_000, 1_000_000, 1_000_000, 2_000_000)
    );

    // The quoter resolves the pack and, in a second round, the randomness account of
    // its first draw (the pack is not open yet, so it will get the curve's next nonce),
    // and quotes the base amount.
    let mut quoter = quoter_for(&env, &m, &[]);
    let request = quote_request(
        &m,
        true,
        amount,
        Some(trader_key),
        0u16.to_le_bytes().to_vec(),
    );
    let resolved = quoter
        .resolve_accounts(&view(&env, &m), &request, |_, _| {})
        .unwrap();
    assert_eq!(
        resolved,
        vec![
            AccountMeta::new(m.token_vault, false),
            AccountMeta::new(m.pot_vault, false),
            AccountMeta::new(pack_key, false),
            AccountMeta::new_readonly(next_randomness(&env, &m, &trader_key, 0), false),
        ]
    );
    let quote = quoter.quote(&view(&env, &m), &request, &resolved).unwrap();
    assert_eq!(quote.amount_out, base);
    // Anonymous quotes carry no pack but still price the base.
    let anonymous = quote_request(&m, true, amount, None, 0u16.to_le_bytes().to_vec());
    let resolved_anonymous = quoter
        .resolve_accounts(&view(&env, &m), &anonymous, |_, _| {})
        .unwrap();
    assert_eq!(resolved_anonymous.len(), 2);
    assert_eq!(
        quoter
            .quote(&view(&env, &m), &anonymous, &resolved_anonymous)
            .unwrap()
            .amount_out,
        base
    );

    // Without an open pack the swap fails.
    let randomness = next_randomness(&env, &m, &trader_key, 0);
    let accounts = swap_accounts(
        &m,
        &trader_key,
        true,
        Some((0, randomness)),
        &trader_quote,
        &trader_token,
    );
    assert_fails_with(
        env.send(
            &[client::swap(&accounts, &pack_args(amount, base, 0))],
            &[&t.0],
        ),
        "WrongAccount",
    );

    // Opened in the same transaction, the buy drops draw 0 into it.
    let slot = current_slot(&env);
    let pot_before = env.token_balance(&m.pot_vault);
    let (got_base, got_pot) = pack_buy(&mut env, &m, &t, 0, amount);
    assert_eq!((got_base, got_pot), (base, pot));
    assert_eq!(env.token_balance(&trader_token), base);
    assert_eq!(env.token_balance(&m.pot_vault), pot);
    // Pipeline's fee is charged on the whole input, the curve's fees included.
    assert_eq!(
        env.token_balance(&trader_quote),
        20_000_000_000 - amount - fees::pipeline_fee(amount)
    );
    assert_eq!(
        reserves(&env, &m),
        (amount - claimable(amount), SUPPLY - base - pot)
    );
    let nonce = curve_pack_nonce(&env, &m);
    assert_eq!(pack(&env, &pack_key), Some((0, nonce, 1, 0)));
    assert_eq!(
        draw(&env, &pack_key, 0),
        (
            DRAW_ARMED,
            base,
            pot_before,
            slot,
            draw_seed(&pack_key, nonce, 0)
        )
    );

    // Another buy drops draw 1; a plain buy drops nothing.
    let (base2, _) = pack_buy(&mut env, &m, &t, 0, amount);
    assert_eq!(pack(&env, &pack_key), Some((0, nonce, 2, 0)));
    assert_eq!(draw(&env, &pack_key, 1).1, base2);
    let (plain, _) = price_buy(&env, &m, amount);
    let accounts = swap_accounts(&m, &trader_key, true, None, &trader_quote, &trader_token);
    env.send(
        &[client::swap(&accounts, &exact_in(amount, plain))],
        &[&t.0],
    )
    .expect("plain buy");
    assert_eq!(pack(&env, &pack_key), Some((0, nonce, 2, 0)));

    // A pack index means nothing on a sell, and other extra lengths are rejected.
    let accounts = swap_accounts(&m, &trader_key, false, None, &trader_quote, &trader_token);
    assert_fails_with(
        env.send(
            &[client::swap(&accounts, &pack_args(1_000_000, 0, 0))],
            &[&t.0],
        ),
        "BadExtra",
    );
    let accounts = swap_accounts(&m, &trader_key, true, None, &trader_quote, &trader_token);
    let odd = client::SwapArgs {
        extra: vec![7],
        ..exact_in(1_000_000, 0)
    };
    assert_fails_with(
        env.send(&[client::swap(&accounts, &odd)], &[&t.0]),
        "BadExtra",
    );
}

#[test]
fn a_full_pack_refuses_draws_until_another_is_opened() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 100_000_000_000);
    let trader_key = t.0.pubkey();
    let pack0 = pack_pda(&m.curve, &trader_key, 0);
    let pack1 = pack_pda(&m.curve, &trader_key, 1);

    for i in 0..PACK_DRAWS as u8 {
        pack_buy(&mut env, &m, &t, 0, 1_000_000_000);
        assert_eq!(draw(&env, &pack0, i).0, DRAW_ARMED);
    }
    let (_, nonce0, draws, _) = pack(&env, &pack0).unwrap();
    assert_eq!(draws as usize, PACK_DRAWS);
    // Every seed is distinct.
    let mut seeds: Vec<[u8; 32]> = (0..PACK_DRAWS as u8)
        .map(|i| draw(&env, &pack0, i).4)
        .collect();
    seeds.sort();
    seeds.dedup();
    assert_eq!(seeds.len(), PACK_DRAWS);

    // The seventeenth draw has no room.
    let amount = 1_000_000_000;
    let (base, _) = price_buy(&env, &m, amount);
    let randomness = next_randomness(&env, &m, &trader_key, 0);
    let accounts = swap_accounts(&m, &trader_key, true, Some((0, randomness)), &t.1, &t.2);
    assert_fails_with(
        env.send(
            &[client::swap(&accounts, &pack_args(amount, base, 0))],
            &[&t.0],
        ),
        "PackFull",
    );

    // A second pack takes it, with its own nonce.
    pack_buy(&mut env, &m, &t, 1, amount);
    assert_eq!(pack(&env, &pack1), Some((1, nonce0 + 1, 1, 0)));
    assert_ne!(draw(&env, &pack1, 0).4, draw(&env, &pack0, 0).4);
}

#[test]
fn packs_close_only_once_every_draw_is_opened() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 10_000_000_000);
    let trader_key = t.0.pubkey();
    let pack_key = pack_pda(&m.curve, &trader_key, 0);

    // An empty pack closes straight away, for its rent.
    let before = env.lamports(&trader_key);
    env.send(&[open_pack_ix(&m, &trader_key, 0)], &[&t.0])
        .expect("open");
    assert!(env.lamports(&trader_key) < before);
    env.send(&[close_pack_ix(&m, &trader_key, 0)], &[&t.0])
        .expect("close");
    assert_eq!(pack(&env, &pack_key), None);
    assert_eq!(env.lamports(&trader_key), before);

    // With an unopened draw it does not, and only the trader may close it at all.
    pack_buy(&mut env, &m, &t, 0, 1_000_000_000);
    assert_fails_with(
        env.send(&[close_pack_ix(&m, &trader_key, 0)], &[&t.0]),
        "PackHasUnopenedDraws",
    );
    let stranger = env.funded_keypair();
    let mut ix = close_pack_ix(&m, &trader_key, 0);
    ix.accounts[0] = AccountMeta::new(stranger.pubkey(), true);
    assert!(env.send(&[ix], &[&stranger]).is_err());

    // Once its draw is opened it closes, and a new pack 0 gets a fresh nonce.
    let (_, nonce, _, _) = pack(&env, &pack_key).unwrap();
    fulfil(&mut env, &pack_key, 0, LOSE);
    env.send(&[open_draw_ix(&env, &m, &trader_key, 0, 0, &t.2)], &[])
        .expect("open the draw");
    env.send(&[close_pack_ix(&m, &trader_key, 0)], &[&t.0])
        .expect("close after opening");
    pack_buy(&mut env, &m, &t, 0, 1_000_000_000);
    assert_eq!(pack(&env, &pack_key), Some((0, nonce + 1, 1, 0)));
}

#[test]
fn draws_open_against_fulfilled_vrf_randomness() {
    let mut env = Env::new();
    let m = setup(&mut env);

    // Several buys build up the pot before the winner's.
    for _ in 0..3 {
        let other = trader(&mut env, &m, 10_000_000_000);
        pack_buy(&mut env, &m, &other, 0, 3_000_000_000);
    }
    let t = trader(&mut env, &m, 10_000_000_000);
    let (trader_key, _, trader_token) = (t.0.pubkey(), t.1, t.2);
    let pack_key = pack_pda(&m.curve, &trader_key, 0);
    let (stake, _) = pack_buy(&mut env, &m, &t, 0, 1_000_000_000);
    let pot_before = env.token_balance(&m.pot_vault);
    let (_, nonce, _, _) = pack(&env, &pack_key).unwrap();
    let seed = draw(&env, &pack_key, 0).4;
    assert_eq!(seed, draw_seed(&pack_key, nonce, 0));

    // Until the VRF has fulfilled the seed, opening waits: not requested at all, then
    // requested but pending.
    assert_fails_with(
        env.send(
            &[open_draw_ix(&env, &m, &trader_key, 0, 0, &trader_token)],
            &[],
        ),
        "NotFulfilled",
    );
    write_randomness(&mut env, &seed, None);
    assert_fails_with(
        env.send(
            &[open_draw_ix(&env, &m, &trader_key, 0, 0, &trader_token)],
            &[],
        ),
        "NotFulfilled",
    );
    // Randomness fulfilled under a different seed is refused.
    let wrong_seed = draw_seed(&pack_key, nonce, 1);
    write_randomness(&mut env, &wrong_seed, Some(&randomness_that(WIN)));
    let mut wrong = open_draw_ix(&env, &m, &trader_key, 0, 0, &trader_token);
    wrong.accounts[7] = AccountMeta::new_readonly(randomness_pda(&wrong_seed), false);
    assert_fails_with(env.send(&[wrong], &[]), "WrongAccount");
    // So is a draw that does not exist.
    assert_fails_with(
        env.send(
            &[open_draw_ix(&env, &m, &trader_key, 0, 1, &trader_token)],
            &[],
        ),
        "DrawNotArmed",
    );
    assert_fails_with(
        env.send(
            &[open_draw_ix(&env, &m, &trader_key, 0, 16, &trader_token)],
            &[],
        ),
        "NoSuchDraw",
    );

    // The oracles fulfil the seed with a winning value. Opened by someone other than
    // the trader (the test payer), as a keeper would.
    fulfil(&mut env, &pack_key, 0, WIN);
    env.send(
        &[open_draw_ix(&env, &m, &trader_key, 0, 0, &trader_token)],
        &[],
    )
    .expect("open");

    // A fresh draw is capped by the pot as it is now.
    let bonus = stake as u128 * BONUS_BPS as u128 / 10_000;
    let cap = pot_before as u128 * MAX_WIN_POT_BPS as u128 / 10_000;
    let payout = bonus.min(cap) as u64;
    assert!(payout > 0);
    assert_eq!(env.token_balance(&trader_token), stake + payout);
    assert_eq!(env.token_balance(&m.pot_vault), pot_before - payout);
    assert_eq!(pack(&env, &pack_key), Some((0, nonce, 1, 1)));
    assert_eq!(draw(&env, &pack_key, 0).0, DRAW_OPENED);

    // A draw opens once.
    assert_fails_with(
        env.send(
            &[open_draw_ix(&env, &m, &trader_key, 0, 0, &trader_token)],
            &[],
        ),
        "DrawNotArmed",
    );
}

#[test]
fn a_buy_refuses_a_seed_whose_randomness_was_already_requested() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 10_000_000_000);
    let (trader_key, trader_quote, trader_token) = (t.0.pubkey(), t.1, t.2);

    // Someone requested (and even got) randomness for the draw's seed before the buy:
    // the buy must not arm against it, or the outcome could be known upfront.
    let seed = next_seed(&env, &m, &trader_key, 0);
    write_randomness(&mut env, &seed, Some(&randomness_that(JACKPOT)));
    let amount = 1_000_000_000;
    let (base, _) = price_buy(&env, &m, amount);
    let accounts = swap_accounts(
        &m,
        &trader_key,
        true,
        Some((0, randomness_pda(&seed))),
        &trader_quote,
        &trader_token,
    );
    let buy = [
        open_pack_ix(&m, &trader_key, 0),
        client::swap(&accounts, &pack_args(amount, base, 0)),
    ];
    assert_fails_with(env.send(&buy, &[&t.0]), "SeedAlreadyRequested");
    // A pending request blocks it just the same.
    write_randomness(&mut env, &seed, None);
    assert_fails_with(env.send(&buy, &[&t.0]), "SeedAlreadyRequested");
}

#[test]
fn a_jackpot_pays_a_multiple_of_the_stake_capped_at_its_share_of_the_pot() {
    let mut env = Env::new();
    let m = setup(&mut env);

    // Build a pot with other traders' buys, then the lucky one's.
    for _ in 0..4 {
        let other = trader(&mut env, &m, 10_000_000_000);
        pack_buy(&mut env, &m, &other, 0, 5_000_000_000);
    }
    let t = trader(&mut env, &m, 10_000_000_000);
    let (trader_key, _, trader_token) = (t.0.pubkey(), t.1, t.2);
    let pack_key = pack_pda(&m.curve, &trader_key, 0);
    let (stake, _) = pack_buy(&mut env, &m, &t, 0, 1_000_000_000);
    let pot_before = env.token_balance(&m.pot_vault);

    fulfil(&mut env, &pack_key, 0, JACKPOT);
    env.send(
        &[open_draw_ix(&env, &m, &trader_key, 0, 0, &trader_token)],
        &[],
    )
    .expect("open a jackpot");

    // 100x the stake would be far more than the pot holds, so the cap binds: half the
    // pot, which is still many times the regular win.
    let prize = stake as u128 * JACKPOT_BPS as u128 / 10_000;
    let cap = pot_before as u128 * MAX_JACKPOT_POT_BPS as u128 / 10_000;
    let payout = prize.min(cap) as u64;
    assert_eq!(payout, pot_before / 2);
    let regular_cap = pot_before / 10;
    assert!(payout > 4 * regular_cap);
    assert_eq!(env.token_balance(&trader_token), stake + payout);
    assert_eq!(env.token_balance(&m.pot_vault), pot_before - payout);
}

#[test]
fn losing_draws_pay_nothing_and_still_open() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 10_000_000_000);
    let (trader_key, _, trader_token) = (t.0.pubkey(), t.1, t.2);
    let pack_key = pack_pda(&m.curve, &trader_key, 0);
    let (stake, pot) = pack_buy(&mut env, &m, &t, 0, 1_000_000_000);

    fulfil(&mut env, &pack_key, 0, LOSE);
    env.send(
        &[open_draw_ix(&env, &m, &trader_key, 0, 0, &trader_token)],
        &[],
    )
    .expect("open a loss");
    assert_eq!(env.token_balance(&trader_token), stake);
    assert_eq!(env.token_balance(&m.pot_vault), pot);
    assert_eq!(pack(&env, &pack_key), Some((0, 1, 1, 1)));
}

#[test]
fn a_stale_draw_is_capped_by_the_pot_as_it_was_at_the_buy() {
    let mut env = Env::new();
    let m = setup(&mut env);
    // A little pot, then the trader's small buy.
    let first = trader(&mut env, &m, 20_000_000_000);
    pack_buy(&mut env, &m, &first, 0, 10_000_000_000);
    let t = trader(&mut env, &m, 10_000_000_000);
    let (trader_key, _, trader_token) = (t.0.pubkey(), t.1, t.2);
    let pack_key = pack_pda(&m.curve, &trader_key, 0);
    let pot_at_buy = env.token_balance(&m.pot_vault);
    let buy_slot = current_slot(&env);
    let (stake, _) = pack_buy(&mut env, &m, &t, 0, 1_000_000_000);
    assert_eq!(draw(&env, &pack_key, 0).2, pot_at_buy);

    // The pot grows a lot afterwards.
    for _ in 0..4 {
        let other = trader(&mut env, &m, 20_000_000_000);
        pack_buy(&mut env, &m, &other, 0, 10_000_000_000);
    }
    let pot_now = env.token_balance(&m.pot_vault);
    assert!(pot_now > 2 * pot_at_buy);

    // Opened after the fresh window, a win is capped by the pot at the buy, not now.
    env.svm.warp_to_slot(buy_slot + FRESH_WINDOW + 1);
    fulfil(&mut env, &pack_key, 0, WIN);
    env.send(
        &[open_draw_ix(&env, &m, &trader_key, 0, 0, &trader_token)],
        &[],
    )
    .expect("open late");
    let bonus = stake as u128 * BONUS_BPS as u128 / 10_000;
    let stale_cap = pot_at_buy as u128 * MAX_WIN_POT_BPS as u128 / 10_000;
    let fresh_cap = pot_now as u128 * MAX_WIN_POT_BPS as u128 / 10_000;
    assert!(bonus > fresh_cap, "the cap must bind for this check");
    assert_eq!(env.token_balance(&trader_token), stake + stale_cap as u64);
    assert!(stale_cap < fresh_cap);
}

#[test]
fn fees_accrue_in_the_vaults_and_are_claimed_by_the_creator_and_the_launchpad() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 20_000_000_000);
    let (trader_key, trader_quote, trader_token) = (t.0.pubkey(), t.1, t.2);

    // A buy and a sell leave fees in both mints.
    let amount = 5_000_000_000;
    let (got, _) = price_buy(&env, &m, amount);
    let accounts = swap_accounts(&m, &trader_key, true, None, &trader_quote, &trader_token);
    env.send(&[client::swap(&accounts, &exact_in(amount, got))], &[&t.0])
        .expect("buy");
    let sold = got / 2;
    let (back, _) = price_sell(&env, &m, sold);
    let accounts = swap_accounts(&m, &trader_key, false, None, &trader_quote, &trader_token);
    env.send(&[client::swap(&accounts, &exact_in(sold, back))], &[&t.0])
        .expect("sell");
    let (creator_quote, creator_token, protocol_quote, protocol_token) = owed(&env, &m);
    assert_eq!(
        (creator_quote, protocol_quote),
        (fees_on(amount).2, fees_on(amount).3)
    );
    assert_eq!(
        (creator_token, protocol_token),
        (fees_on(sold).2, fees_on(sold).3)
    );
    assert!(protocol_quote == 2 * creator_quote);

    // A stranger cannot claim.
    let stranger = env.funded_keypair();
    let ix = claim_fees_ix(&mut env, &m, &stranger.pubkey());
    assert_fails_with(env.send(&[ix], &[&stranger]), "NotFeeRecipient");

    // The creator claims the 0.05% side; the launchpad's share stays put.
    let creator = m.creator.pubkey();
    let ix = claim_fees_ix(&mut env, &m, &creator);
    env.send(&[ix], &[&m.creator]).expect("creator claims");
    assert_eq!(
        env.token_balance(&pda::associated_token(&creator, &m.quote_mint, &TOKEN)),
        creator_quote
    );
    assert_eq!(
        env.token_balance(&pda::associated_token(&creator, &m.token_mint, &TOKEN)),
        creator_token
    );
    assert_eq!(owed(&env, &m), (0, 0, protocol_quote, protocol_token));

    // The program's upgrade authority, the launchpad, claims the 0.1% side.
    let owner = m.owner.pubkey();
    let ix = claim_fees_ix(&mut env, &m, &owner);
    env.send(&[ix], &[&m.owner]).expect("launchpad claims");
    assert_eq!(
        env.token_balance(&pda::associated_token(&owner, &m.quote_mint, &TOKEN)),
        protocol_quote
    );
    assert_eq!(
        env.token_balance(&pda::associated_token(&owner, &m.token_mint, &TOKEN)),
        protocol_token
    );
    assert_eq!(owed(&env, &m), (0, 0, 0, 0));

    // Claims never touch the reserves or the pot, and trading continues on them.
    let (quote_reserve, token_reserve) = reserves(&env, &m);
    assert_eq!(env.token_balance(&m.quote_vault), quote_reserve);
    assert_eq!(env.token_balance(&m.token_vault), token_reserve);
    let (got, _) = price_buy(&env, &m, 1_000_000_000);
    let accounts = swap_accounts(&m, &trader_key, true, None, &trader_quote, &trader_token);
    env.send(
        &[client::swap(&accounts, &exact_in(1_000_000_000, got))],
        &[&t.0],
    )
    .expect("buy after claims");

    // Once the program is made immutable, nobody can claim the launchpad side.
    set_upgrade_authority(&mut env.svm, &GACHA_PROGRAM_ID, None);
    let ix = claim_fees_ix(&mut env, &m, &owner);
    assert_fails_with(env.send(&[ix], &[&m.owner]), "NotFeeRecipient");
}

#[test]
fn wins_are_paid_only_to_the_packs_trader() {
    let mut env = Env::new();
    let m = setup(&mut env);
    let t = trader(&mut env, &m, 10_000_000_000);
    let trader_key = t.0.pubkey();
    let pack_key = pack_pda(&m.curve, &trader_key, 0);
    pack_buy(&mut env, &m, &t, 0, 1_000_000_000);
    fulfil(&mut env, &pack_key, 0, WIN);

    // A keeper pointing the payout at their own token account is refused.
    let keeper = Pubkey::new_unique();
    let keeper_token = env.create_ata(&keeper, &m.token_mint, &TOKEN);
    assert!(env
        .send(
            &[open_draw_ix(&env, &m, &trader_key, 0, 0, &keeper_token)],
            &[]
        )
        .is_err());
    assert_eq!(draw(&env, &pack_key, 0).0, DRAW_ARMED);
}
