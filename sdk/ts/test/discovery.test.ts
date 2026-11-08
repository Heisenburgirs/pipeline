// Discovery against a fake RPC object: checks the request filters and decoding.

import { getBase58Encoder, getBase64Decoder, type GetProgramAccountsApi, type Rpc } from '@solana/kit';
import { describe, expect, it } from 'vitest';

import { ACCOUNT_DISCRIMINATORS, fetchMarkets, fetchStrategies, PIPELINE_PROGRAM_ADDRESS } from '../src/index.js';
import { fixtures, key, toJson, unhex } from './helpers.js';

function fakeRpc(accounts: { pubkey: string; data: Uint8Array }[]) {
    const calls: { program: string; config: any }[] = [];
    const base64 = getBase64Decoder();
    const rpc = {
        getProgramAccounts(program: string, config: any) {
            calls.push({ program, config });
            return {
                send: async () =>
                    accounts.map(({ pubkey, data }) => ({
                        pubkey,
                        account: { data: [base64.decode(data), 'base64'] },
                    })),
            };
        },
    };
    return { rpc: rpc as unknown as Rpc<GetProgramAccountsApi>, calls };
}

const marketFixture = fixtures.accounts.find((e) => e.type === 'Market')!;
const strategyFixture = fixtures.accounts.find((e) => e.type === 'Strategy')!;

describe('discovery', () => {
    it('fetchMarkets filters by the Market discriminator and decodes, skipping other data', async () => {
        const { rpc, calls } = fakeRpc([
            { pubkey: key(40), data: unhex(marketFixture.hex) },
            { pubkey: key(39), data: new Uint8Array([1, 2, 3]) },
        ]);
        const markets = await fetchMarkets(rpc);
        expect(markets).toHaveLength(1);
        expect(markets[0]!.address).toBe(key(40));
        expect(toJson(markets[0]!.account)).toEqual(marketFixture.value);

        expect(calls[0]!.program).toBe(PIPELINE_PROGRAM_ADDRESS);
        expect(calls[0]!.config.encoding).toBe('base64');
        const [filter] = calls[0]!.config.filters;
        expect(filter.memcmp.offset).toBe(0n);
        expect(filter.memcmp.encoding).toBe('base58');
        expect(Array.from(getBase58Encoder().encode(filter.memcmp.bytes))).toEqual(
            Array.from(ACCOUNT_DISCRIMINATORS.Market),
        );
    });

    it('fetchMarkets can filter by strategy program', async () => {
        const { rpc, calls } = fakeRpc([]);
        await fetchMarkets(rpc, { strategyProgram: key(7) });
        expect(calls[0]!.config.filters[1]).toEqual({ memcmp: { offset: 8n, bytes: key(7), encoding: 'base58' } });
    });

    it('fetchStrategies filters by the Strategy discriminator and decodes', async () => {
        const { rpc, calls } = fakeRpc([{ pubkey: key(7), data: unhex(strategyFixture.hex) }]);
        const strategies = await fetchStrategies(rpc);
        expect(toJson(strategies[0]!.account)).toEqual(strategyFixture.value);
        const [filter] = calls[0]!.config.filters;
        expect(Array.from(getBase58Encoder().encode(filter.memcmp.bytes))).toEqual(
            Array.from(ACCOUNT_DISCRIMINATORS.Strategy),
        );
    });
});
