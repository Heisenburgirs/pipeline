// Initialises Pipeline on a cluster (config and the constant-product playground strategy)
// and runs an end-to-end smoke test: create two tokens and a market, add liquidity, resolve
// accounts and quote by RPC simulation, swap, and decode the swap event.
//
// Usage (on devnet, or a local validator with the programs loaded):
//   RPC_URL=https://api.devnet.solana.com node smoke.ts
// Environment:
//   RPC_URL   cluster RPC (default http://127.0.0.1:8899)
//   WS_URL    cluster WebSocket (default derived from RPC_URL)
//   KEYPAIR   keypair JSON that pays for everything. On devnet, where Pipeline is set up,
//             any funded wallet works. On a fresh local validator it must be the router's
//             upgrade authority, which initialises the config and registers the strategy.
//             Default: ~/.config/pipeline/keys/devnet-admin.json if it exists, otherwise
//             ~/.config/solana/id.json.

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

const CP_PROGRAM = address('yRptCdde1du1JYR2DcNFtLC1fkp5Asd6rE3XSvZoUFx');
const LISTING_FEE_LAMPORTS = 10_000_000n; // 0.01 SOL, the devnet listing fee
const POOL_FEE_BPS = 30;
const SWAP_AMOUNT = 1_000_000n;

const rpcUrl = process.env.RPC_URL ?? 'http://127.0.0.1:8899';
const wsUrl =
    process.env.WS_URL ??
    rpcUrl.replace(/^http/, 'ws').replace(/:8899(\/?)$/, ':8900$1');
// Public RPC endpoints rate-limit bursts (HTTP 429); retry those with back-off.
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

function log(message: string) {
    console.log(`[smoke] ${message}`);
}

async function loadAdmin(): Promise<KeyPairSigner> {
    const pipelineKey = join(homedir(), '.config/pipeline/keys/devnet-admin.json');
    const path =
        process.env.KEYPAIR ??
        process.env.ADMIN_KEYPAIR ??
        (existsSync(pipelineKey) ? pipelineKey : join(homedir(), '.config/solana/id.json'));
    const bytes = new Uint8Array(JSON.parse(readFileSync(path, 'utf8')));
    return createKeyPairSignerFromBytes(bytes);
}

/** Sends instructions paid for by `payer`, signed by `payer` and any extra signers. */
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
    await sendAndConfirm(signed as Parameters<typeof sendAndConfirm>[0], { commitment: 'finalized' });
    return getSignatureFromTransaction(signed);
}

async function accountData(account: Address): Promise<Uint8Array | null> {
    const { value } = await rpc.getAccountInfo(account, { encoding: 'base64', commitment: 'finalized' }).send();
    return value ? new Uint8Array(Buffer.from(value.data[0], 'base64')) : null;
}

async function tokenBalance(account: Address): Promise<bigint> {
    const { value } = await rpc.getTokenAccountBalance(account, { commitment: 'finalized' }).send();
    return BigInt(value.amount);
}

async function ata(owner: Address, mint: Address): Promise<Address> {
    const [account] = await findAssociatedTokenPda({ owner, mint, tokenProgram: TOKEN_PROGRAM_ADDRESS });
    return account;
}

async function createMintWithBalance(admin: KeyPairSigner, decimals: number, amount: bigint) {
    const mint = await generateKeyPairSigner();
    const space = BigInt(getMintSize());
    const rent = await rpc.getMinimumBalanceForRentExemption(space).send();
    const holder = await ata(admin.address, mint.address);
    await send(
        admin,
        [
            getCreateAccountInstruction({
                payer: admin,
                newAccount: mint,
                lamports: rent,
                space,
                programAddress: TOKEN_PROGRAM_ADDRESS,
            }),
            getInitializeMint2Instruction({ mint: mint.address, decimals, mintAuthority: admin.address }),
            await getCreateAssociatedTokenIdempotentInstructionAsync({
                payer: admin,
                owner: admin.address,
                mint: mint.address,
            }),
            getMintToInstruction({ mint: mint.address, token: holder, mintAuthority: admin, amount }),
        ],
        [mint],
    );
    return mint.address;
}

/** The constant-product strategy's `deposit` custom action (no LP shares; playground only). */
function depositInstruction(
    depositor: Address,
    pool: Address,
    accounts: { a: Address; b: Address; vaultA: Address; vaultB: Address; mintA: Address; mintB: Address },
    amountA: bigint,
    amountB: bigint,
): Instruction {
    const data = new Uint8Array(24);
    data.set([242, 35, 198, 137, 82, 225, 242, 182]); // sha256("global:deposit")[..8]
    const view = new DataView(data.buffer);
    view.setBigUint64(8, amountA, true);
    view.setBigUint64(16, amountB, true);
    return {
        programAddress: CP_PROGRAM,
        accounts: [
            { address: depositor, role: AccountRole.READONLY_SIGNER },
            { address: pool, role: AccountRole.WRITABLE },
            { address: accounts.a, role: AccountRole.WRITABLE },
            { address: accounts.b, role: AccountRole.WRITABLE },
            { address: accounts.vaultA, role: AccountRole.WRITABLE },
            { address: accounts.vaultB, role: AccountRole.WRITABLE },
            { address: accounts.mintA, role: AccountRole.READONLY },
            { address: accounts.mintB, role: AccountRole.READONLY },
            { address: TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
            { address: TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
        ],
        data,
    };
}

async function main() {
    const admin = await loadAdmin();
    log(`RPC ${rpcUrl}, wallet ${admin.address}`);

    // 1. Config.
    const configAddress = await pipeline.configPda();
    let configData = await accountData(configAddress);
    if (!configData) {
        log('initialising config');
        await send(admin, [
            await pipeline.initializeConfig({
                admin: admin.address,
                args: {
                    listingFeeLamports: LISTING_FEE_LAMPORTS,
                    listingFeeRecipient: admin.address,
                    treasuryOwner: admin.address,
                },
            }),
        ]);
        configData = await accountData(configAddress);
    }
    const config = pipeline.decodeConfig(configData!);
    if (!config) throw new Error('config account did not decode');
    log(`config: admin ${config.admin}, listing fee ${config.listingFeeLamports} lamports`);

    // 2. Register the playground strategy.
    if (!(await accountData(await pipeline.strategyPda(CP_PROGRAM)))) {
        log('registering the constant-product strategy');
        await send(admin, [
            await pipeline.registerStrategy({
                payer: admin.address,
                registrar: admin.address,
                strategyProgram: CP_PROGRAM,
                listingFeeRecipient: config.listingFeeRecipient,
                metadataUri: 'https://example.com/pipeline/constant-product.json',
                maxListingFeeLamports: config.listingFeeLamports,
            }),
        ]);
    }

    // 3. Two fresh tokens and a market between them.
    const mintA = await createMintWithBalance(admin, 6, 1_000_000_000_000n);
    const mintB = await createMintWithBalance(admin, 9, 1_000_000_000_000n);
    const feeSeed = new Uint8Array([POOL_FEE_BPS & 0xff, POOL_FEE_BPS >> 8]);
    const encoder = getAddressEncoder();
    const [pool] = await getProgramDerivedAddress({
        programAddress: CP_PROGRAM,
        seeds: ['pool', encoder.encode(mintA), encoder.encode(mintB), feeSeed],
    });
    const vaultA = await ata(pool, mintA);
    const vaultB = await ata(pool, mintB);
    log(`creating market: pool ${pool}`);
    await send(admin, [
        await pipeline.createMarket({
            creator: admin.address,
            strategyProgram: CP_PROGRAM,
            marketState: pool,
            initData: feeSeed,
            maxCreatorLamports: 100_000_000n,
            strategyAccounts: [
                { address: mintA, role: AccountRole.READONLY },
                { address: mintB, role: AccountRole.READONLY },
                { address: vaultA, role: AccountRole.WRITABLE },
                { address: vaultB, role: AccountRole.WRITABLE },
                { address: TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
                { address: TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
                { address: pipeline.ASSOCIATED_TOKEN_PROGRAM_ADDRESS, role: AccountRole.READONLY },
            ],
        }),
    ]);
    const marketAddress = await pipeline.marketPda(CP_PROGRAM, pool);
    await send(admin, [
        await pipeline.initFeeAccount({ payer: admin.address, market: marketAddress, mint: mintA, tokenProgram: TOKEN_PROGRAM_ADDRESS }),
        await pipeline.initFeeAccount({ payer: admin.address, market: marketAddress, mint: mintB, tokenProgram: TOKEN_PROGRAM_ADDRESS }),
    ]);

    // 4. Liquidity.
    const adminA = await ata(admin.address, mintA);
    const adminB = await ata(admin.address, mintB);
    await send(admin, [
        depositInstruction(admin.address, pool, { a: adminA, b: adminB, vaultA, vaultB, mintA, mintB }, 100_000_000_000n, 200_000_000_000n),
    ]);
    log('market created and funded');

    // 5. Resolve accounts and quote by RPC simulation.
    const marketData = await accountData(marketAddress);
    const market = pipeline.decodeMarket(marketData!);
    if (!market) throw new Error('market account did not decode');
    const view = pipeline.marketViewFromMarket(marketAddress, market, [TOKEN_PROGRAM_ADDRESS, TOKEN_PROGRAM_ADDRESS]);
    const trade = {
        mode: pipeline.TradeMode.ExactIn,
        inputIndex: 0,
        outputIndex: 1,
        amount: SWAP_AMOUNT,
        trader: admin.address,
        extra: new Uint8Array(),
    };
    const strategyAccounts = await pipeline.resolveAccounts(rpc, view, trade, admin.address);
    const { quote, unitsConsumed } = await pipeline.quoteStrategy(rpc, view, trade, strategyAccounts, admin.address);
    log(`quote: ${SWAP_AMOUNT} A -> ${quote.amountOut} B (${unitsConsumed} CU)`);

    // 6. Swap with the quote as the minimum, then check the event.
    const beforeB = await tokenBalance(adminB);
    const signature = await send(admin, [
        await pipeline.swap(
            {
                trader: admin.address,
                strategyProgram: CP_PROGRAM,
                marketState: pool,
                traderInput: adminA,
                traderOutput: adminB,
                inputVault: vaultA,
                inputMint: mintA,
                outputMint: mintB,
                inputTokenProgram: TOKEN_PROGRAM_ADDRESS,
                outputTokenProgram: TOKEN_PROGRAM_ADDRESS,
                strategyAccounts,
            },
            {
                mode: pipeline.TradeMode.ExactIn,
                amountIn: SWAP_AMOUNT,
                amountOut: 0n,
                minAmountOut: quote.amountOut,
                integratorFeeBps: 0,
                extra: new Uint8Array(),
            },
        ),
    ]);
    const received = (await tokenBalance(adminB)) - beforeB;

    const tx = await rpc
        .getTransaction(signature, { encoding: 'json', maxSupportedTransactionVersion: 0, commitment: 'finalized' })
        .send();
    if (!tx?.meta?.innerInstructions) throw new Error('swap transaction not found');
    const keys = [
        ...tx.transaction.message.accountKeys,
        ...(tx.meta.loadedAddresses?.writable ?? []),
        ...(tx.meta.loadedAddresses?.readonly ?? []),
    ];
    const events = pipeline.extractEventsFromInnerInstructions(keys, tx.meta.innerInstructions);
    const swapEvent = events.find((event) => event.type === 'Swap');
    if (!swapEvent || swapEvent.type !== 'Swap') throw new Error('no swap event');

    const expectedFee = pipeline.pipelineFee(SWAP_AMOUNT);
    const checks: [string, boolean][] = [
        ['received equals quote', received === quote.amountOut],
        ['event received equals quote', swapEvent.data.received === quote.amountOut],
        ['event net_sent equals amount', swapEvent.data.netSent === SWAP_AMOUNT],
        ['event fee is 0.25% rounded up', swapEvent.data.pipelineFee === expectedFee],
        ['event market', swapEvent.data.market === marketAddress],
    ];
    for (const [name, ok] of checks) log(`${ok ? 'PASS' : 'FAIL'} ${name}`);
    log(`swap ${signature}: received ${received}, fee ${swapEvent.data.pipelineFee}, ${tx.meta.computeUnitsConsumed} CU`);
    if (checks.some(([, ok]) => !ok)) process.exit(1);
    log('smoke test passed');
    process.exit(0);
}

main().catch((error) => {
    console.error(error);
    process.exit(1);
});
