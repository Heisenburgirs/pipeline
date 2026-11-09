// Registers the launch-curve and gacha-curve strategies with Pipeline on a cluster and
// creates a demo market on each: a buy on the launch curve, and a gacha buy, ticket
// resolution and payout check on the gacha curve.
//
// Usage:
//   RPC_URL=https://api.devnet.solana.com KEYPAIR=~/.config/solana/id.json node strategies.ts
// Environment:
//   RPC_URL, WS_URL        cluster endpoints (default local validator)
//   KEYPAIR                pays for everything and must be both programs' upgrade authority
//                          for the registration step (skipped if already registered)
//   LAUNCH_METADATA_URI    metadata JSON for the launch curve (used when registering)
//   GACHA_METADATA_URI     metadata JSON for the gacha curve
//   ONLY                   "launch" or "gacha" to run one of the two

import { existsSync, readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

import {
    AccountRole,
    addSignersToTransactionMessage,
    address,
    appendTransactionMessageInstructions,
    createKeyPairSignerFromBytes,
    createDefaultRpcTransport,
    createSolanaRpcFromTransport,
    createSolanaRpcSubscriptions,
    createTransactionMessage,
    generateKeyPairSigner,
    getAddressEncoder,
    getProgramDerivedAddress,
    getSignatureFromTransaction,
    pipe,
    sendAndConfirmTransactionFactory,
    setTransactionMessageFeePayerSigner,
    setTransactionMessageLifetimeUsingBlockhash,
    signTransactionMessageWithSigners,
    type Address,
    type Instruction,
    type KeyPairSigner,
} from '@solana/kit';
import { getCreateAccountInstruction } from '@solana-program/system';
import {
    findAssociatedTokenPda,
    getCreateAssociatedTokenIdempotentInstructionAsync,
    getInitializeMint2Instruction,
    getMintSize,
    getMintToInstruction,
    TOKEN_PROGRAM_ADDRESS,
} from '@solana-program/token';
import * as pipeline from '@pipeline-placeholder/sdk';

const LAUNCH_PROGRAM = address('4scUVyL9JbLjvkppBk8PYNGRPMCQfp7ppdBFkthDYPLA');
const GACHA_PROGRAM = address('2TmcmquxnCB8AHoLmLJFYxZnVQdm9pREMEsmHqByQq7G');
const SYSTEM_PROGRAM = address('11111111111111111111111111111111');
const SLOT_HASHES = address('SysvarS1otHashes111111111111111111111111111');

// Launch curve: pump.fun's ratios scaled to a 1B supply, a 6-decimal quote and a $1.00
// final price (a $1B graduation). See docs/LAUNCH-CURVE.md.
const USD = 1_000_000n;
const LAUNCH = {
    supply: 1_000_000_000n * USD,
    curveSupply: 793_100_000n * USD,
    virtualQuote: 73_013_988_816_403n,
    virtualToken: 279_900_000n * USD,
    poolFeeBps: 30,
};
// Gacha curve: a 1B supply against a 9-decimal quote, 2% fee, 1-in-20 for a 40% bonus
// capped at 10% of the pot, decided 10 slots after the buy. See docs/GACHA-CURVE.md.
const GACHA = {
    supply: 1_000_000_000n * USD,
    virtualQuote: 30_000_000_000n,
    virtualToken: 280_000_000n * USD,
    gachaFeeBps: 200,
    winOneIn: 20,
    bonusBps: 4_000,
    maxWinPotBps: 1_000,
    revealDelaySlots: 10n,
};

const rpcUrl = process.env.RPC_URL ?? 'http://127.0.0.1:8899';
const wsUrl = process.env.WS_URL ?? rpcUrl.replace(/^http/, 'ws').replace(/:8899(\/?)$/, ':8900$1');
const baseTransport = createDefaultRpcTransport({ url: rpcUrl });
const retryingTransport = (async (...args: Parameters<typeof baseTransport>) => {
    for (let attempt = 0; ; attempt++) {
        try {
            return await baseTransport(...args);
        } catch (error) {
            if (attempt >= 8 || !String(error).includes('429')) throw error;
            await new Promise((resolve) => setTimeout(resolve, 500 * 2 ** attempt));
        }
    }
}) as typeof baseTransport;
const rpc = createSolanaRpcFromTransport(retryingTransport);
const rpcSubscriptions = createSolanaRpcSubscriptions(wsUrl);
const sendAndConfirm = sendAndConfirmTransactionFactory({ rpc, rpcSubscriptions });
const encoder = getAddressEncoder();

function log(message: string) {
    console.log(`[strategies] ${message}`);
}

async function loadWallet(): Promise<KeyPairSigner> {
    const path = process.env.KEYPAIR ?? join(homedir(), '.config/solana/id.json');
    if (!existsSync(path)) throw new Error(`no keypair at ${path}`);
    return createKeyPairSignerFromBytes(new Uint8Array(JSON.parse(readFileSync(path, 'utf8'))));
}

async function send(payer: KeyPairSigner, instructions: Instruction[], extraSigners: KeyPairSigner[] = []) {
    const { value: blockhash } = await rpc.getLatestBlockhash({ commitment: 'finalized' }).send();
    const message = pipe(
        createTransactionMessage({ version: 0 }),
        (m) => setTransactionMessageFeePayerSigner(payer, m),
        (m) => setTransactionMessageLifetimeUsingBlockhash(blockhash, m),
        (m) => appendTransactionMessageInstructions(instructions, m),
        (m) => addSignersToTransactionMessage(extraSigners, m),
    );
    const signed = await signTransactionMessageWithSigners(message);
    await sendAndConfirm(signed as Parameters<typeof sendAndConfirm>[0], { commitment: 'confirmed' });
    return getSignatureFromTransaction(signed);
}

async function accountData(account: Address): Promise<Uint8Array | null> {
    const { value } = await rpc.getAccountInfo(account, { encoding: 'base64', commitment: 'confirmed' }).send();
    return value ? new Uint8Array(Buffer.from(value.data[0], 'base64')) : null;
}

async function tokenBalance(account: Address): Promise<bigint> {
    const { value } = await rpc.getTokenAccountBalance(account, { commitment: 'confirmed' }).send();
    return BigInt(value.amount);
}

async function currentSlot(): Promise<bigint> {
    return rpc.getSlot({ commitment: 'confirmed' }).send();
}

async function ata(owner: Address, mint: Address): Promise<Address> {
    const [account] = await findAssociatedTokenPda({ owner, mint, tokenProgram: TOKEN_PROGRAM_ADDRESS });
    return account;
}

async function pda(program: Address, seeds: (string | Uint8Array)[]): Promise<Address> {
    const [result] = await getProgramDerivedAddress({ programAddress: program, seeds });
    return result;
}

/** A fresh mint with `amount` minted to the wallet; stands in for USDC or wrapped SOL. */
async function createQuoteMint(wallet: KeyPairSigner, decimals: number, amount: bigint) {
    const mint = await generateKeyPairSigner();
    const space = BigInt(getMintSize());
    const rent = await rpc.getMinimumBalanceForRentExemption(space).send();
    const holder = await ata(wallet.address, mint.address);
    await send(
        wallet,
        [
            getCreateAccountInstruction({ payer: wallet, newAccount: mint, lamports: rent, space, programAddress: TOKEN_PROGRAM_ADDRESS }),
            getInitializeMint2Instruction({ mint: mint.address, decimals, mintAuthority: wallet.address }),
            await getCreateAssociatedTokenIdempotentInstructionAsync({ payer: wallet, owner: wallet.address, mint: mint.address }),
            getMintToInstruction({ mint: mint.address, token: holder, mintAuthority: wallet, amount }),
        ],
        [mint],
    );
    return mint.address;
}

function u64(value: bigint): Uint8Array {
    const out = new Uint8Array(8);
    new DataView(out.buffer).setBigUint64(0, value, true);
    return out;
}
function u32(value: number): Uint8Array {
    const out = new Uint8Array(4);
    new DataView(out.buffer).setUint32(0, value, true);
    return out;
}
function u16(value: number): Uint8Array {
    return new Uint8Array([value & 0xff, value >> 8]);
}
function concat(...parts: Uint8Array[]): Uint8Array {
    const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
    let offset = 0;
    for (const part of parts) {
        out.set(part, offset);
        offset += part.length;
    }
    return out;
}
function hex(bytes: string): Uint8Array {
    return Uint8Array.from(Buffer.from(bytes, 'hex'));
}

/** Registers `program` if it is not registered yet, paying the listing fee. */
async function register(wallet: KeyPairSigner, program: Address, metadataUri: string | undefined, label: string) {
    const configData = await accountData(await pipeline.configPda());
    const config = configData && pipeline.decodeConfig(configData);
    if (!config) throw new Error('Pipeline is not set up on this cluster');
    if (await accountData(await pipeline.strategyPda(program))) {
        log(`${label}: already registered`);
        return;
    }
    if (!metadataUri) throw new Error(`${label}: set the metadata URI to register`);
    const signature = await send(wallet, [
        await pipeline.registerStrategy({
            payer: wallet.address,
            registrar: wallet.address,
            strategyProgram: program,
            listingFeeRecipient: config.listingFeeRecipient,
            metadataUri,
            maxListingFeeLamports: config.listingFeeLamports,
        }),
    ]);
    log(`${label}: registered, listing fee ${config.listingFeeLamports} lamports, ${signature}`);
}

/** Quotes an exact-in trade by RPC simulation and swaps with the quote as the minimum. */
async function quoteAndSwap(
    wallet: KeyPairSigner,
    program: Address,
    marketState: Address,
    mints: [Address, Address],
    vaults: [Address, Address],
    buy: boolean,
    amount: bigint,
    extra: Uint8Array,
    before: Instruction[] = [],
) {
    const marketAddress = await pipeline.marketPda(program, marketState);
    const marketData = await accountData(marketAddress);
    const market = marketData && pipeline.decodeMarket(marketData);
    if (!market) throw new Error('market account did not decode');
    const view = pipeline.marketViewFromMarket(marketAddress, market, [TOKEN_PROGRAM_ADDRESS, TOKEN_PROGRAM_ADDRESS]);
    const trade = {
        mode: pipeline.TradeMode.ExactIn,
        inputIndex: buy ? 0 : 1,
        outputIndex: buy ? 1 : 0,
        amount,
        trader: wallet.address,
        extra,
    };
    const strategyAccounts = await pipeline.resolveAccounts(rpc, view, trade, wallet.address);
    const { quote } = await pipeline.quoteStrategy(rpc, view, trade, strategyAccounts, wallet.address);
    const [inputMint, outputMint] = buy ? mints : [mints[1], mints[0]];
    const inputVault = buy ? vaults[0] : vaults[1];
    const traderInput = await ata(wallet.address, inputMint);
    const traderOutput = await ata(wallet.address, outputMint);
    const outputBefore = await tokenBalance(traderOutput);
    const signature = await send(wallet, [
        ...before,
        await pipeline.swap(
            {
                trader: wallet.address,
                strategyProgram: program,
                marketState,
                traderInput,
                traderOutput,
                inputVault,
                inputMint,
                outputMint,
                inputTokenProgram: TOKEN_PROGRAM_ADDRESS,
                outputTokenProgram: TOKEN_PROGRAM_ADDRESS,
                strategyAccounts,
            },
            {
                mode: pipeline.TradeMode.ExactIn,
                amountIn: amount,
                amountOut: 0n,
                minAmountOut: quote.amountOut,
                integratorFeeBps: 0,
                extra,
            },
        ),
    ]);
    const received = (await tokenBalance(traderOutput)) - outputBefore;
    return { quote: quote.amountOut, received, signature, strategyAccounts, marketAddress };
}

// ------------------------------------------------------------------ launch curve

async function launchDemo(wallet: KeyPairSigner) {
    await register(wallet, LAUNCH_PROGRAM, process.env.LAUNCH_METADATA_URI, 'launch-curve');

    // A "USDC" with a million dollars for the wallet, and the token mint keypair.
    const quoteMint = await createQuoteMint(wallet, 6, 1_000_000n * USD);
    const tokenMint = await generateKeyPairSigner();
    const launch = await pda(LAUNCH_PROGRAM, ['launch', encoder.encode(tokenMint.address)]);
    const quoteVault = await pda(LAUNCH_PROGRAM, ['quote_vault', encoder.encode(launch)]);
    const tokenVault = await pda(LAUNCH_PROGRAM, ['token_vault', encoder.encode(launch)]);
    const initData = concat(
        u64(LAUNCH.supply),
        u64(LAUNCH.curveSupply),
        u64(LAUNCH.virtualQuote),
        u64(LAUNCH.virtualToken),
        u16(LAUNCH.poolFeeBps),
    );
    await send(
        wallet,
        [
            await pipeline.createMarket({
                creator: wallet.address,
                strategyProgram: LAUNCH_PROGRAM,
                marketState: launch,
                initData,
                maxCreatorLamports: 100_000_000n,
                strategyAccounts: [
                    { address: quoteMint, role: AccountRole.READONLY },
                    { address: tokenMint.address, role: AccountRole.WRITABLE_SIGNER },
                    { address: quoteVault, role: AccountRole.WRITABLE },
                    { address: tokenVault, role: AccountRole.WRITABLE },
                    { address: TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
                    { address: TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
                ],
            }),
        ],
        [tokenMint],
    );
    const marketAddress = await pipeline.marketPda(LAUNCH_PROGRAM, launch);
    await send(wallet, [
        await pipeline.initFeeAccount({ payer: wallet.address, market: marketAddress, mint: quoteMint, tokenProgram: TOKEN_PROGRAM_ADDRESS }),
        await pipeline.initFeeAccount({ payer: wallet.address, market: marketAddress, mint: tokenMint.address, tokenProgram: TOKEN_PROGRAM_ADDRESS }),
        await getCreateAssociatedTokenIdempotentInstructionAsync({ payer: wallet, owner: wallet.address, mint: tokenMint.address }),
    ]);
    log(`launch-curve market ${marketAddress}: token ${tokenMint.address}, quote ${quoteMint}, launch ${launch}`);

    // Buy $1,000 on the fresh curve: about 14,700 tokens at the $0.068 opening price.
    const amount = 1_000n * USD;
    const { quote, received, signature } = await quoteAndSwap(
        wallet, LAUNCH_PROGRAM, launch, [quoteMint, tokenMint.address], [quoteVault, tokenVault], true, amount, new Uint8Array(),
    );
    const price = Number(amount) / Number(received);
    log(`launch-curve buy: $${Number(amount) / 1e6} -> ${(Number(received) / 1e6).toFixed(2)} tokens at $${price.toFixed(4)} each (quote ${quote}, received ${received}) ${signature}`);
    if (quote !== received) throw new Error('received differs from quote');
    return { marketAddress, tokenMint: tokenMint.address, quoteMint, swap: signature };
}

// ------------------------------------------------------------------- gacha curve

async function gachaDemo(wallet: KeyPairSigner) {
    await register(wallet, GACHA_PROGRAM, process.env.GACHA_METADATA_URI, 'gacha-curve');

    // A 9-decimal quote standing in for wrapped SOL, with 1,000 units for the wallet.
    const quoteMint = await createQuoteMint(wallet, 9, 1_000n * 1_000_000_000n);
    const tokenMint = await generateKeyPairSigner();
    const curve = await pda(GACHA_PROGRAM, ['curve', encoder.encode(tokenMint.address)]);
    const quoteVault = await pda(GACHA_PROGRAM, ['quote_vault', encoder.encode(curve)]);
    const tokenVault = await pda(GACHA_PROGRAM, ['token_vault', encoder.encode(curve)]);
    const potVault = await pda(GACHA_PROGRAM, ['pot', encoder.encode(curve)]);
    const ticket = await pda(GACHA_PROGRAM, ['ticket', encoder.encode(curve), encoder.encode(wallet.address)]);
    const initData = concat(
        u64(GACHA.supply),
        u64(GACHA.virtualQuote),
        u64(GACHA.virtualToken),
        u16(GACHA.gachaFeeBps),
        u32(GACHA.winOneIn),
        u16(GACHA.bonusBps),
        u16(GACHA.maxWinPotBps),
        u64(GACHA.revealDelaySlots),
    );
    await send(
        wallet,
        [
            await pipeline.createMarket({
                creator: wallet.address,
                strategyProgram: GACHA_PROGRAM,
                marketState: curve,
                initData,
                maxCreatorLamports: 100_000_000n,
                strategyAccounts: [
                    { address: quoteMint, role: AccountRole.READONLY },
                    { address: tokenMint.address, role: AccountRole.WRITABLE_SIGNER },
                    { address: quoteVault, role: AccountRole.WRITABLE },
                    { address: tokenVault, role: AccountRole.WRITABLE },
                    { address: potVault, role: AccountRole.WRITABLE },
                    { address: TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
                    { address: TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
                ],
            }),
        ],
        [tokenMint],
    );
    const marketAddress = await pipeline.marketPda(GACHA_PROGRAM, curve);
    await send(wallet, [
        await pipeline.initFeeAccount({ payer: wallet.address, market: marketAddress, mint: quoteMint, tokenProgram: TOKEN_PROGRAM_ADDRESS }),
        await pipeline.initFeeAccount({ payer: wallet.address, market: marketAddress, mint: tokenMint.address, tokenProgram: TOKEN_PROGRAM_ADDRESS }),
        await getCreateAssociatedTokenIdempotentInstructionAsync({ payer: wallet, owner: wallet.address, mint: tokenMint.address }),
    ]);
    log(`gacha-curve market ${marketAddress}: token ${tokenMint.address}, quote ${quoteMint}, curve ${curve}`);

    // A gacha buy of 2 quote units, with open_ticket in the same transaction before it.
    const openTicket: Instruction = {
        programAddress: GACHA_PROGRAM,
        accounts: [
            { address: wallet.address, role: AccountRole.WRITABLE_SIGNER },
            { address: curve, role: AccountRole.READONLY },
            { address: ticket, role: AccountRole.WRITABLE },
            { address: SYSTEM_PROGRAM, role: AccountRole.READONLY },
        ],
        data: hex('9a78a6459b6f1bcf'),
    };
    const amount = 2_000_000_000n;
    const { quote, received, signature, strategyAccounts } = await quoteAndSwap(
        wallet, GACHA_PROGRAM, curve, [quoteMint, tokenMint.address], [quoteVault, tokenVault], true, amount, new Uint8Array([1]), [openTicket],
    );
    const pot = await tokenBalance(potVault);
    log(`gacha buy: ${Number(amount) / 1e9} quote -> ${(Number(received) / 1e6).toFixed(2)} tokens (quote ${quote}), pot now holds ${(Number(pot) / 1e6).toFixed(2)} tokens, ${strategyAccounts.length} strategy accounts ${signature}`);
    if (quote !== received) throw new Error('received differs from quote');

    const ticketData = await accountData(ticket);
    if (!ticketData || ticketData[72] !== 1) throw new Error('ticket is not armed');
    const buySlot = new DataView(ticketData.buffer, ticketData.byteOffset).getBigUint64(73, true);
    const revealSlot = buySlot + GACHA.revealDelaySlots;
    log(`ticket ${ticket} armed at slot ${buySlot}; reveal slot ${revealSlot}`);
    while ((await currentSlot()) < revealSlot + 2n) {
        await new Promise((resolve) => setTimeout(resolve, 1_500));
    }

    const traderToken = await ata(wallet.address, tokenMint.address);
    const before = await tokenBalance(traderToken);
    const resolve: Instruction = {
        programAddress: GACHA_PROGRAM,
        accounts: [
            { address: ticket, role: AccountRole.WRITABLE },
            { address: curve, role: AccountRole.READONLY },
            { address: wallet.address, role: AccountRole.WRITABLE },
            { address: potVault, role: AccountRole.WRITABLE },
            { address: traderToken, role: AccountRole.WRITABLE },
            { address: tokenMint.address, role: AccountRole.READONLY },
            { address: TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
            { address: SLOT_HASHES, role: AccountRole.READONLY },
        ],
        data: hex('d5c239720de3a005'),
    };
    const resolveSignature = await send(wallet, [resolve]);
    const payout = (await tokenBalance(traderToken)) - before;
    const closed = (await accountData(ticket)) === null;
    log(`ticket resolved: ${payout > 0n ? `WON ${(Number(payout) / 1e6).toFixed(2)} tokens from the pot` : 'lost (1-in-20 odds)'}, ticket closed: ${closed}, ${resolveSignature}`);
    if (!closed) throw new Error('ticket should be closed after resolution');
    return { marketAddress, tokenMint: tokenMint.address, quoteMint, swap: signature, resolve: resolveSignature, payout };
}

async function main() {
    const wallet = await loadWallet();
    log(`RPC ${rpcUrl}, wallet ${wallet.address}`);
    const only = process.env.ONLY;
    const results: Record<string, unknown> = {};
    if (only !== 'gacha') results.launch = await launchDemo(wallet);
    if (only !== 'launch') results.gacha = await gachaDemo(wallet);
    console.log(JSON.stringify(results, (_, v) => (typeof v === 'bigint' ? v.toString() : v), 2));
    process.exit(0);
}

main().catch((error) => {
    console.error(error);
    process.exit(1);
});
