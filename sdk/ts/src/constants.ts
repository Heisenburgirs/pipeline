import { address, type Address } from '@solana/kit';

/** The Pipeline router program. */
export const PIPELINE_PROGRAM_ADDRESS: Address = address('PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34');
export const SYSTEM_PROGRAM_ADDRESS: Address = address('11111111111111111111111111111111');
export const TOKEN_PROGRAM_ADDRESS: Address = address('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA');
export const TOKEN_2022_PROGRAM_ADDRESS: Address = address('TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb');
export const ASSOCIATED_TOKEN_PROGRAM_ADDRESS: Address = address('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL');
export const BPF_LOADER_UPGRADEABLE_PROGRAM_ADDRESS: Address = address('BPFLoaderUpgradeab1e11111111111111111111111');
export const COMPUTE_BUDGET_PROGRAM_ADDRESS: Address = address('ComputeBudget111111111111111111111111111111');

/** The all-zero address, used as the trader of anonymous quotes. */
export const DEFAULT_ADDRESS: Address = SYSTEM_PROGRAM_ADDRESS;

/** Version of the strategy interface a strategy declares at registration. */
export const INTERFACE_VERSION = 1;

/** Pipeline's fee on every swap's input, in basis points. */
export const PIPELINE_FEE_BPS = 25n;
export const BPS_DENOMINATOR = 10_000n;

/** A market trades between 2 and 4 mints. */
export const MIN_MARKET_MINTS = 2;
export const MAX_MARKET_MINTS = 4;

/** Clients stop asking a strategy for more accounts after this many rounds. */
export const MAX_RESOLVE_ROUNDS = 4;

/** Solana caps return data at 1 KiB. */
export const MAX_RETURN_DATA = 1024;

const utf8 = new TextEncoder();

/** PDA seeds used by the router. */
export const SEEDS = {
    config: utf8.encode('config'),
    strategy: utf8.encode('strategy'),
    /** `["market", strategy_program, market_state]`. Signs every call into the strategy. */
    market: utf8.encode('market'),
    /** `["fees", market]`. Owns the market's fee accounts. Never passed to strategies. */
    feeAuthority: utf8.encode('fees'),
    /** `["setup", market]`. Temporary payer for a strategy's market setup. */
    setupPayer: utf8.encode('setup'),
    /**
     * `["pipeline-register"]` derived from the strategy program itself. Lets a program
     * without an upgrade authority register by signing through a CPI.
     */
    register: utf8.encode('pipeline-register'),
    /** Anchor's `#[event_cpi]` authority. */
    eventAuthority: utf8.encode('__event_authority'),
} as const;

/** Strategy instruction discriminators: `sha256("global:<name>")[..8]`. */
export const STRATEGY_DISCRIMINATORS = {
    initMarket: new Uint8Array([178, 5, 160, 22, 249, 172, 13, 224]),
    marketInfo: new Uint8Array([180, 137, 108, 173, 216, 7, 77, 97]),
    resolveAccounts: new Uint8Array([12, 225, 237, 111, 47, 123, 6, 62]),
    quote: new Uint8Array([134, 139, 91, 59, 182, 37, 152, 218]),
    swap: new Uint8Array([161, 235, 193, 107, 155, 234, 162, 140]),
} as const;

/** Router instruction discriminators: `sha256("global:<name>")[..8]`. */
export const ROUTER_INSTRUCTION_DISCRIMINATORS = {
    initializeConfig: new Uint8Array([208, 127, 21, 1, 194, 190, 196, 70]),
    updateConfig: new Uint8Array([29, 158, 252, 191, 10, 83, 219, 99]),
    registerStrategy: new Uint8Array([121, 12, 64, 75, 99, 15, 177, 143]),
    updateMetadata: new Uint8Array([170, 182, 43, 239, 97, 78, 225, 186]),
    createMarket: new Uint8Array([103, 226, 97, 235, 200, 188, 251, 254]),
    registerExistingMarket: new Uint8Array([101, 160, 55, 166, 150, 150, 123, 54]),
    initFeeAccount: new Uint8Array([85, 169, 185, 211, 103, 153, 111, 2]),
    swap: new Uint8Array([248, 198, 158, 145, 225, 117, 135, 200]),
    sweepFees: new Uint8Array([175, 225, 98, 71, 118, 66, 34, 148]),
    liftPause: new Uint8Array([170, 175, 204, 42, 87, 252, 221, 120]),
} as const;

/** Router account discriminators: `sha256("account:<Name>")[..8]`. */
export const ACCOUNT_DISCRIMINATORS = {
    Config: new Uint8Array([155, 12, 170, 224, 30, 250, 204, 130]),
    Strategy: new Uint8Array([174, 110, 39, 119, 82, 106, 169, 102]),
    Market: new Uint8Array([219, 190, 213, 55, 0, 227, 198, 154]),
} as const;

/** Anchor's prefix for self-CPI event instructions (`0x1d9acb512ea545e4`, little-endian). */
export const EVENT_IX_TAG = new Uint8Array([228, 69, 165, 46, 81, 203, 154, 29]);

/** Router event discriminators: `sha256("event:<Name>")[..8]`. */
export const EVENT_DISCRIMINATORS = {
    Swap: new Uint8Array([64, 198, 205, 232, 38, 8, 113, 226]),
    StrategyRegistered: new Uint8Array([167, 54, 3, 14, 221, 37, 211, 214]),
    MetadataUpdated: new Uint8Array([132, 36, 215, 246, 166, 90, 189, 44]),
    MarketCreated: new Uint8Array([88, 184, 130, 231, 226, 84, 6, 58]),
    FeesSwept: new Uint8Array([96, 218, 115, 136, 74, 170, 202, 172]),
    ConfigChanged: new Uint8Array([147, 25, 86, 98, 98, 77, 78, 192]),
} as const;

/**
 * Positions of the fixed accounts at the start of `pipeline_swap`, `pipeline_quote` and
 * `pipeline_resolve_accounts`. Strategy-specific accounts follow.
 */
export const TRADE_ACCOUNTS = {
    marketSigner: 0,
    marketState: 1,
    trader: 2,
    traderInput: 3,
    traderOutput: 4,
    inputVault: 5,
    inputMint: 6,
    outputMint: 7,
    inputTokenProgram: 8,
    outputTokenProgram: 9,
    len: 10,
} as const;
