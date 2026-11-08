// Router instruction builders. Each mirrors the builder of the same name in the Rust crate
// `pipeline-client`: same accounts, order, signer and writable flags, and data bytes.
// Builders are async only because kit derives PDAs with WebCrypto.

import {
    AccountRole,
    getStructCodec,
    getU64Codec,
    getU8Codec,
    type AccountMeta,
    type Address,
    type Instruction,
    type InstructionWithAccounts,
    type InstructionWithData,
    type ReadonlyUint8Array,
} from '@solana/kit';

import {
    getBorshBytesCodec,
    getBorshStringCodec,
    getConfigArgsCodec,
    getSwapArgsCodec,
    getUpdateConfigArgsCodec,
    instructionData,
    type ConfigArgs,
    type SwapArgs,
    type UpdateConfigArgs,
} from './codecs.js';
import {
    ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
    INTERFACE_VERSION,
    PIPELINE_PROGRAM_ADDRESS,
    ROUTER_INSTRUCTION_DISCRIMINATORS as DISCRIMINATORS,
    SYSTEM_PROGRAM_ADDRESS,
} from './constants.js';
import {
    associatedTokenAddress,
    configPda,
    eventAuthorityPda,
    feeAccountPda,
    feeAuthorityPda,
    setupPayerPda,
    marketPda,
    programDataPda,
    strategyPda,
} from './pda.js';

/** An instruction with its accounts and data always present. */
export type PipelineInstruction = Instruction &
    InstructionWithAccounts<readonly AccountMeta[]> &
    InstructionWithData<Uint8Array>;

function readonly(address: Address): AccountMeta {
    return { address, role: AccountRole.READONLY };
}

function writable(address: Address): AccountMeta {
    return { address, role: AccountRole.WRITABLE };
}

function readonlySigner(address: Address): AccountMeta {
    return { address, role: AccountRole.READONLY_SIGNER };
}

function writableSigner(address: Address): AccountMeta {
    return { address, role: AccountRole.WRITABLE_SIGNER };
}

function routerInstruction(accounts: AccountMeta[], data: Uint8Array): PipelineInstruction {
    return { programAddress: PIPELINE_PROGRAM_ADDRESS, accounts, data };
}

function discriminatorOnly(discriminator: ReadonlyUint8Array): Uint8Array {
    return Uint8Array.from(discriminator);
}

/** The two accounts `#[event_cpi]` appends to an instruction. */
async function eventCpiAccounts(): Promise<AccountMeta[]> {
    return [readonly(await eventAuthorityPda()), readonly(PIPELINE_PROGRAM_ADDRESS)];
}

/** `initialize_config`, signed by the router's upgrade authority. */
export async function initializeConfig(input: { admin: Address; args: ConfigArgs }): Promise<PipelineInstruction> {
    return routerInstruction(
        [
            writableSigner(input.admin),
            writable(await configPda()),
            readonly(PIPELINE_PROGRAM_ADDRESS),
            readonly(await programDataPda(PIPELINE_PROGRAM_ADDRESS)),
            readonly(SYSTEM_PROGRAM_ADDRESS),
        ],
        instructionData(DISCRIMINATORS.initializeConfig, getConfigArgsCodec(), input.args),
    );
}

export async function updateConfig(input: { admin: Address; args: UpdateConfigArgs }): Promise<PipelineInstruction> {
    return routerInstruction(
        [readonlySigner(input.admin), writable(await configPda()), ...(await eventCpiAccounts())],
        instructionData(DISCRIMINATORS.updateConfig, getUpdateConfigArgsCodec(), input.args),
    );
}

const registerStrategyArgsCodec = getStructCodec([
    ['metadataUri', getBorshStringCodec()],
    ['interfaceVersion', getU8Codec()],
]);

/**
 * `register_strategy`. `registrar` is the program's upgrade authority or its registration
 * PDA ({@link registerPda}).
 */
export async function registerStrategy(input: {
    payer: Address;
    registrar: Address;
    strategyProgram: Address;
    listingFeeRecipient: Address;
    metadataUri: string;
}): Promise<PipelineInstruction> {
    return routerInstruction(
        [
            writableSigner(input.payer),
            readonlySigner(input.registrar),
            readonly(input.strategyProgram),
            readonly(await programDataPda(input.strategyProgram)),
            readonly(await configPda()),
            writable(input.listingFeeRecipient),
            writable(await strategyPda(input.strategyProgram)),
            readonly(SYSTEM_PROGRAM_ADDRESS),
            ...(await eventCpiAccounts()),
        ],
        instructionData(DISCRIMINATORS.registerStrategy, registerStrategyArgsCodec, {
            metadataUri: input.metadataUri,
            interfaceVersion: INTERFACE_VERSION,
        }),
    );
}

export async function updateMetadata(input: {
    registrar: Address;
    strategyProgram: Address;
    metadataUri: string;
}): Promise<PipelineInstruction> {
    return routerInstruction(
        [
            readonlySigner(input.registrar),
            readonly(input.strategyProgram),
            readonly(await programDataPda(input.strategyProgram)),
            writable(await strategyPda(input.strategyProgram)),
            ...(await eventCpiAccounts()),
        ],
        instructionData(DISCRIMINATORS.updateMetadata, getBorshStringCodec(), input.metadataUri),
    );
}

/**
 * `create_market`. `strategyAccounts` are the strategy's own setup accounts, passed after
 * the router's fixed accounts. `maxCreatorLamports` caps the SOL the strategy's setup may
 * take from the creator; set `marketStateSigns` if the market state is a fresh keypair
 * that signs the transaction.
 */
export async function createMarket(input: {
    creator: Address;
    strategyProgram: Address;
    marketState: Address;
    marketStateSigns?: boolean;
    initData: ReadonlyUint8Array;
    maxCreatorLamports: bigint;
    strategyAccounts?: readonly AccountMeta[];
}): Promise<PipelineInstruction> {
    const market = await marketPda(input.strategyProgram, input.marketState);
    return routerInstruction(
        [
            writableSigner(input.creator),
            readonly(await configPda()),
            writable(await strategyPda(input.strategyProgram)),
            readonly(input.strategyProgram),
            input.marketStateSigns ? writableSigner(input.marketState) : writable(input.marketState),
            writable(market),
            writable(await setupPayerPda(market)),
            readonly(SYSTEM_PROGRAM_ADDRESS),
            ...(await eventCpiAccounts()),
            ...(input.strategyAccounts ?? []),
        ],
        instructionData(DISCRIMINATORS.createMarket, createMarketArgsCodec, {
            initData: input.initData,
            maxCreatorLamports: input.maxCreatorLamports,
        }),
    );
}

const createMarketArgsCodec = getStructCodec([
    ['initData', getBorshBytesCodec()],
    ['maxCreatorLamports', getU64Codec()],
]);

/**
 * `register_existing_market`. `infoAccounts` are passed to the strategy's
 * `pipeline_market_info` after the market state.
 */
export async function registerExistingMarket(input: {
    payer: Address;
    strategyProgram: Address;
    marketState: Address;
    infoAccounts?: readonly AccountMeta[];
}): Promise<PipelineInstruction> {
    return routerInstruction(
        [
            writableSigner(input.payer),
            readonly(await configPda()),
            writable(await strategyPda(input.strategyProgram)),
            readonly(input.strategyProgram),
            readonly(input.marketState),
            writable(await marketPda(input.strategyProgram, input.marketState)),
            readonly(SYSTEM_PROGRAM_ADDRESS),
            ...(await eventCpiAccounts()),
            ...(input.infoAccounts ?? []),
        ],
        discriminatorOnly(DISCRIMINATORS.registerExistingMarket),
    );
}

export async function initFeeAccount(input: {
    payer: Address;
    market: Address;
    mint: Address;
    tokenProgram: Address;
}): Promise<PipelineInstruction> {
    return routerInstruction(
        [
            writableSigner(input.payer),
            readonly(input.market),
            readonly(await feeAuthorityPda(input.market)),
            readonly(input.mint),
            writable(await feeAccountPda(input.market, input.mint, input.tokenProgram)),
            readonly(input.tokenProgram),
            readonly(ASSOCIATED_TOKEN_PROGRAM_ADDRESS),
            readonly(SYSTEM_PROGRAM_ADDRESS),
        ],
        discriminatorOnly(DISCRIMINATORS.initFeeAccount),
    );
}

/** Everything needed to build a `swap` instruction. */
export type SwapAccounts = {
    /** Signs the swap. Never passed to the strategy as a signer. */
    trader: Address;
    strategyProgram: Address;
    marketState: Address;
    traderInput: Address;
    traderOutput: Address;
    inputVault: Address;
    inputMint: Address;
    outputMint: Address;
    inputTokenProgram: Address;
    outputTokenProgram: Address;
    /** The integrator's token account for the input mint, if an integrator fee is charged. */
    integratorFeeAccount?: Address | null;
    /** Strategy-specific accounts, as returned by `pipeline_resolve_accounts`. */
    strategyAccounts?: readonly AccountMeta[];
    /** Transfer-hook accounts for the input mint (empty if it has no hook). */
    inputHookAccounts?: readonly AccountMeta[];
};

/** `swap` arguments; `inputHookAccounts` is always set from the accounts. */
export type SwapInstructionArgs = Omit<SwapArgs, 'inputHookAccounts'> & { inputHookAccounts?: number };

/** `swap`. `args.inputHookAccounts` is set from `accounts.inputHookAccounts`. */
export async function swap(accounts: SwapAccounts, args: SwapInstructionArgs): Promise<PipelineInstruction> {
    const market = await marketPda(accounts.strategyProgram, accounts.marketState);
    const hookAccounts = accounts.inputHookAccounts ?? [];
    if (hookAccounts.length > 0xff) {
        throw new RangeError(`at most 255 input hook accounts, got ${hookAccounts.length}`);
    }
    return routerInstruction(
        [
            readonlySigner(accounts.trader),
            readonly(await configPda()),
            readonly(accounts.strategyProgram),
            readonly(market),
            writable(accounts.marketState),
            writable(accounts.traderInput),
            writable(accounts.traderOutput),
            writable(accounts.inputVault),
            readonly(accounts.inputMint),
            readonly(accounts.outputMint),
            readonly(accounts.inputTokenProgram),
            readonly(accounts.outputTokenProgram),
            readonly(await feeAuthorityPda(market)),
            writable(await feeAccountPda(market, accounts.inputMint, accounts.inputTokenProgram)),
            // Anchor encodes a missing optional account as the program ID.
            accounts.integratorFeeAccount
                ? writable(accounts.integratorFeeAccount)
                : readonly(PIPELINE_PROGRAM_ADDRESS),
            ...(await eventCpiAccounts()),
            ...(accounts.strategyAccounts ?? []),
            ...hookAccounts,
        ],
        instructionData(DISCRIMINATORS.swap, getSwapArgsCodec(), {
            ...args,
            inputHookAccounts: hookAccounts.length,
        }),
    );
}

/**
 * `sweep_fees`, moving a market's fees in `mint` to the treasury owner's associated token
 * account (which must already exist).
 */
export async function sweepFees(input: {
    market: Address;
    mint: Address;
    tokenProgram: Address;
    treasuryOwner: Address;
    hookAccounts?: readonly AccountMeta[];
}): Promise<PipelineInstruction> {
    return routerInstruction(
        [
            readonly(await configPda()),
            readonly(input.market),
            readonly(await feeAuthorityPda(input.market)),
            readonly(input.mint),
            writable(await feeAccountPda(input.market, input.mint, input.tokenProgram)),
            writable(await associatedTokenAddress(input.treasuryOwner, input.mint, input.tokenProgram)),
            readonly(input.tokenProgram),
            ...(await eventCpiAccounts()),
            ...(input.hookAccounts ?? []),
        ],
        discriminatorOnly(DISCRIMINATORS.sweepFees),
    );
}
