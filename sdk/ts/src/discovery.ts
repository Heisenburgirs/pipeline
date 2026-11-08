// Lists router accounts through `getProgramAccounts`, filtered by Anchor account
// discriminator. Thin wrappers: no caching, no pagination.

import {
    getBase58Decoder,
    getBase64Encoder,
    type Address,
    type Base58EncodedBytes,
    type Commitment,
    type Decoder,
    type GetProgramAccountsApi,
    type GetProgramAccountsMemcmpFilter,
    type ReadonlyUint8Array,
    type Rpc,
} from '@solana/kit';

import {
    decodeAccount,
    getMarketCodec,
    getStrategyCodec,
    type Market,
    type RouterAccountName,
    type Strategy,
} from './codecs.js';
import { ACCOUNT_DISCRIMINATORS, PIPELINE_PROGRAM_ADDRESS } from './constants.js';

/** A decoded router account and its address. */
export type DecodedAccount<T> = {
    address: Address;
    account: T;
};

export type FetchOptions = {
    commitment?: Commitment;
};

const base58 = getBase58Decoder();
const base64 = getBase64Encoder();

function memcmp(offset: bigint, bytes: ReadonlyUint8Array | Address): GetProgramAccountsMemcmpFilter {
    // An address is already the base58 encoding of its 32 bytes.
    const encoded = typeof bytes === 'string' ? bytes : base58.decode(bytes);
    return {
        memcmp: {
            offset,
            bytes: encoded as Base58EncodedBytes,
            encoding: 'base58',
        },
    };
}

async function fetchRouterAccounts<T>(
    rpc: Rpc<GetProgramAccountsApi>,
    name: RouterAccountName,
    decoder: Decoder<T>,
    extraFilters: GetProgramAccountsMemcmpFilter[],
    options: FetchOptions,
): Promise<DecodedAccount<T>[]> {
    const accounts = await rpc
        .getProgramAccounts(PIPELINE_PROGRAM_ADDRESS, {
            commitment: options.commitment,
            encoding: 'base64',
            filters: [memcmp(0n, ACCOUNT_DISCRIMINATORS[name]), ...extraFilters],
        })
        .send();
    const decoded: DecodedAccount<T>[] = [];
    for (const { pubkey, account } of accounts) {
        const value = decodeAccount(name, decoder, base64.encode(account.data[0]));
        // Accounts that fail to decode (for example from a newer layout) are skipped.
        if (value !== null) {
            decoded.push({ address: pubkey, account: value });
        }
    }
    return decoded;
}

/** Every Pipeline market, optionally only those of one strategy program. */
export function fetchMarkets(
    rpc: Rpc<GetProgramAccountsApi>,
    options: FetchOptions & { strategyProgram?: Address } = {},
): Promise<DecodedAccount<Market>[]> {
    // `Market.strategy_program` is the first field, right after the discriminator.
    const filters = options.strategyProgram ? [memcmp(8n, options.strategyProgram)] : [];
    return fetchRouterAccounts(rpc, 'Market', getMarketCodec(), filters, options);
}

/** Every registered strategy. */
export function fetchStrategies(
    rpc: Rpc<GetProgramAccountsApi>,
    options: FetchOptions = {},
): Promise<DecodedAccount<Strategy>[]> {
    return fetchRouterAccounts(rpc, 'Strategy', getStrategyCodec(), [], options);
}
