import { describe, expect, it } from 'vitest';

import { amountForBudget, ceilBps, maxTotalSpend, pipelineFee, U64_MAX } from '../src/index.js';
import { fixtures, optionalBigint } from './helpers.js';

describe('fees match Rust', () => {
    it('ceilBps', () => {
        for (const c of fixtures.fees.ceilBps) {
            expect(ceilBps(BigInt(c.amount), BigInt(c.bps)), `${c.amount} × ${c.bps}`).toBe(optionalBigint(c.result));
        }
    });

    it('pipelineFee', () => {
        for (const c of fixtures.fees.pipelineFee) {
            expect(pipelineFee(BigInt(c.netSent)), c.netSent).toBe(BigInt(c.result));
        }
    });

    it('maxTotalSpend', () => {
        for (const c of fixtures.fees.maxTotalSpend) {
            expect(
                maxTotalSpend(BigInt(c.amountSent), c.integratorFeeBps),
                `${c.amountSent} @ ${c.integratorFeeBps}`,
            ).toBe(optionalBigint(c.result));
        }
    });

    it('amountForBudget', () => {
        for (const c of fixtures.fees.amountForBudget) {
            expect(amountForBudget(BigInt(c.budget), c.integratorFeeBps), `${c.budget} @ ${c.integratorFeeBps}`).toBe(
                BigInt(c.result),
            );
        }
    });
});

describe('fee properties', () => {
    it('round up, so a non-zero amount always pays', () => {
        expect(pipelineFee(0n)).toBe(0n);
        expect(pipelineFee(1n)).toBe(1n);
        expect(pipelineFee(400n)).toBe(1n);
        expect(pipelineFee(401n)).toBe(2n);
        expect(pipelineFee(U64_MAX)).toBe(46_116_860_184_273_880n);
    });

    it('amountForBudget is the largest amount within budget', () => {
        for (const budget of [0n, 1n, 2n, 999n, 1_000n, 1_000_000n, 123_456_789n, U64_MAX / 2n]) {
            for (const bps of [0, 1, 50, 100, 9_999]) {
                const amount = amountForBudget(budget, bps);
                expect(maxTotalSpend(amount, bps)! <= budget).toBe(true);
                const next = maxTotalSpend(amount + 1n, bps);
                if (next !== null) {
                    expect(next > budget).toBe(true);
                }
            }
        }
    });

    it('reject values outside u64 and u16', () => {
        expect(() => ceilBps(-1n, 25n)).toThrow(RangeError);
        expect(() => ceilBps(U64_MAX + 1n, 25n)).toThrow(RangeError);
        expect(() => maxTotalSpend(1n, 65_536)).toThrow(RangeError);
        expect(() => amountForBudget(1n, 1.5)).toThrow(RangeError);
    });
});
