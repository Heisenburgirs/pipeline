import { createHash } from 'node:crypto';

import { getAddressEncoder } from '@solana/kit';
import { describe, expect, it } from 'vitest';

import {
    ACCOUNT_DISCRIMINATORS,
    ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
    BPF_LOADER_UPGRADEABLE_PROGRAM_ADDRESS,
    BPS_DENOMINATOR,
    EVENT_DISCRIMINATORS,
    EVENT_IX_TAG,
    INTERFACE_VERSION,
    MAX_MARKET_MINTS,
    MAX_RESOLVE_ROUNDS,
    MIN_MARKET_MINTS,
    PIPELINE_FEE_BPS,
    PIPELINE_PROGRAM_ADDRESS,
    ROUTER_INSTRUCTION_DISCRIMINATORS,
    STRATEGY_DISCRIMINATORS,
    SYSTEM_PROGRAM_ADDRESS,
    TOKEN_2022_PROGRAM_ADDRESS,
    TOKEN_PROGRAM_ADDRESS,
} from '../src/index.js';
import { fixtures, hex } from './helpers.js';

function sha256Prefix(preimage: string): string {
    return createHash('sha256').update(preimage).digest('hex').slice(0, 16);
}

function snakeCase(name: string): string {
    return name.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`);
}

describe('program addresses', () => {
    it('match the Rust crate', () => {
        expect(PIPELINE_PROGRAM_ADDRESS).toBe(fixtures.programIds.router);
        expect(hex(getAddressEncoder().encode(PIPELINE_PROGRAM_ADDRESS))).toBe(fixtures.programIds.routerBytes);
        expect(SYSTEM_PROGRAM_ADDRESS).toBe(fixtures.programIds.system);
        expect(TOKEN_PROGRAM_ADDRESS).toBe(fixtures.programIds.token);
        expect(TOKEN_2022_PROGRAM_ADDRESS).toBe(fixtures.programIds.token2022);
        expect(ASSOCIATED_TOKEN_PROGRAM_ADDRESS).toBe(fixtures.programIds.associatedToken);
        expect(BPF_LOADER_UPGRADEABLE_PROGRAM_ADDRESS).toBe(fixtures.programIds.bpfLoaderUpgradeable);
    });
});

describe('constants', () => {
    it('match the Rust crate', () => {
        expect(Number(PIPELINE_FEE_BPS)).toBe(fixtures.constants.pipelineFeeBps);
        expect(Number(BPS_DENOMINATOR)).toBe(fixtures.constants.bpsDenominator);
        expect(INTERFACE_VERSION).toBe(fixtures.constants.interfaceVersion);
        expect(MIN_MARKET_MINTS).toBe(fixtures.constants.minMarketMints);
        expect(MAX_MARKET_MINTS).toBe(fixtures.constants.maxMarketMints);
        expect(MAX_RESOLVE_ROUNDS).toBe(fixtures.constants.maxResolveRounds);
    });
});

describe('discriminators', () => {
    it('router instructions match Rust and sha256("global:<name>")', () => {
        const names = Object.keys(
            ROUTER_INSTRUCTION_DISCRIMINATORS,
        ) as (keyof typeof ROUTER_INSTRUCTION_DISCRIMINATORS)[];
        expect(names.map(snakeCase).sort()).toEqual(Object.keys(fixtures.discriminators.routerInstructions).sort());
        for (const name of names) {
            const rustName = snakeCase(name);
            expect(hex(ROUTER_INSTRUCTION_DISCRIMINATORS[name])).toBe(
                fixtures.discriminators.routerInstructions[rustName],
            );
            expect(hex(ROUTER_INSTRUCTION_DISCRIMINATORS[name])).toBe(sha256Prefix(`global:${rustName}`));
        }
    });

    it('strategy instructions match Rust and sha256("global:pipeline_<name>")', () => {
        for (const [name, bytes] of Object.entries(STRATEGY_DISCRIMINATORS)) {
            const rustName = `pipeline_${snakeCase(name)}`;
            expect(hex(bytes)).toBe(fixtures.discriminators.strategy[rustName]);
            expect(hex(bytes)).toBe(sha256Prefix(`global:${rustName}`));
        }
    });

    it('accounts match Rust and sha256("account:<Name>")', () => {
        for (const [name, bytes] of Object.entries(ACCOUNT_DISCRIMINATORS)) {
            expect(hex(bytes)).toBe(fixtures.discriminators.accounts[name]);
            expect(hex(bytes)).toBe(sha256Prefix(`account:${name}`));
        }
    });

    it('events match Rust and sha256("event:<Name>")', () => {
        expect(hex(EVENT_IX_TAG)).toBe(fixtures.discriminators.eventIxTag);
        for (const [type, bytes] of Object.entries(EVENT_DISCRIMINATORS)) {
            const name = type === 'Swap' ? 'SwapEvent' : type;
            expect(hex(bytes)).toBe(fixtures.discriminators.events[name]);
            expect(hex(bytes)).toBe(sha256Prefix(`event:${name}`));
        }
    });
});
