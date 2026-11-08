// Decodes events emitted by the Pipeline router, as the Rust crate `pipeline-events`.
//
// The router emits every event as a self-CPI (Anchor `emit_cpi!`), so events survive log
// truncation. In a transaction's inner instructions, an event is an instruction to the
// router program whose data is:
//
//     EVENT_IX_TAG (8 bytes) | event discriminator (8 bytes) | Borsh-encoded event
//
// Only the router's event authority PDA can sign such an instruction, so any successful
// inner instruction to the router with this prefix is a genuine router event.

import {
    getAddressCodec,
    getBase58Encoder,
    getStructCodec,
    getU64Codec,
    getI64Codec,
    getU8Codec,
    type Address,
    type Codec,
    type ReadonlyUint8Array,
} from '@solana/kit';

import {
    decodeExact,
    getBorshBoolCodec,
    getBorshOptionCodec,
    getBorshStringCodec,
    getBorshVecCodec,
    startsWith,
} from './codecs.js';
import { EVENT_DISCRIMINATORS, EVENT_IX_TAG, PIPELINE_PROGRAM_ADDRESS } from './constants.js';

/** One trade through Pipeline. Amounts are in the smallest units of their mints. */
export type SwapEvent = {
    market: Address;
    strategy: Address;
    trader: Address;
    inputMint: Address;
    outputMint: Address;
    /** 0 = exact-in, 1 = exact-out. */
    mode: number;
    /** Input the trader parted with, excluding fees. */
    netSent: bigint;
    /** Output that arrived in the trader's output account. */
    received: bigint;
    pipelineFee: bigint;
    integrator: Address | null;
    integratorFee: bigint;
    slot: bigint;
    unixTimestamp: bigint;
};

export type StrategyRegistered = {
    strategy: Address;
    feePaid: bigint;
    metadataUri: string;
};

export type MetadataUpdated = {
    strategy: Address;
    metadataUri: string;
};

export type MarketCreated = {
    market: Address;
    strategy: Address;
    marketState: Address;
    mints: Address[];
    createdBy: Address;
};

export type FeesSwept = {
    market: Address;
    mint: Address;
    amount: bigint;
};

export type ConfigChanged = {
    admin: Address;
    paused: boolean;
    listingFeeLamports: bigint;
    listingFeeRecipient: Address;
    treasuryOwner: Address;
};

/** A decoded router event, tagged by `type`. */
export type PipelineEvent =
    | { type: 'Swap'; data: SwapEvent }
    | { type: 'StrategyRegistered'; data: StrategyRegistered }
    | { type: 'MetadataUpdated'; data: MetadataUpdated }
    | { type: 'MarketCreated'; data: MarketCreated }
    | { type: 'FeesSwept'; data: FeesSwept }
    | { type: 'ConfigChanged'; data: ConfigChanged };

export type PipelineEventType = PipelineEvent['type'];

const pubkey = getAddressCodec;

export function getSwapEventCodec(): Codec<SwapEvent> {
    return getStructCodec([
        ['market', pubkey()],
        ['strategy', pubkey()],
        ['trader', pubkey()],
        ['inputMint', pubkey()],
        ['outputMint', pubkey()],
        ['mode', getU8Codec()],
        ['netSent', getU64Codec()],
        ['received', getU64Codec()],
        ['pipelineFee', getU64Codec()],
        ['integrator', getBorshOptionCodec(pubkey())],
        ['integratorFee', getU64Codec()],
        ['slot', getU64Codec()],
        ['unixTimestamp', getI64Codec()],
    ]);
}

export function getStrategyRegisteredCodec(): Codec<StrategyRegistered> {
    return getStructCodec([
        ['strategy', pubkey()],
        ['feePaid', getU64Codec()],
        ['metadataUri', getBorshStringCodec()],
    ]);
}

export function getMetadataUpdatedCodec(): Codec<MetadataUpdated> {
    return getStructCodec([
        ['strategy', pubkey()],
        ['metadataUri', getBorshStringCodec()],
    ]);
}

export function getMarketCreatedCodec(): Codec<MarketCreated> {
    return getStructCodec([
        ['market', pubkey()],
        ['strategy', pubkey()],
        ['marketState', pubkey()],
        ['mints', getBorshVecCodec(pubkey())],
        ['createdBy', pubkey()],
    ]);
}

export function getFeesSweptCodec(): Codec<FeesSwept> {
    return getStructCodec([
        ['market', pubkey()],
        ['mint', pubkey()],
        ['amount', getU64Codec()],
    ]);
}

export function getConfigChangedCodec(): Codec<ConfigChanged> {
    return getStructCodec([
        ['admin', pubkey()],
        ['paused', getBorshBoolCodec()],
        ['listingFeeLamports', getU64Codec()],
        ['listingFeeRecipient', pubkey()],
        ['treasuryOwner', pubkey()],
    ]);
}

type EventData<T extends PipelineEventType> = Extract<PipelineEvent, { type: T }>['data'];

const EVENT_CODECS: { [T in PipelineEventType]: () => Codec<EventData<T>> } = {
    Swap: getSwapEventCodec,
    StrategyRegistered: getStrategyRegisteredCodec,
    MetadataUpdated: getMetadataUpdatedCodec,
    MarketCreated: getMarketCreatedCodec,
    FeesSwept: getFeesSweptCodec,
    ConfigChanged: getConfigChangedCodec,
};

const EVENT_TYPES = Object.keys(EVENT_DISCRIMINATORS) as PipelineEventType[];

/**
 * Decodes the data of a router self-CPI event instruction. Returns `null` for any other
 * instruction data, including events from newer router versions this SDK does not know.
 */
export function decodeEvent(data: ReadonlyUint8Array): PipelineEvent | null {
    if (!startsWith(data, EVENT_IX_TAG)) {
        return null;
    }
    const rest = data.subarray(EVENT_IX_TAG.length);
    for (const type of EVENT_TYPES) {
        const discriminator = EVENT_DISCRIMINATORS[type];
        if (startsWith(rest, discriminator)) {
            const body = rest.subarray(discriminator.length);
            // The codec table is keyed by type, so the decoded data matches `type`.
            const decoded = decodeExact(EVENT_CODECS[type]() as Codec<unknown>, body);
            return decoded === null ? null : ({ type, data: decoded } as PipelineEvent);
        }
    }
    return null;
}

/** Encodes an event as the data of the router's self-CPI instruction. */
export function encodeEvent(event: PipelineEvent): Uint8Array {
    const codec = EVENT_CODECS[event.type]() as Codec<PipelineEvent['data']>;
    const body = codec.encode(event.data);
    const discriminator = EVENT_DISCRIMINATORS[event.type];
    const data = new Uint8Array(EVENT_IX_TAG.length + discriminator.length + body.length);
    data.set(EVENT_IX_TAG);
    data.set(discriminator, EVENT_IX_TAG.length);
    data.set(body, EVENT_IX_TAG.length + discriminator.length);
    return data;
}

/** An instruction whose program is known by address. */
export type ResolvedInstruction = {
    programAddress: Address;
    data: ReadonlyUint8Array;
};

/** An instruction as returned by `getTransaction` with `json` encoding. */
export type CompiledInstruction = {
    programIdIndex: number;
    /** Base58 string (RPC `json` encoding) or raw bytes. */
    data: string | ReadonlyUint8Array;
};

/** Decodes every router event among `instructions`, in order. */
export function extractEvents(instructions: Iterable<ResolvedInstruction>): PipelineEvent[] {
    const events: PipelineEvent[] = [];
    for (const instruction of instructions) {
        if (instruction.programAddress !== PIPELINE_PROGRAM_ADDRESS) {
            continue;
        }
        const event = decodeEvent(instruction.data);
        if (event !== null) {
            events.push(event);
        }
    }
    return events;
}

const base58 = getBase58Encoder();

/**
 * Decodes every router event in a transaction's inner instructions (from `getTransaction`
 * with `json` encoding), in order.
 *
 * `accountKeys` must be the transaction's full account list: the message's static keys,
 * then `meta.loadedAddresses.writable`, then `meta.loadedAddresses.readonly`.
 */
export function extractEventsFromInnerInstructions(
    accountKeys: readonly Address[],
    innerInstructions: readonly { instructions: readonly CompiledInstruction[] }[],
): PipelineEvent[] {
    const resolved: ResolvedInstruction[] = [];
    for (const group of innerInstructions) {
        for (const instruction of group.instructions) {
            const programAddress = accountKeys[instruction.programIdIndex];
            if (programAddress === undefined) {
                throw new RangeError(`program index ${instruction.programIdIndex} is outside the account keys`);
            }
            if (programAddress !== PIPELINE_PROGRAM_ADDRESS) {
                continue;
            }
            const data = typeof instruction.data === 'string' ? base58.encode(instruction.data) : instruction.data;
            resolved.push({ programAddress, data });
        }
    }
    return extractEvents(resolved);
}
