//! SOS audit #2, H030: a program integrator that CPIs `create_market` and marks one of its
//! own authority PDAs as a signer hands that signature to the strategy when the PDA looks
//! fresh (zero lamports, no data, System-owned), which an authority PDA always does.
//!
//! Status: accepted and documented (the router docs' known limitations, "`create_market` forwards
//! fresh signers"): the router can't tell a program PDA signing through CPI from a new
//! keypair, so programs must not pass their own PDAs to `create_market` as signers. These
//! tests pin that behaviour.
//!
//! No dedicated integrator fixture exists, so the integrator is `test-strategy-evil`'s
//! `FORWARD_TO_ROUTER`, deployed a second time under another program id (the integrator,
//! "A"). It signs the CPI with its register PDA, which here plays the integrator's
//! escrow-owning authority PDA. The hostile strategy "B" is the same binary registered
//! under its own id; its setup (mode 7) calls SPL `Approve(u64::MAX)` on the token
//! account at strategy slot 8 with the account at slot 9 as the authority.
//!
//! Call chain: tx -> A (forwarder) -> router `create_market` -> B (`init_market`) -> SPL
//! Approve. The creator is an ordinary keypair that signs the outer transaction; the
//! authority PDA is listed in the strategy accounts and signed only by A's seeds.

mod common;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
/// The integrator: the evil binary used only as a forwarder.
const INTEGRATOR_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
/// The strategy: the same binary under a second id, registered with the router.
const STRATEGY_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ3");
const FORWARD_TO_ROUTER: [u8; 8] = *b"evilfwd!";
const INIT_APPROVES_CREATOR_TOKENS: u8 = 7;

struct Scenario {
    env: Env,
    authority: Pubkey,
    escrow_tokens: Pubkey,
    ix: Instruction,
    creator: solana_keypair::Keypair,
    state: Pubkey,
}

fn delegate_of(env: &Env, token_account: &Pubkey) -> Option<Pubkey> {
    let data = env.account_data(token_account);
    (data[72] == 1).then(|| Pubkey::new_from_array(data[76..108].try_into().unwrap()))
}

/// Builds the integrator scenario. `authority_lamports` is what the integrator's authority
/// PDA holds (0 is the usual state of a PDA that only owns token accounts).
fn scenario(authority_lamports: u64) -> Scenario {
    let mut env = Env::new();
    env.register(&STRATEGY_ID, "test_strategy_evil");
    let integrator_owner = env.funded_keypair();
    env.deploy_strategy(
        &INTEGRATOR_ID,
        "test_strategy_evil",
        &integrator_owner.pubkey(),
    );

    let authority = pda::register(&INTEGRATOR_ID);
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &STRATEGY_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    // The integrator's escrow: a token account owned by its authority PDA.
    let escrow_mint = env.create_mint(&TOKEN, 6, None, None);
    let escrow_tokens = env.create_ata(&authority, &escrow_mint, &TOKEN);
    env.mint_to(&escrow_mint, &TOKEN, &escrow_tokens, 1_000_000);
    if authority_lamports > 0 {
        // Crafted state: `airdrop` refuses sub-rent balances, and the point is "non-zero".
        env.svm
            .set_account(
                authority,
                solana_account::Account {
                    lamports: authority_lamports,
                    data: vec![],
                    owner: client::SYSTEM_PROGRAM_ID,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
    }
    assert_eq!(env.lamports(&authority), authority_lamports);
    assert_eq!(env.token_balance(&escrow_tokens), 1_000_000);
    assert_eq!(delegate_of(&env, &escrow_tokens), None);

    let creator = env.funded_keypair();
    let create = client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
        strategy_program: STRATEGY_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![INIT_APPROVES_CREATOR_TOKENS],
        max_creator_lamports: SOL,
        strategy_accounts: vec![
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
            // Strategy slots 8..10: the escrow, its authority PDA as a signer, token program.
            AccountMeta::new(escrow_tokens, false),
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new_readonly(TOKEN, false),
        ],
    });
    // The integrator program forwards the instruction. The outer transaction cannot sign
    // for the PDA; the forwarder signs it with its seeds. Everything else keeps its flags.
    let mut data = FORWARD_TO_ROUTER.to_vec();
    data.extend_from_slice(&create.data);
    let mut accounts = vec![AccountMeta::new_readonly(client::PROGRAM_ID, false)];
    for meta in &create.accounts {
        let mut outer = meta.clone();
        if outer.pubkey == authority {
            outer.is_signer = false;
        }
        accounts.push(outer);
    }
    let ix = Instruction::new_with_bytes(INTEGRATOR_ID, &data, accounts);
    Scenario {
        env,
        authority,
        escrow_tokens,
        ix,
        creator,
        state,
    }
}

/// Documented behaviour (the router docs' known limitations): a zero-lamport authority PDA signed by
/// an integrator program looks fresh, so its signature reaches the strategy and the strategy
/// approves itself as a delegate over the integrator's escrow. This is why programs must not
/// pass their own PDAs to `create_market` as signers.
#[test]
fn h030_fresh_authority_pda_signature_is_forwarded_documented() {
    let mut s = scenario(0);
    let result = s.env.send(&[s.ix.clone()], &[&s.creator]);
    let meta = result.unwrap_or_else(|e| {
        panic!(
            "setup: integrator create_market failed: {:?}\n{}",
            e.err,
            e.meta.logs.join("\n")
        )
    });
    println!("{}", meta.logs.join("\n"));
    assert!(
        s.env.lamports(&pda::market(&STRATEGY_ID, &s.state)) > 0,
        "setup: market created"
    );
    assert_eq!(
        delegate_of(&s.env, &s.escrow_tokens),
        Some(s.state),
        "H030: the integrator's zero-lamport authority PDA {} looks fresh, so its signature \
         is forwarded and the strategy becomes delegate over the PDA's escrow (documented)",
        s.authority
    );
}

/// Safety/mitigation: the same call with the authority PDA holding 1 lamport (not fresh)
/// loses the signature, the strategy's Approve fails and nothing is approved. So holding
/// any lamports on the integrator's authority PDA is a complete workaround.
#[test]
fn h030_authority_pda_with_one_lamport_is_not_forwarded() {
    let mut s = scenario(1);
    let result = s.env.send(&[s.ix.clone()], &[&s.creator]);
    assert_fails_with(result, "signer privilege escalated");
    assert_eq!(
        delegate_of(&s.env, &s.escrow_tokens),
        None,
        "H030: a funded authority PDA must not be forwarded"
    );
}
