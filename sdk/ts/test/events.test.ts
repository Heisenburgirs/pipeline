import { getBase58Decoder } from '@solana/kit';
import { describe, expect, it } from 'vitest';

import {
    decodeEvent,
    encodeEvent,
    extractEvents,
    extractEventsFromInnerInstructions,
    PIPELINE_PROGRAM_ADDRESS,
    TOKEN_PROGRAM_ADDRESS,
} from '../src/index.js';
import { fixtures, hex, key, toJson, unhex } from './helpers.js';

describe('events match Rust', () => {
    it('cover every event type', () => {
        expect(new Set(fixtures.events.map((e) => e.type)).size).toBe(6);
    });

    for (const [n, fixture] of fixtures.events.entries()) {
        it(`${fixture.type} #${n} decodes to the Rust value and re-encodes to the Rust bytes`, () => {
            const event = decodeEvent(unhex(fixture.hex));
            expect(event?.type).toBe(fixture.type);
            expect(toJson(event?.data)).toEqual(fixture.value);
            expect(hex(encodeEvent(event!))).toBe(fixture.hex);
        });
    }
});

describe('decodeEvent rejects other data', () => {
    const swap = unhex(fixtures.events[0]!.hex);

    it('without the event tag', () => {
        expect(decodeEvent(swap.subarray(1))).toBeNull();
        expect(decodeEvent(new Uint8Array())).toBeNull();
    });

    it('with an unknown discriminator', () => {
        const unknown = Uint8Array.from(swap);
        unknown[8]! ^= 0xff;
        expect(decodeEvent(unknown)).toBeNull();
    });

    it('with trailing or missing bytes', () => {
        expect(decodeEvent(new Uint8Array([...swap, 0]))).toBeNull();
        expect(decodeEvent(swap.subarray(0, swap.length - 1))).toBeNull();
    });
});

describe('extracting events from transactions', () => {
    const swap = unhex(fixtures.events[0]!.hex);
    const swept = unhex(fixtures.events.find((e) => e.type === 'FeesSwept')!.hex);

    it('keeps only router event instructions, in order', () => {
        const events = extractEvents([
            { programAddress: PIPELINE_PROGRAM_ADDRESS, data: swap },
            // Same bytes from another program are not router events.
            { programAddress: TOKEN_PROGRAM_ADDRESS, data: swap },
            // A router instruction that is not an event.
            { programAddress: PIPELINE_PROGRAM_ADDRESS, data: new Uint8Array([1, 2, 3]) },
            { programAddress: PIPELINE_PROGRAM_ADDRESS, data: swept },
        ]);
        expect(events.map((e) => e.type)).toEqual(['Swap', 'FeesSwept']);
    });

    it('resolves program indices and base58 data from getTransaction', () => {
        const base58 = getBase58Decoder();
        const accountKeys = [key(30), PIPELINE_PROGRAM_ADDRESS, TOKEN_PROGRAM_ADDRESS];
        const events = extractEventsFromInnerInstructions(accountKeys, [
            {
                instructions: [
                    { programIdIndex: 2, data: base58.decode(swap) },
                    { programIdIndex: 1, data: base58.decode(swap) },
                ],
            },
            { instructions: [{ programIdIndex: 1, data: swept }] },
        ]);
        expect(events.map((e) => e.type)).toEqual(['Swap', 'FeesSwept']);
        expect(events[0]!.data).toMatchObject({ trader: key(30), netSent: 1_000_000n });
    });

    it('rejects a program index outside the account keys', () => {
        expect(() =>
            extractEventsFromInnerInstructions([key(1)], [{ instructions: [{ programIdIndex: 1, data: '' }] }]),
        ).toThrow(RangeError);
    });
});
