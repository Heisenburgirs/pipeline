// Quotes a strategy and resolves its accounts by simulating `pipeline_quote` and
// `pipeline_resolve_accounts` through any RPC node. This is the zero-infrastructure
// fallback; `pipeline-quoter` (Rust) does the same in an embedded VM.
//
// The instructions use the same fixed read accounts as `pipeline-quoter`: everything
// read-only, nothing signs, and the trader's token accounts are its associated token
// accounts (which need not exist). An anonymous trade uses the all-zero address.

import {
    AccountRole,
    appendTransactionMessageInstructions,
    compileTransaction,
    createTransactionMessage,
    getBase64EncodedWireTransaction,
    getBase64Encoder,
    pipe,
    setTransactionMessageFeePayer,
    setTransactionMessageLifetimeUsingBlockhash,
    type AccountMeta,
    type Address,
    type Base64EncodedWireTransaction,
    type Blockhash,
    type Commitment,
    type Decoder,
    type Rpc,
    type SimulateTransactionApi,
    type Slot,
} from '@solana/kit';

import {
    decodeReturnData,
    getQuoteResultCodec,
    getResolveRequestCodec,
    getResolveResultCodec,
    getTradeRequestCodec,
    instructionData,
    marketMints,
    type Market,
    type QuoteResult,
    type TradeRequest,
} from './codecs.js';
import {
    COMPUTE_BUDGET_PROGRAM_ADDRESS,
    DEFAULT_ADDRESS,
    MAX_RESOLVE_ROUNDS,
    STRATEGY_DISCRIMINATORS,
} from './constants.js';
import type { PipelineInstruction } from './instructions.js';
import { associatedTokenAddress } from './pda.js';

/** Default compute cap for a strategy's read-only call, as in `pipeline-quoter`. */
export const DEFAULT_COMPUTE_LIMIT = 200_000;

/** The parts of a Pipeline market a quote needs. Build it with {@link marketViewFromMarket}. */
export type MarketView = {
    market: Address;
    strategyProgram: Address;
    marketState: Address;
    mints: readonly Address[];
    vaults: readonly Address[];
    /** Each mint's owning token program, in mint order. */
    tokenPrograms: readonly Address[];
    supportsExactOut: boolean;
};

/** `tokenPrograms` must give each mint's owning token program, in mint order. */
export function marketViewFromMarket(
    marketAddress: Address,
    market: Market,
    tokenPrograms: readonly Address[],
): MarketView {
    const mints = marketMints(market);
    if (tokenPrograms.length !== mints.length) {
        throw new RangeError(`expected ${mints.length} token programs, got ${tokenPrograms.length}`);
    }
    return {
        market: marketAddress,
        strategyProgram: market.strategyProgram,
        marketState: market.marketState,
        mints,
        vaults: market.vaults.slice(0, mints.length),
        tokenPrograms,
        supportsExactOut: market.supportsExactOut,
    };
}

function at<T>(list: readonly T[], index: number, what: string): T {
    const value = list[index];
    if (value === undefined) {
        throw new RangeError(`${what} index ${index} is out of range`);
    }
    return value;
}

function readonly(address: Address): AccountMeta {
    return { address, role: AccountRole.READONLY };
}

/** The fixed trade accounts for a read-only strategy call. */
async function readAccounts(market: MarketView, trade: TradeRequest): Promise<AccountMeta[]> {
    const trader = trade.trader ?? DEFAULT_ADDRESS;
    const inputMint = at(market.mints, trade.inputIndex, 'input mint');
    const outputMint = at(market.mints, trade.outputIndex, 'output mint');
    const inputTokenProgram = at(market.tokenPrograms, trade.inputIndex, 'input token program');
    const outputTokenProgram = at(market.tokenPrograms, trade.outputIndex, 'output token program');
    return [
        readonly(market.market),
        readonly(market.marketState),
        readonly(trader),
        readonly(await associatedTokenAddress(trader, inputMint, inputTokenProgram)),
        readonly(await associatedTokenAddress(trader, outputMint, outputTokenProgram)),
        readonly(at(market.vaults, trade.inputIndex, 'input vault')),
        readonly(inputMint),
        readonly(outputMint),
        readonly(inputTokenProgram),
        readonly(outputTokenProgram),
    ];
}

/**
 * The strategy's `pipeline_quote` for `trade` (vault-level amounts). Strategy accounts are
 * passed read-only after the fixed accounts.
 */
export async function buildQuoteInstruction(
    market: MarketView,
    trade: TradeRequest,
    strategyAccounts: readonly AccountMeta[],
): Promise<PipelineInstruction> {
    return {
        programAddress: market.strategyProgram,
        accounts: [...(await readAccounts(market, trade)), ...strategyAccounts.map((m) => readonly(m.address))],
        data: instructionData(STRATEGY_DISCRIMINATORS.quote, getTradeRequestCodec(), trade),
    };
}

/**
 * The strategy's `pipeline_resolve_accounts` for one round. `known` are the accounts
 * returned by the previous round, passed read-only after the fixed accounts.
 */
export async function buildResolveAccountsInstruction(
    market: MarketView,
    trade: TradeRequest,
    round: number,
    known: readonly AccountMeta[],
): Promise<PipelineInstruction> {
    return {
        programAddress: market.strategyProgram,
        accounts: [...(await readAccounts(market, trade)), ...known.map((m) => readonly(m.address))],
        data: instructionData(STRATEGY_DISCRIMINATORS.resolveAccounts, getResolveRequestCodec(), { trade, round }),
    };
}

export type SimulationOptions = {
    /** Compute unit limit for the strategy call. Defaults to {@link DEFAULT_COMPUTE_LIMIT}. */
    computeUnitLimit?: number;
    /** Defaults to `finalized`; with Alpenglow (live on mainnet and devnet) that is ~150 ms behind the tip. */
    commitment?: Commitment;
    minContextSlot?: Slot;
};

/** The simulated strategy call failed, or returned no usable data. */
export class SimulationError extends Error {
    readonly err: unknown;
    readonly logs: readonly string[];

    constructor(message: string, err: unknown, logs: readonly string[] | null) {
        super(message);
        this.name = 'SimulationError';
        this.err = err;
        this.logs = logs ?? [];
    }
}

function setComputeUnitLimit(units: number): PipelineInstruction {
    // Compute budget `SetComputeUnitLimit`: tag 2, then the limit as a little-endian u32.
    const data = new Uint8Array(5);
    data[0] = 2;
    new DataView(data.buffer).setUint32(1, units, true);
    return { programAddress: COMPUTE_BUDGET_PROGRAM_ADDRESS, accounts: [], data };
}

// Any 32-byte value; the node replaces it (`replaceRecentBlockhash`).
const PLACEHOLDER_BLOCKHASH = '11111111111111111111111111111111' as Blockhash;

/**
 * The unsigned, base64-encoded transaction simulated for a read-only strategy call: a
 * compute limit, then `instruction`, paid by `feePayer` (which must exist on chain).
 */
export function buildSimulationTransaction(
    instruction: PipelineInstruction,
    feePayer: Address,
    computeUnitLimit: number = DEFAULT_COMPUTE_LIMIT,
): Base64EncodedWireTransaction {
    const message = pipe(
        createTransactionMessage({ version: 0 }),
        (m) => setTransactionMessageFeePayer(feePayer, m),
        (m) =>
            setTransactionMessageLifetimeUsingBlockhash(
                { blockhash: PLACEHOLDER_BLOCKHASH, lastValidBlockHeight: 0n },
                m,
            ),
        (m) => appendTransactionMessageInstructions([setComputeUnitLimit(computeUnitLimit), instruction], m),
    );
    return getBase64EncodedWireTransaction(compileTransaction(message));
}

const base64 = getBase64Encoder();

async function simulate<T>(
    rpc: Rpc<SimulateTransactionApi>,
    instruction: PipelineInstruction,
    feePayer: Address,
    decoder: Decoder<T>,
    options: SimulationOptions,
): Promise<{ value: T; unitsConsumed: bigint | null }> {
    const wire = buildSimulationTransaction(instruction, feePayer, options.computeUnitLimit);
    const { value } = await rpc
        .simulateTransaction(wire, {
            encoding: 'base64',
            sigVerify: false,
            replaceRecentBlockhash: true,
            commitment: options.commitment ?? 'finalized',
            minContextSlot: options.minContextSlot,
        })
        .send();
    if (value.err !== null) {
        throw new SimulationError('strategy call failed in simulation', value.err, value.logs);
    }
    // The runtime records no return data when it is empty or all zeros, so a missing
    // value decodes as all zeros. Data set by another program is rejected.
    const returned = value.returnData;
    if (returned !== null && returned.programId !== instruction.programAddress) {
        throw new SimulationError(`return data came from ${returned.programId}`, null, value.logs);
    }
    const bytes = returned === null ? new Uint8Array() : base64.encode(returned.data[0]);
    const decoded = decodeReturnData(decoder, bytes);
    if (decoded === null) {
        throw new SimulationError('malformed return data', null, value.logs);
    }
    return { value: decoded, unitsConsumed: value.unitsConsumed ?? null };
}

/**
 * Runs the strategy's `pipeline_quote` for `trade` by RPC simulation. Amounts are
 * vault-level: Pipeline, integrator and token transfer fees are not applied.
 *
 * `feePayer` must be an existing account with lamports; it does not sign.
 */
export async function quoteStrategy(
    rpc: Rpc<SimulateTransactionApi>,
    market: MarketView,
    trade: TradeRequest,
    strategyAccounts: readonly AccountMeta[],
    feePayer: Address,
    options: SimulationOptions = {},
): Promise<{ quote: QuoteResult; unitsConsumed: bigint | null }> {
    const instruction = await buildQuoteInstruction(market, trade, strategyAccounts);
    const { value, unitsConsumed } = await simulate(rpc, instruction, feePayer, getQuoteResultCodec(), options);
    return { quote: value, unitsConsumed };
}

/**
 * Asks the strategy which accounts `trade` needs, by RPC simulation, for up to
 * {@link MAX_RESOLVE_ROUNDS} rounds. Each round passes the accounts returned so far.
 * Returns the strategy accounts to pass to `swap` (and to {@link quoteStrategy}).
 */
export async function resolveAccounts(
    rpc: Rpc<SimulateTransactionApi>,
    market: MarketView,
    trade: TradeRequest,
    feePayer: Address,
    options: SimulationOptions = {},
): Promise<AccountMeta[]> {
    let known: AccountMeta[] = [];
    for (let round = 0; round < MAX_RESOLVE_ROUNDS; round++) {
        const instruction = await buildResolveAccountsInstruction(market, trade, round, known);
        const { value } = await simulate(rpc, instruction, feePayer, getResolveResultCodec(), options);
        known = value.accounts.map((account) => ({
            address: account.address,
            role: account.writable ? AccountRole.WRITABLE : AccountRole.READONLY,
        }));
        if (value.complete) {
            return known;
        }
    }
    throw new SimulationError(`account resolution did not finish within ${MAX_RESOLVE_ROUNDS} rounds`, null, null);
}
