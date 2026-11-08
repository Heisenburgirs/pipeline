import { address } from '@solana/kit';
import { describe, expect, it } from 'vitest';

import {
    associatedTokenAddress,
    configPda,
    eventAuthorityPda,
    feeAccountPda,
    feeAuthorityPda,
    setupPayerPda,
    marketPda,
    programDataPda,
    registerPda,
    strategyPda,
} from '../src/index.js';
import { fixtures } from './helpers.js';

async function derive(kind: string, inputs: Record<string, string>): Promise<string> {
    const a = (name: string) => address(inputs[name]!);
    switch (kind) {
        case 'config':
            return configPda();
        case 'eventAuthority':
            return eventAuthorityPda();
        case 'strategy':
            return strategyPda(a('strategyProgram'));
        case 'market':
            return marketPda(a('strategyProgram'), a('marketState'));
        case 'feeAuthority':
            return feeAuthorityPda(a('market'));
        case 'setupPayer':
            return setupPayerPda(a('market'));
        case 'feeAccount':
            return feeAccountPda(a('market'), a('mint'), a('tokenProgram'));
        case 'register':
            return registerPda(a('strategyProgram'));
        case 'programData':
            return programDataPda(a('program'));
        case 'associatedToken':
            return associatedTokenAddress(a('owner'), a('mint'), a('tokenProgram'));
        default:
            throw new Error(`unknown PDA kind ${kind}`);
    }
}

describe('PDAs', () => {
    it('cover every kind in the fixtures', () => {
        expect(new Set(fixtures.pdas.map((p) => p.kind)).size).toBe(10);
    });

    for (const [i, pda] of fixtures.pdas.entries()) {
        it(`${pda.kind} #${i} matches Rust`, async () => {
            expect(await derive(pda.kind, pda.inputs)).toBe(pda.address);
        });
    }
});
