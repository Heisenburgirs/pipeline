//! Wire format shared by the Pipeline router, strategy programs and off-chain clients.
//!
//! A strategy is any Solana program that handles the five instructions below. Each
//! instruction's data is an 8-byte discriminator followed by a Borsh-encoded argument.
//! The discriminators equal Anchor's `sha256("global:<name>")[..8]`, so an Anchor
//! function with the same name matches without extra work.
//!
//! This crate depends only on Borsh, so it builds for on-chain programs and hosts alike.
//! Addresses are plain `[u8; 32]` to stay independent of Solana SDK versions.

use borsh::{BorshDeserialize, BorshSerialize};

/// Address of the Pipeline router program.
/// `PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34`
pub const PIPELINE_PROGRAM_ID: [u8; 32] = [
    5, 209, 145, 243, 184, 181, 46, 195, 131, 219, 88, 199, 237, 191, 215, 190, 108, 123, 221, 171,
    32, 212, 5, 188, 219, 255, 234, 54, 136, 89, 182, 255,
];

/// Version of this interface that a strategy declares at registration.
pub const INTERFACE_VERSION: u8 = 1;

/// Pipeline's fee on every swap's input, in basis points.
pub const PIPELINE_FEE_BPS: u64 = 25;
pub const BPS_DENOMINATOR: u64 = 10_000;

/// A market trades between 2 and 4 mints.
pub const MIN_MARKET_MINTS: usize = 2;
pub const MAX_MARKET_MINTS: usize = 4;

/// Clients stop asking a strategy for more accounts after this many rounds.
pub const MAX_RESOLVE_ROUNDS: u8 = 4;

/// PDA seeds used by the router.
pub mod seeds {
    pub const CONFIG: &[u8] = b"config";
    pub const STRATEGY: &[u8] = b"strategy";
    /// `["market", strategy_program, market_state]`. Signs every call into the strategy.
    pub const MARKET: &[u8] = b"market";
    /// `["fees", market]`. Owns the market's fee accounts. Never passed to strategies.
    pub const FEE_AUTHORITY: &[u8] = b"fees";
    /// `["setup", market]`. A temporary payer funded with the creator's
    /// `max_creator_lamports` for the strategy's market setup; emptied afterwards.
    pub const SETUP_PAYER: &[u8] = b"setup";
    /// `["pipeline-register"]` derived from the strategy program itself. Lets a program
    /// without an upgrade authority register by signing through a CPI.
    pub const REGISTER: &[u8] = b"pipeline-register";
}

/// Instruction discriminators: `sha256("global:<name>")[..8]`.
pub mod discriminator {
    pub const INIT_MARKET: [u8; 8] = [178, 5, 160, 22, 249, 172, 13, 224];
    pub const MARKET_INFO: [u8; 8] = [180, 137, 108, 173, 216, 7, 77, 97];
    pub const RESOLVE_ACCOUNTS: [u8; 8] = [12, 225, 237, 111, 47, 123, 6, 62];
    pub const QUOTE: [u8; 8] = [134, 139, 91, 59, 182, 37, 152, 218];
    pub const SWAP: [u8; 8] = [161, 235, 193, 107, 155, 234, 162, 140];
    /// Optional extension: see [`super::seed_accounts`].
    pub const SEED_LIQUIDITY: [u8; 8] = [197, 81, 12, 118, 24, 65, 212, 81];
}

/// Fixed accounts at the start of `pipeline_swap`, `pipeline_quote` and
/// `pipeline_resolve_accounts`. Strategy-specific accounts follow.
///
/// In `pipeline_swap`, the market signer is a router PDA that signs, and the trader is
/// never a signer: a strategy can deposit into the trader's token accounts but cannot
/// withdraw from them. In quotes and account resolution nothing signs, and the trader
/// accounts may be placeholders.
pub mod trade_accounts {
    pub const MARKET_SIGNER: usize = 0;
    pub const MARKET_STATE: usize = 1;
    pub const TRADER: usize = 2;
    pub const TRADER_INPUT: usize = 3;
    pub const TRADER_OUTPUT: usize = 4;
    pub const INPUT_VAULT: usize = 5;
    pub const INPUT_MINT: usize = 6;
    pub const OUTPUT_MINT: usize = 7;
    pub const INPUT_TOKEN_PROGRAM: usize = 8;
    pub const OUTPUT_TOKEN_PROGRAM: usize = 9;
    pub const LEN: usize = 10;
}

/// Fixed accounts at the start of `pipeline_init_market`; strategy-specific accounts
/// follow. The payer is a router PDA holding at most the creator's
/// `max_creator_lamports`: pay rent from it, but don't reassign or allocate it. The
/// creator's own signature never reaches the strategy.
pub mod init_accounts {
    pub const MARKET_SIGNER: usize = 0;
    pub const MARKET_STATE: usize = 1;
    pub const PAYER: usize = 2;
    pub const SYSTEM_PROGRAM: usize = 3;
    pub const LEN: usize = 4;
}

/// Fixed accounts at the start of `pipeline_market_info`; strategy-specific accounts
/// follow.
pub mod info_accounts {
    pub const MARKET_STATE: usize = 0;
    pub const LEN: usize = 1;
}

/// Optional extension: `pipeline_seed_liquidity`, which lets another program (such as a
/// launchpad graduating a token) create and fund a new market on a strategy in one call.
///
/// Accounts: the fixed ones below, then three per mint (mint, source escrow, token
/// program), then strategy-specific accounts, then transfer-hook accounts.
///
/// A strategy implementing it must:
/// - derive the new market state's address from the seeder, so nobody can create or
///   fund the market before the seeder does;
/// - take at most `amounts[i]` from each escrow, and open the market at the price the
///   amounts imply;
/// - accept trades only from the router's market signer for the new market state, if it
///   is router-only;
/// - set return data to [`SeedResult`].
///
/// The caller then records the market with the router's `register_existing_market`.
pub mod seed_accounts {
    /// A PDA of the calling program, signing. It owns the escrows.
    pub const SEEDER: usize = 0;
    /// Writable; the strategy creates it.
    pub const MARKET_STATE: usize = 1;
    /// Signer, writable, system-owned, holding a bounded amount of SOL. Pay rent from it;
    /// don't reassign or allocate it.
    pub const PAYER: usize = 2;
    pub const SYSTEM_PROGRAM: usize = 3;
    /// Owner of any LP tokens or position the strategy issues. Strategies without LP
    /// shares ignore it.
    pub const LP_RECIPIENT: usize = 4;
    pub const LEN: usize = 5;
    /// Accounts per mint after the fixed ones: mint, escrow (writable), token program.
    pub const PER_MINT: usize = 3;
}

/// Argument of `pipeline_seed_liquidity`.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct SeedRequest {
    /// The most to take from each escrow, in mint order.
    pub amounts: Vec<u64>,
    /// Strategy-specific setup data, such as a fee tier.
    pub data: Vec<u8>,
}

/// Return data of `pipeline_seed_liquidity`. Callers should measure their escrows
/// rather than trust `taken`.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct SeedResult {
    pub taken: Vec<u64>,
    /// LP tokens or shares issued to the LP recipient (0 if the strategy has none).
    pub lp_amount: u64,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TradeMode {
    /// `amount` is the input the strategy receives; it reports the output.
    ExactIn,
    /// `amount` is the output the strategy must send; it reports the input it needs.
    ExactOut,
}

/// What a trader wants, as seen by the strategy.
///
/// Amounts are measured at the strategy's own vaults: input is what arrives in the
/// input vault (after any token transfer fee), output is what leaves the output vault
/// (before any token transfer fee). Clients convert to the trader's view.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct TradeRequest {
    pub mode: TradeMode,
    /// Index into the market's mints.
    pub input_index: u8,
    pub output_index: u8,
    /// Exact-in: what the router sends into the input vault (`amount_in`, before any
    /// Token-2022 transfer fee, so less may arrive). Exact-out: the output to deliver.
    pub amount: u64,
    /// `None` for anonymous quotes, such as an aggregator's.
    pub trader: Option<[u8; 32]>,
    /// Strategy-specific data, passed through from the client unchanged.
    pub extra: Vec<u8>,
}

/// Argument of `pipeline_swap`.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct SwapRequest {
    pub trade: TradeRequest,
    /// Gross input the router transferred into the input vault just before this call:
    /// `amount` for exact-in, the trader's maximum for exact-out. Strategies should
    /// measure what actually arrived rather than trust this.
    pub amount_sent: u64,
    /// The trader's minimum output. The router enforces it independently.
    pub min_amount_out: u64,
}

/// Return data of `pipeline_quote`, in the strategy's vault-level units.
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuoteResult {
    /// Input the strategy keeps. Exact-in may keep less than requested and refund the rest.
    pub amount_in: u64,
    /// Output the strategy sends.
    pub amount_out: u64,
}

/// Return data of `pipeline_market_info`.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct MarketInfo {
    pub mints: Vec<[u8; 32]>,
    /// The token account that receives input for each mint, in the same order as `mints`.
    pub vaults: Vec<[u8; 32]>,
    pub supports_exact_out: bool,
}

/// Argument of `pipeline_resolve_accounts`.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct ResolveRequest {
    pub trade: TradeRequest,
    /// Starts at 0. In later rounds, the accounts returned so far are passed after the
    /// fixed accounts, in order, so the strategy can read them.
    pub round: u8,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountRef {
    pub address: [u8; 32],
    pub writable: bool,
}

/// Return data of `pipeline_resolve_accounts`: the full list of strategy-specific
/// accounts known so far (not just this round's additions).
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct ResolveResult {
    pub accounts: Vec<AccountRef>,
    /// `false` if the strategy needs to read the returned accounts before it can list more.
    pub complete: bool,
}

/// Encodes instruction data: discriminator followed by the Borsh-encoded argument.
pub fn instruction_data<T: BorshSerialize>(discriminator: [u8; 8], arg: &T) -> Vec<u8> {
    let mut data = discriminator.to_vec();
    arg.serialize(&mut data)
        .expect("serialising into a Vec cannot fail");
    data
}

/// Decodes instruction data written by [`instruction_data`]. Returns `None` if the
/// discriminator does not match or the argument does not decode.
pub fn parse_instruction<T: BorshDeserialize>(discriminator: [u8; 8], data: &[u8]) -> Option<T> {
    let rest = data.strip_prefix(&discriminator)?;
    T::try_from_slice(rest).ok()
}

/// How much input a strategy should credit for a swap: what arrived in its input vault
/// beyond the recorded reserve, but never more than the router sent in this call.
///
/// Anything beyond `amount_sent` was deposited some other way (for example by an
/// earlier instruction in the same transaction) and must be treated as a donation.
/// Crediting it would let a trader pre-deposit most of their input outside Pipeline and
/// pay Pipeline's fee only on a token amount.
pub fn credited_input(vault_balance: u64, recorded_reserve: u64, amount_sent: u64) -> u64 {
    vault_balance
        .saturating_sub(recorded_reserve)
        .min(amount_sent)
}

/// Fee maths. All fees round up, so a non-zero amount always pays at least one unit.
pub mod fees {
    use super::{BPS_DENOMINATOR, PIPELINE_FEE_BPS};

    /// `ceil(amount × bps / 10,000)`, or `None` if that doesn't fit in a u64 (only
    /// possible above 100%).
    ///
    /// Computed in 64 bits, exactly. With `amount = q × D + r` and `bps = bq × D + br`
    /// (`D` = 10,000), `amount × bps / D = q × bps + r × bq + r × br / D`, and the first
    /// two terms are whole, so the ceiling only applies to `r × br / D`, where
    /// `r × br < D²` always fits. Each term is at most the true result, so a term or sum
    /// overflows exactly when the result doesn't fit. Avoiding 128-bit division keeps
    /// this provable with Kani for every input.
    pub fn ceil_bps(amount: u64, bps: u64) -> Option<u64> {
        let (q, r) = (amount / BPS_DENOMINATOR, amount % BPS_DENOMINATOR);
        let (bq, br) = (bps / BPS_DENOMINATOR, bps % BPS_DENOMINATOR);
        let whole = q.checked_mul(bps)?;
        let middle = r.checked_mul(bq)?;
        let rest = (r * br).div_ceil(BPS_DENOMINATOR);
        whole.checked_add(middle)?.checked_add(rest)
    }

    /// Pipeline's fee on the input a trader actually parted with.
    pub fn pipeline_fee(net_sent: u64) -> u64 {
        ceil_bps(net_sent, PIPELINE_FEE_BPS).expect("25 bps of a u64 fits in a u64")
    }

    /// The most a trader can spend on a swap that sends `amount_sent` to the strategy:
    /// the amount plus Pipeline's and the integrator's fees on all of it.
    pub fn max_total_spend(amount_sent: u64, integrator_fee_bps: u16) -> Option<u64> {
        let pipeline = pipeline_fee(amount_sent);
        let integrator = ceil_bps(amount_sent, integrator_fee_bps as u64)?;
        amount_sent.checked_add(pipeline)?.checked_add(integrator)
    }

    /// The largest amount to send to the strategy so that the total spend, including
    /// both fees, stays within `budget`.
    pub fn amount_for_budget(budget: u64, integrator_fee_bps: u16) -> u64 {
        let rate = BPS_DENOMINATOR as u128 + PIPELINE_FEE_BPS as u128 + integrator_fee_bps as u128;
        // Start from the exact-rate estimate, then step down past rounding.
        let mut amount = ((budget as u128 * BPS_DENOMINATOR as u128) / rate) as u64;
        while amount > 0 {
            match max_total_spend(amount, integrator_fee_bps) {
                Some(total) if total <= budget => break,
                _ => amount -= 1,
            }
        }
        amount
    }
}

/// What a router swap moved and what it is charged, computed from the balances the
/// router measured around the strategy call. The router uses this, so quoters and SDKs
/// can share its exact definition of `net_sent`.
pub mod settlement {
    use super::fees;

    /// Token balances measured before and after the strategy call.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Measured {
        /// The trader's input account.
        pub input_before: u64,
        pub input_after: u64,
        /// The trader's output account.
        pub output_before: u64,
        pub output_after: u64,
        /// The market's input vault (measured before the router's own transfer into it).
        pub vault_before: u64,
        pub vault_after: u64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Settlement {
        /// What the trader's input account lost.
        pub trader_spent: u64,
        /// What arrived in the trader's output account.
        pub received: u64,
        /// What the market's input vault gained.
        pub vault_kept: u64,
        /// The input the swap is charged for: `min(amount_in, max(trader_spent,
        /// vault_kept))`. Code other than the strategy (a transfer hook, for one) can top
        /// up the trader's input account during the call; the vault is the market's own.
        pub net_sent: u64,
        pub pipeline_fee: u64,
        pub integrator_fee: u64,
    }

    /// Why a swap is refused, in the order the checks run.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum SettleError {
        /// Less than `min_amount_out` arrived.
        InsufficientOutput,
        /// The trader lost more than `amount_in` before fees (for example through a
        /// permanent delegate).
        SpendLimitExceeded,
        /// Output arrived but no input was kept.
        OutputWithoutInput,
        /// Nothing was sent and nothing received.
        NothingSwapped,
        /// The integrator fee doesn't fit in a u64 (only possible above 100%).
        MathOverflow,
    }

    pub fn settle(
        measured: &Measured,
        amount_in: u64,
        min_amount_out: u64,
        integrator_fee_bps: u16,
    ) -> Result<Settlement, SettleError> {
        let trader_spent = measured.input_before.saturating_sub(measured.input_after);
        let received = measured.output_after.saturating_sub(measured.output_before);
        let vault_kept = measured.vault_after.saturating_sub(measured.vault_before);
        if received < min_amount_out {
            return Err(SettleError::InsufficientOutput);
        }
        if trader_spent > amount_in {
            return Err(SettleError::SpendLimitExceeded);
        }
        let net_sent = trader_spent.max(vault_kept).min(amount_in);
        if net_sent == 0 {
            return Err(if received > 0 {
                SettleError::OutputWithoutInput
            } else {
                SettleError::NothingSwapped
            });
        }
        let pipeline_fee = fees::pipeline_fee(net_sent);
        let integrator_fee =
            fees::ceil_bps(net_sent, integrator_fee_bps as u64).ok_or(SettleError::MathOverflow)?;
        Ok(Settlement {
            trader_spent,
            received,
            vault_kept,
            net_sent,
            pipeline_fee,
            integrator_fee,
        })
    }

    /// Where the input mint's `hook_count` transfer-hook accounts start in a swap's
    /// remaining accounts (they are the last ones), or `None` if there are fewer.
    pub fn hook_split(remaining_len: usize, hook_count: u8) -> Option<usize> {
        remaining_len.checked_sub(hook_count as usize)
    }
}

/// Book of Knowledge Kani proofs (`cargo kani -p pipeline-strategy-interface`).
#[cfg(kani)]
mod bok_kani;

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn anchor_discriminator(name: &str) -> [u8; 8] {
        Sha256::digest(format!("global:{name}").as_bytes())[..8]
            .try_into()
            .unwrap()
    }

    #[test]
    fn discriminators_match_anchor() {
        assert_eq!(
            discriminator::INIT_MARKET,
            anchor_discriminator("pipeline_init_market")
        );
        assert_eq!(
            discriminator::MARKET_INFO,
            anchor_discriminator("pipeline_market_info")
        );
        assert_eq!(
            discriminator::RESOLVE_ACCOUNTS,
            anchor_discriminator("pipeline_resolve_accounts")
        );
        assert_eq!(discriminator::QUOTE, anchor_discriminator("pipeline_quote"));
        assert_eq!(discriminator::SWAP, anchor_discriminator("pipeline_swap"));
        assert_eq!(
            discriminator::SEED_LIQUIDITY,
            anchor_discriminator("pipeline_seed_liquidity")
        );
    }

    #[test]
    fn instruction_data_round_trips() {
        let request = SwapRequest {
            trade: TradeRequest {
                mode: TradeMode::ExactOut,
                input_index: 1,
                output_index: 0,
                amount: 42,
                trader: Some([7; 32]),
                extra: vec![1, 2, 3],
            },
            amount_sent: 100,
            min_amount_out: 40,
        };
        let data = instruction_data(discriminator::SWAP, &request);
        assert_eq!(
            parse_instruction::<SwapRequest>(discriminator::SWAP, &data),
            Some(request)
        );
        assert_eq!(
            parse_instruction::<SwapRequest>(discriminator::QUOTE, &data),
            None
        );
    }

    #[test]
    fn credited_input_ignores_pre_deposits() {
        assert_eq!(credited_input(1_100, 1_000, 100), 100);
        assert_eq!(credited_input(1_098, 1_000, 100), 98); // transfer fee took 2
        assert_eq!(credited_input(6_000, 1_000, 1), 1); // 4,999 pre-deposited: donated
        assert_eq!(credited_input(900, 1_000, 100), 0);
    }

    #[test]
    fn fees_round_up() {
        assert_eq!(fees::pipeline_fee(0), 0);
        assert_eq!(fees::pipeline_fee(1), 1);
        assert_eq!(fees::pipeline_fee(400), 1);
        assert_eq!(fees::pipeline_fee(401), 2);
        assert_eq!(fees::pipeline_fee(10_000), 25);
        assert_eq!(fees::pipeline_fee(10_001), 26);
        assert_eq!(fees::pipeline_fee(u64::MAX), 46_116_860_184_273_880);
        assert_eq!(fees::ceil_bps(u64::MAX, 10_000), Some(u64::MAX));
        assert_eq!(fees::ceil_bps(u64::MAX, 10_001), None);
    }

    #[test]
    fn budget_inverts_total_spend() {
        for budget in [0u64, 1, 2, 999, 1_000, 1_000_000, 123_456_789, u64::MAX / 2] {
            for bps in [0u16, 1, 50, 100, 9_999] {
                let amount = fees::amount_for_budget(budget, bps);
                let total = fees::max_total_spend(amount, bps).unwrap();
                assert!(total <= budget, "budget {budget} bps {bps}");
                if let Some(next) = fees::max_total_spend(amount + 1, bps) {
                    assert!(next > budget, "budget {budget} bps {bps} not maximal");
                }
            }
        }
    }

    fn measured(spent: u64, received: u64, kept: u64) -> settlement::Measured {
        settlement::Measured {
            input_before: 1_000_000,
            input_after: 1_000_000 - spent,
            output_before: 5,
            output_after: 5 + received,
            vault_before: 7,
            vault_after: 7 + kept,
        }
    }

    #[test]
    fn settle_charges_the_larger_of_spend_and_vault_gain_up_to_amount_in() {
        use settlement::settle;
        // Honest swap: spend = what the vault kept.
        let s = settle(&measured(1_000, 990, 1_000), 1_000, 0, 0).unwrap();
        assert_eq!((s.net_sent, s.pipeline_fee, s.received), (1_000, 3, 990));
        // A hook refilled the trader's input: the vault's gain is charged.
        let s = settle(&measured(1, 990, 1_000), 1_000, 0, 0).unwrap();
        assert_eq!(s.net_sent, 1_000);
        // Transfer-fee input: the vault kept less; the trader's spend is charged.
        let s = settle(&measured(1_000, 990, 990), 1_000, 0, 0).unwrap();
        assert_eq!(s.net_sent, 1_000);
        // Never more than amount_in, whatever the vault gained.
        let s = settle(&measured(1_000, 990, 5_000), 1_000, 0, 50).unwrap();
        assert_eq!((s.net_sent, s.integrator_fee), (1_000, 5));
    }

    #[test]
    fn settle_refuses_in_a_fixed_order() {
        use settlement::{settle, SettleError::*};
        assert_eq!(
            settle(&measured(1_000, 9, 1_000), 1_000, 10, 0),
            Err(InsufficientOutput)
        );
        assert_eq!(
            settle(&measured(1_001, 9, 1_001), 1_000, 0, 0),
            Err(SpendLimitExceeded)
        );
        assert_eq!(
            settle(&measured(0, 9, 0), 1_000, 0, 0),
            Err(OutputWithoutInput)
        );
        assert_eq!(settle(&measured(0, 0, 0), 1_000, 0, 0), Err(NothingSwapped));
        assert_eq!(
            settle(&measured(10, 1, 10), u64::MAX, 0, u16::MAX).map(|s| s.integrator_fee),
            Ok(66)
        );
    }

    #[test]
    fn hook_split_takes_the_trailing_accounts() {
        use settlement::hook_split;
        assert_eq!(hook_split(5, 2), Some(3));
        assert_eq!(hook_split(2, 2), Some(0));
        assert_eq!(hook_split(1, 2), None);
    }

    /// The 64-bit `ceil_bps` equals the 128-bit definition, including where it overflows.
    #[test]
    fn ceil_bps_matches_the_128_bit_definition() {
        let reference = |a: u64, b: u64| {
            u64::try_from((a as u128 * b as u128).div_ceil(BPS_DENOMINATOR as u128)).ok()
        };
        let edges = [
            0,
            1,
            9_999,
            10_000,
            10_001,
            65_535,
            u32::MAX as u64,
            u64::MAX / 10_000,
            u64::MAX / 10_000 + 1,
            u64::MAX - 1,
            u64::MAX,
        ];
        for &a in &edges {
            for &b in &edges {
                assert_eq!(fees::ceil_bps(a, b), reference(a, b), "{a} × {b}");
            }
        }
        // A cheap deterministic sweep over mixed magnitudes.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let a = x >> (x % 64);
            let b = x.rotate_left(17) >> ((x >> 8) % 64);
            assert_eq!(fees::ceil_bps(a, b), reference(a, b), "{a} × {b}");
        }
    }
}
