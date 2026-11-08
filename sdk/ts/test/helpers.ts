// Loads the cross-language fixtures printed by
// `cargo run -p pipeline-client --example ts_fixtures` and converts between their JSON
// conventions and the SDK's types.

import { readFileSync } from 'node:fs';

import { AccountRole, address, isSignerRole, isWritableRole, type AccountMeta, type Address } from '@solana/kit';

import type { PipelineInstruction } from '../src/index.js';

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export type FixtureMeta = { address: string; isSigner: boolean; isWritable: boolean };

export type FixtureInstruction = {
    name: string;
    inputs: Record<string, any>;
    programAddress: string;
    accounts: FixtureMeta[];
    data: string;
};

export type FixtureEncoding = { type: string; value: Record<string, Json>; hex: string };

export type Fixtures = {
    keys: string[];
    programIds: Record<string, string>;
    constants: Record<string, number>;
    discriminators: {
        routerInstructions: Record<string, string>;
        accounts: Record<string, string>;
        strategy: Record<string, string>;
        events: Record<string, string>;
        eventIxTag: string;
    };
    pdas: { kind: string; inputs: Record<string, string>; address: string }[];
    instructions: FixtureInstruction[];
    strategyInstructions: FixtureInstruction[];
    fees: {
        ceilBps: { amount: string; bps: string; result: string | null }[];
        pipelineFee: { netSent: string; result: string }[];
        maxTotalSpend: { amountSent: string; integratorFeeBps: number; result: string | null }[];
        amountForBudget: { budget: string; integratorFeeBps: number; result: string }[];
    };
    encodings: FixtureEncoding[];
    accounts: FixtureEncoding[];
    events: FixtureEncoding[];
};

export const fixtures: Fixtures = JSON.parse(readFileSync(new URL('./fixtures.json', import.meta.url), 'utf8'));

/** The fixture key `n`: the address whose 32 bytes all equal `n`. */
export function key(n: number): Address {
    return address(fixtures.keys[n]!);
}

export function hex(bytes: ArrayLike<number>): string {
    return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

export function unhex(text: string): Uint8Array {
    const bytes = new Uint8Array(text.length / 2);
    for (let i = 0; i < bytes.length; i++) {
        bytes[i] = parseInt(text.slice(2 * i, 2 * i + 2), 16);
    }
    return bytes;
}

/** Converts an SDK value to the fixtures' JSON conventions (bigint → string, bytes → hex). */
export function toJson(value: unknown): Json {
    if (typeof value === 'bigint') {
        return value.toString();
    }
    if (value instanceof Uint8Array) {
        return hex(value);
    }
    if (Array.isArray(value)) {
        return value.map(toJson);
    }
    if (value !== null && typeof value === 'object') {
        return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, toJson(v)]));
    }
    return value as Json;
}

export function toMeta(meta: FixtureMeta): AccountMeta {
    const role =
        (meta.isSigner ? AccountRole.READONLY_SIGNER : AccountRole.READONLY) |
        (meta.isWritable ? AccountRole.WRITABLE : 0);
    return { address: address(meta.address), role: role as AccountRole };
}

export function toMetas(metas: FixtureMeta[]): AccountMeta[] {
    return metas.map(toMeta);
}

/** An SDK instruction in the fixtures' shape, for direct comparison. */
export function instructionJson(instruction: PipelineInstruction): Omit<FixtureInstruction, 'name' | 'inputs'> {
    return {
        programAddress: instruction.programAddress,
        accounts: instruction.accounts.map((meta) => ({
            address: meta.address,
            isSigner: isSignerRole(meta.role),
            isWritable: isWritableRole(meta.role),
        })),
        data: hex(instruction.data),
    };
}

export function expected(fixture: FixtureInstruction): Omit<FixtureInstruction, 'name' | 'inputs'> {
    return { programAddress: fixture.programAddress, accounts: fixture.accounts, data: fixture.data };
}

export function optionalAddress(value: string | null): Address | null {
    return value === null ? null : address(value);
}

export function optionalBigint(value: string | null): bigint | null {
    return value === null ? null : BigInt(value);
}
