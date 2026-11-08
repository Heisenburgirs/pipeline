// RPC quoting against a fake RPC object: checks the simulated transaction and how
// results are read back. Nothing here touches the network.

import {
    AccountRole,
    decompileTransactionMessage,
    getBase64Decoder,
    getBase64Encoder,
    getCompiledTransactionMessageDecoder,
    getTransactionDecoder,
    type Address,
    type Rpc,
    type SimulateTransactionApi,
} from '@solana/kit';
import { describe, expect, it } from 'vitest';

import {
    buildQuoteInstruction,
    buildResolveAccountsInstruction,
    COMPUTE_BUDGET_PROGRAM_ADDRESS,
    getQuoteResultCodec,
    getResolveResultCodec,
    marketPda,
    quoteStrategy,
    resolveAccounts,
    SimulationError,
    TOKEN_2022_PROGRAM_ADDRESS,
    TOKEN_PROGRAM_ADDRESS,
    TradeMode,
    type MarketView,
    type ResolveResult,
    type TradeRequest,
} from '../src/index.js';
import { key } from './helpers.js';

type FakeResult = {
    err?: unknown;
    returnData?: { bytes: Uint8Array; programId: Address } | null;
};

function fakeRpc(results: FakeResult[]) {
    const calls: { transaction: string; config: any }[] = [];
    const base64 = getBase64Decoder();
    const rpc = {
        simulateTransaction(transaction: string, config: any) {
            calls.push({ transaction, config });
            const result = results.shift()!;
            const returnData = result.returnData
                ? { data: [base64.decode(result.returnData.bytes), 'base64'], programId: result.returnData.programId }
                : null;
            return {
                send: async () => ({
                    context: { slot: 1n },
                    value: { err: result.err ?? null, logs: ['log'], returnData, unitsConsumed: 1_234n },
                }),
            };
        },
    };
    return { rpc: rpc as unknown as Rpc<SimulateTransactionApi>, calls };
}

/** The instructions of a simulated transaction, as account addresses, roles and data. */
function decodeTransaction(transaction: string) {
    const { messageBytes } = getTransactionDecoder().decode(getBase64Encoder().encode(transaction));
    const message = decompileTransactionMessage(getCompiledTransactionMessageDecoder().decode(messageBytes));
    return { feePayer: message.feePayer.address, instructions: message.instructions };
}

const strategyProgram = key(7);
const feePayer = key(33);

async function market(): Promise<MarketView> {
    return {
        market: await marketPda(strategyProgram, key(8)),
        strategyProgram,
        marketState: key(8),
        mints: [key(13), key(14)],
        vaults: [key(16), key(17)],
        tokenPrograms: [TOKEN_PROGRAM_ADDRESS, TOKEN_2022_PROGRAM_ADDRESS],
        supportsExactOut: true,
    };
}

const trade: TradeRequest = {
    mode: TradeMode.ExactIn,
    inputIndex: 0,
    outputIndex: 1,
    amount: 1_000n,
    trader: key(30),
    extra: new Uint8Array(),
};

describe('quoteStrategy', () => {
    it('simulates pipeline_quote unsigned with a compute limit and reads the quote', async () => {
        const view = await market();
        const strategyAccounts = [{ address: key(21), role: AccountRole.WRITABLE }];
        const quote = { amountIn: 1_000n, amountOut: 990n };
        const { rpc, calls } = fakeRpc([
            { returnData: { bytes: Uint8Array.from(getQuoteResultCodec().encode(quote)), programId: strategyProgram } },
        ]);

        const result = await quoteStrategy(rpc, view, trade, strategyAccounts, feePayer);
        expect(result).toEqual({ quote, unitsConsumed: 1_234n });

        expect(calls).toHaveLength(1);
        expect(calls[0]!.config).toMatchObject({ encoding: 'base64', sigVerify: false, replaceRecentBlockhash: true });
        const { feePayer: payer, instructions } = decodeTransaction(calls[0]!.transaction);
        expect(payer).toBe(feePayer);
        expect(instructions).toHaveLength(2);
        expect(instructions[0]!.programAddress).toBe(COMPUTE_BUDGET_PROGRAM_ADDRESS);
        expect(Array.from(instructions[0]!.data!)).toEqual([2, 0x40, 0x0d, 0x03, 0x00]);
        const expected = await buildQuoteInstruction(view, trade, strategyAccounts);
        expect(instructions[1]!.programAddress).toBe(strategyProgram);
        expect(instructions[1]!.accounts).toEqual(expected.accounts);
        expect(Array.from(instructions[1]!.data!)).toEqual(Array.from(expected.data));
    });

    it('restores trailing zeros the runtime strips from return data', async () => {
        // A zero output quote ends in zero bytes, which RPC does not report.
        const { rpc } = fakeRpc([{ returnData: { bytes: new Uint8Array([0xe8, 0x03]), programId: strategyProgram } }]);
        const { quote } = await quoteStrategy(rpc, await market(), trade, [], feePayer);
        expect(quote).toEqual({ amountIn: 1_000n, amountOut: 0n });
    });

    it('fails when the strategy fails or another program set the return data', async () => {
        const failing = fakeRpc([{ err: { InstructionError: [1, 'Custom'] } }]);
        await expect(quoteStrategy(failing.rpc, await market(), trade, [], feePayer)).rejects.toThrow(SimulationError);

        const foreign = fakeRpc([{ returnData: { bytes: new Uint8Array([1]), programId: TOKEN_PROGRAM_ADDRESS } }]);
        await expect(quoteStrategy(foreign.rpc, await market(), trade, [], feePayer)).rejects.toThrow(
            /return data came from/,
        );
    });
});

describe('resolveAccounts', () => {
    function reply(result: ResolveResult): FakeResult {
        return {
            returnData: { bytes: Uint8Array.from(getResolveResultCodec().encode(result)), programId: strategyProgram },
        };
    }

    it('runs rounds until complete, passing known accounts read-only', async () => {
        const view = await market();
        const first = { address: key(21), writable: true };
        const second = { address: key(22), writable: false };
        const { rpc, calls } = fakeRpc([
            reply({ accounts: [first], complete: false }),
            reply({ accounts: [first, second], complete: true }),
        ]);

        const accounts = await resolveAccounts(rpc, view, trade, feePayer);
        expect(accounts).toEqual([
            { address: key(21), role: AccountRole.WRITABLE },
            { address: key(22), role: AccountRole.READONLY },
        ]);

        expect(calls).toHaveLength(2);
        for (const [round, known] of [[], [{ address: key(21), role: AccountRole.WRITABLE }]].entries()) {
            const expected = await buildResolveAccountsInstruction(view, trade, round, known);
            const { instructions } = decodeTransaction(calls[round]!.transaction);
            expect(instructions[1]!.accounts).toEqual(expected.accounts);
            expect(Array.from(instructions[1]!.data!)).toEqual(Array.from(expected.data));
        }
    });

    it('gives up after four incomplete rounds', async () => {
        const incomplete = reply({ accounts: [], complete: false });
        const { rpc, calls } = fakeRpc([incomplete, incomplete, incomplete, incomplete, incomplete]);
        await expect(resolveAccounts(rpc, await market(), trade, feePayer)).rejects.toThrow(/within 4 rounds/);
        expect(calls).toHaveLength(4);
    });
});
