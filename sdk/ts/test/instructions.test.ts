import { address } from '@solana/kit';
import { describe, expect, it } from 'vitest';

import {
    buildQuoteInstruction,
    buildResolveAccountsInstruction,
    createMarket,
    initFeeAccount,
    initializeConfig,
    registerExistingMarket,
    registerStrategy,
    swap,
    sweepFees,
    updateConfig,
    updateMetadata,
    type MarketView,
    type PipelineInstruction,
    type TradeRequest,
} from '../src/index.js';
import {
    expected,
    fixtures,
    instructionJson,
    optionalAddress,
    optionalBigint,
    toMetas,
    unhex,
    type FixtureInstruction,
} from './helpers.js';

/** Calls the SDK builder for a fixture, converting its JSON inputs. */
function build(fixture: FixtureInstruction): Promise<PipelineInstruction> {
    const i = fixture.inputs;
    switch (fixture.name) {
        case 'initializeConfig':
            return initializeConfig({
                admin: address(i.admin),
                args: {
                    listingFeeLamports: BigInt(i.args.listingFeeLamports),
                    listingFeeRecipient: address(i.args.listingFeeRecipient),
                    treasuryOwner: address(i.args.treasuryOwner),
                },
            });
        case 'updateConfig':
            return updateConfig({
                admin: address(i.admin),
                args: {
                    admin: optionalAddress(i.args.admin),
                    paused: i.args.paused,
                    listingFeeLamports: optionalBigint(i.args.listingFeeLamports),
                    listingFeeRecipient: optionalAddress(i.args.listingFeeRecipient),
                    treasuryOwner: optionalAddress(i.args.treasuryOwner),
                },
            });
        case 'registerStrategy':
            return registerStrategy({
                payer: address(i.payer),
                registrar: address(i.registrar),
                strategyProgram: address(i.strategyProgram),
                listingFeeRecipient: address(i.listingFeeRecipient),
                metadataUri: i.metadataUri,
            });
        case 'updateMetadata':
            return updateMetadata({
                registrar: address(i.registrar),
                strategyProgram: address(i.strategyProgram),
                metadataUri: i.metadataUri,
            });
        case 'createMarket':
            return createMarket({
                creator: address(i.creator),
                strategyProgram: address(i.strategyProgram),
                marketState: address(i.marketState),
                marketStateSigns: i.marketStateSigns,
                initData: unhex(i.initData),
                maxCreatorLamports: BigInt(i.maxCreatorLamports),
                strategyAccounts: toMetas(i.strategyAccounts),
            });
        case 'registerExistingMarket':
            return registerExistingMarket({
                payer: address(i.payer),
                strategyProgram: address(i.strategyProgram),
                marketState: address(i.marketState),
                infoAccounts: toMetas(i.infoAccounts),
            });
        case 'initFeeAccount':
            return initFeeAccount({
                payer: address(i.payer),
                market: address(i.market),
                mint: address(i.mint),
                tokenProgram: address(i.tokenProgram),
            });
        case 'swap': {
            const a = i.accounts;
            return swap(
                {
                    trader: address(a.trader),
                    strategyProgram: address(a.strategyProgram),
                    marketState: address(a.marketState),
                    traderInput: address(a.traderInput),
                    traderOutput: address(a.traderOutput),
                    inputVault: address(a.inputVault),
                    inputMint: address(a.inputMint),
                    outputMint: address(a.outputMint),
                    inputTokenProgram: address(a.inputTokenProgram),
                    outputTokenProgram: address(a.outputTokenProgram),
                    integratorFeeAccount: optionalAddress(a.integratorFeeAccount),
                    strategyAccounts: toMetas(a.strategyAccounts),
                    inputHookAccounts: toMetas(a.inputHookAccounts),
                },
                {
                    mode: i.args.mode,
                    amountIn: BigInt(i.args.amountIn),
                    amountOut: BigInt(i.args.amountOut),
                    minAmountOut: BigInt(i.args.minAmountOut),
                    integratorFeeBps: i.args.integratorFeeBps,
                    // Deliberately wrong: the builder must overwrite it from the accounts.
                    inputHookAccounts: i.args.inputHookAccounts,
                    extra: unhex(i.args.extra),
                },
            );
        }
        case 'sweepFees':
            return sweepFees({
                market: address(i.market),
                mint: address(i.mint),
                tokenProgram: address(i.tokenProgram),
                treasuryOwner: address(i.treasuryOwner),
                hookAccounts: toMetas(i.hookAccounts),
            });
        default:
            throw new Error(`no builder for ${fixture.name}`);
    }
}

describe('router instruction builders match Rust', () => {
    it('cover every builder', () => {
        expect(new Set(fixtures.instructions.map((f) => f.name)).size).toBe(9);
    });

    for (const [n, fixture] of fixtures.instructions.entries()) {
        it(`${fixture.name} #${n}`, async () => {
            expect(instructionJson(await build(fixture))).toEqual(expected(fixture));
        });
    }

    it('swap sets inputHookAccounts from the accounts, whatever the args say', async () => {
        const fixture = fixtures.instructions.find(
            (f) => f.name === 'swap' && f.inputs.accounts.inputHookAccounts.length > 0,
        )!;
        const instruction = await build(fixture);
        // Data: discriminator (8), mode (1), three u64s (24), u16 (2), then the hook count.
        expect(instruction.data[8 + 1 + 24 + 2]).toBe(fixture.inputs.accounts.inputHookAccounts.length);
    });
});

function marketView(json: any): MarketView {
    return {
        market: address(json.market),
        strategyProgram: address(json.strategyProgram),
        marketState: address(json.marketState),
        mints: json.mints.map(address),
        vaults: json.vaults.map(address),
        tokenPrograms: json.tokenPrograms.map(address),
        supportsExactOut: json.supportsExactOut,
    };
}

function trade(json: any): TradeRequest {
    return {
        mode: json.mode,
        inputIndex: json.inputIndex,
        outputIndex: json.outputIndex,
        amount: BigInt(json.amount),
        trader: optionalAddress(json.trader),
        extra: unhex(json.extra),
    };
}

describe('strategy read instructions match pipeline-quoter', () => {
    for (const [n, fixture] of fixtures.strategyInstructions.entries()) {
        it(`${fixture.name} #${n}`, async () => {
            const i = fixture.inputs;
            const instruction =
                fixture.name === 'quote'
                    ? await buildQuoteInstruction(marketView(i.market), trade(i.trade), toMetas(i.strategyAccounts))
                    : await buildResolveAccountsInstruction(
                          marketView(i.market),
                          trade(i.trade),
                          i.round,
                          toMetas(i.known),
                      );
            expect(instructionJson(instruction)).toEqual(expected(fixture));
        });
    }

    it('reject trades whose indices are outside the market', async () => {
        const fixture = fixtures.strategyInstructions[0]!;
        const bad = { ...trade(fixture.inputs.trade), inputIndex: 3 };
        await expect(buildQuoteInstruction(marketView(fixture.inputs.market), bad, [])).rejects.toThrow(RangeError);
    });
});
