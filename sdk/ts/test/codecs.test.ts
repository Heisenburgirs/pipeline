import type { Codec } from '@solana/kit';
import { describe, expect, it } from 'vitest';

import {
    decodeConfig,
    decodeExact,
    decodeMarket,
    decodeReturnData,
    decodeStrategy,
    encodeAccount,
    getAccountRefCodec,
    getConfigArgsCodec,
    getConfigCodec,
    getMarketCodec,
    getMarketInfoCodec,
    getQuoteResultCodec,
    getResolveRequestCodec,
    getResolveResultCodec,
    getStrategyCodec,
    getSwapArgsCodec,
    getSwapRequestCodec,
    getSwapResultCodec,
    getTradeRequestCodec,
    getUpdateConfigArgsCodec,
    instructionData,
    marketMints,
    marketVaultFor,
    parseInstruction,
    STRATEGY_DISCRIMINATORS,
    TradeMode,
    type Market,
} from '../src/index.js';
import { fixtures, hex, key, toJson, unhex } from './helpers.js';

const CODECS: Record<string, () => Codec<any>> = {
    TradeRequest: getTradeRequestCodec,
    SwapRequest: getSwapRequestCodec,
    QuoteResult: getQuoteResultCodec,
    MarketInfo: getMarketInfoCodec,
    ResolveRequest: getResolveRequestCodec,
    AccountRef: getAccountRefCodec,
    ResolveResult: getResolveResultCodec,
    ConfigArgs: getConfigArgsCodec,
    UpdateConfigArgs: getUpdateConfigArgsCodec,
    SwapArgs: getSwapArgsCodec,
    SwapResult: getSwapResultCodec,
};

describe('wire types match Rust Borsh', () => {
    it('cover every type', () => {
        expect(new Set(fixtures.encodings.map((e) => e.type))).toEqual(new Set(Object.keys(CODECS)));
    });

    for (const [i, fixture] of fixtures.encodings.entries()) {
        it(`${fixture.type} #${i} decodes to the Rust value and re-encodes to the Rust bytes`, () => {
            const codec = CODECS[fixture.type]!();
            const decoded = decodeExact(codec, unhex(fixture.hex));
            expect(toJson(decoded)).toEqual(fixture.value);
            expect(hex(codec.encode(decoded))).toBe(fixture.hex);
        });
    }

    it('encode a hand-built value to the Rust bytes', () => {
        const fixture = fixtures.encodings.find((e) => e.type === 'TradeRequest' && e.value.trader !== null)!;
        const bytes = getTradeRequestCodec().encode({
            mode: TradeMode.ExactOut,
            inputIndex: 3,
            outputIndex: 2,
            amount: 2n ** 64n - 1n,
            trader: key(30),
            extra: new Uint8Array([0, 1, 2, 254, 255]),
        });
        expect(hex(bytes)).toBe(fixture.hex);
    });
});

describe('strict Borsh decoding', () => {
    const quote = unhex(fixtures.encodings.find((e) => e.type === 'QuoteResult')!.hex);
    const resolve = unhex(fixtures.encodings.find((e) => e.type === 'ResolveResult')!.hex);
    const update = unhex(fixtures.encodings.find((e) => e.type === 'UpdateConfigArgs')!.hex);

    it('rejects trailing and missing bytes', () => {
        expect(decodeExact(getQuoteResultCodec(), new Uint8Array([...quote, 0]))).toBeNull();
        expect(decodeExact(getQuoteResultCodec(), quote.subarray(1))).toBeNull();
    });

    it('rejects bools other than 0 and 1', () => {
        const bad = Uint8Array.from(resolve);
        bad[bad.length - 1] = 2;
        expect(decodeExact(getResolveResultCodec(), bad)).toBeNull();
    });

    it('rejects option tags other than 0 and 1', () => {
        const bad = Uint8Array.from(update);
        bad[0] = 2;
        expect(decodeExact(getUpdateConfigArgsCodec(), bad)).toBeNull();
    });

    it('rejects unknown trade modes', () => {
        expect(
            decodeExact(getTradeRequestCodec(), new Uint8Array([2, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])),
        ).toBeNull();
    });

    it('rejects invalid UTF-8 in strings', () => {
        const strategy = fixtures.accounts.find((e) => e.type === 'Strategy')!;
        const bad = unhex(strategy.hex);
        // The URI starts after the discriminator, program id and u32 length.
        bad[8 + 32 + 4] = 0xff;
        expect(decodeStrategy(bad)).toBeNull();
    });
});

describe('strategy instruction data', () => {
    it('round-trips and rejects the wrong discriminator', () => {
        const request = {
            trade: {
                mode: TradeMode.ExactOut,
                inputIndex: 1,
                outputIndex: 0,
                amount: 42n,
                trader: key(7),
                extra: new Uint8Array([1, 2, 3]),
            },
            amountSent: 100n,
            minAmountOut: 40n,
        };
        const data = instructionData(STRATEGY_DISCRIMINATORS.swap, getSwapRequestCodec(), request);
        expect(toJson(parseInstruction(STRATEGY_DISCRIMINATORS.swap, getSwapRequestCodec(), data))).toEqual(
            toJson(request),
        );
        expect(parseInstruction(STRATEGY_DISCRIMINATORS.quote, getSwapRequestCodec(), data)).toBeNull();
    });
});

describe('return data', () => {
    it('restores trailing zero bytes stripped by the runtime', () => {
        // { amountIn: 5, amountOut: 0 } is recorded as the single byte 0x05.
        expect(decodeReturnData(getQuoteResultCodec(), new Uint8Array([5]))).toEqual({ amountIn: 5n, amountOut: 0n });
        // { accounts: [], complete: false } is all zeros and recorded as nothing.
        expect(decodeReturnData(getResolveResultCodec(), new Uint8Array())).toEqual({ accounts: [], complete: false });
    });

    it('decodes untruncated data unchanged', () => {
        const fixture = fixtures.encodings.find((e) => e.type === 'ResolveResult')!;
        expect(toJson(decodeReturnData(getResolveResultCodec(), unhex(fixture.hex)))).toEqual(fixture.value);
    });
});

describe('router accounts', () => {
    const decoders: Record<string, (data: Uint8Array) => unknown> = {
        Config: decodeConfig,
        Strategy: decodeStrategy,
        Market: decodeMarket,
    };
    const codecs: Record<string, () => Codec<any>> = {
        Config: getConfigCodec,
        Strategy: getStrategyCodec,
        Market: getMarketCodec,
    };

    for (const fixture of fixtures.accounts) {
        const name = fixture.type as 'Config' | 'Strategy' | 'Market';

        it(`${name} decodes to the Rust value and re-encodes to the Rust bytes`, () => {
            const decoded = decoders[name]!(unhex(fixture.hex));
            expect(toJson(decoded)).toEqual(fixture.value);
            expect(hex(encodeAccount(name, codecs[name]!(), decoded))).toBe(fixture.hex);
        });

        it(`${name} ignores Anchor's trailing allocation`, () => {
            const padded = new Uint8Array([...unhex(fixture.hex), ...new Uint8Array(64)]);
            expect(toJson(decoders[name]!(padded))).toEqual(fixture.value);
        });

        it(`${name} rejects another account's discriminator`, () => {
            const bytes = unhex(fixture.hex);
            bytes[0]! ^= 0xff;
            expect(decoders[name]!(bytes)).toBeNull();
            expect(decoders[name]!(new Uint8Array())).toBeNull();
        });
    }

    it('market helpers use only the first mintCount mints', () => {
        const market = decodeMarket(unhex(fixtures.accounts.find((e) => e.type === 'Market')!.hex)) as Market;
        expect(marketMints(market)).toEqual([key(13), key(14), key(15)]);
        expect(marketVaultFor(market, key(14))).toBe(key(17));
        expect(marketVaultFor(market, key(0))).toBeNull();
    });
});
