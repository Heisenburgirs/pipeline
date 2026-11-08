// Fee maths, identical to `pipeline_strategy_interface::fees`. All fees round up, so a
// non-zero amount always pays at least one unit. Amounts are u64 values held in `bigint`.

import { BPS_DENOMINATOR, PIPELINE_FEE_BPS } from './constants.js';

export const U64_MAX = (1n << 64n) - 1n;
const U16_MAX = 0xffff;

function assertU64(name: string, value: bigint): void {
    if (value < 0n || value > U64_MAX) {
        throw new RangeError(`${name} must be a u64, got ${value}`);
    }
}

function assertU16(name: string, value: number): void {
    if (!Number.isInteger(value) || value < 0 || value > U16_MAX) {
        throw new RangeError(`${name} must be a u16, got ${value}`);
    }
}

/** `ceil(amount × bps / 10,000)`, or `null` if the result does not fit in a u64. */
export function ceilBps(amount: bigint, bps: bigint): bigint | null {
    assertU64('amount', amount);
    assertU64('bps', bps);
    const fee = (amount * bps + BPS_DENOMINATOR - 1n) / BPS_DENOMINATOR;
    return fee > U64_MAX ? null : fee;
}

/** Pipeline's fee on the input a trader actually parted with. */
export function pipelineFee(netSent: bigint): bigint {
    // 25 bps of any u64 fits in a u64.
    return ceilBps(netSent, PIPELINE_FEE_BPS)!;
}

/**
 * The most a trader can spend on a swap that sends `amountSent` to the strategy: the
 * amount plus Pipeline's and the integrator's fees on all of it. `null` on u64 overflow.
 */
export function maxTotalSpend(amountSent: bigint, integratorFeeBps: number): bigint | null {
    assertU16('integratorFeeBps', integratorFeeBps);
    const integrator = ceilBps(amountSent, BigInt(integratorFeeBps));
    if (integrator === null) {
        return null;
    }
    const total = amountSent + pipelineFee(amountSent) + integrator;
    return total > U64_MAX ? null : total;
}

/**
 * The largest amount to send to the strategy so that the total spend, including both
 * fees, stays within `budget`.
 */
export function amountForBudget(budget: bigint, integratorFeeBps: number): bigint {
    assertU64('budget', budget);
    assertU16('integratorFeeBps', integratorFeeBps);
    const rate = BPS_DENOMINATOR + PIPELINE_FEE_BPS + BigInt(integratorFeeBps);
    // Start from the exact-rate estimate, then step down past rounding.
    let amount = (budget * BPS_DENOMINATOR) / rate;
    while (amount > 0n) {
        const total = maxTotalSpend(amount, integratorFeeBps);
        if (total !== null && total <= budget) {
            break;
        }
        amount -= 1n;
    }
    return amount;
}
