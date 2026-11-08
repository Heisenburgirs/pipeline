import { getAddressEncoder, getProgramDerivedAddress, type Address } from '@solana/kit';

import {
    ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
    BPF_LOADER_UPGRADEABLE_PROGRAM_ADDRESS,
    PIPELINE_PROGRAM_ADDRESS,
    SEEDS,
} from './constants.js';

// Program-derived addresses used by the router. Each helper returns the address found
// with the canonical (highest valid) bump, exactly as `Pubkey::find_program_address`.

const addressBytes = getAddressEncoder();

async function find(programAddress: Address, seeds: Uint8Array[]): Promise<Address> {
    const [pda] = await getProgramDerivedAddress({ programAddress, seeds });
    return pda;
}

function bytes(address: Address): Uint8Array {
    return addressBytes.encode(address) as Uint8Array;
}

export function configPda(): Promise<Address> {
    return find(PIPELINE_PROGRAM_ADDRESS, [SEEDS.config]);
}

export function strategyPda(strategyProgram: Address): Promise<Address> {
    return find(PIPELINE_PROGRAM_ADDRESS, [SEEDS.strategy, bytes(strategyProgram)]);
}

/** The market account, which also signs every router call into the strategy. */
export function marketPda(strategyProgram: Address, marketState: Address): Promise<Address> {
    return find(PIPELINE_PROGRAM_ADDRESS, [SEEDS.market, bytes(strategyProgram), bytes(marketState)]);
}

/** Temporary payer for a strategy's market setup, funded by the creator. */
export function setupPayerPda(market: Address): Promise<Address> {
    return find(PIPELINE_PROGRAM_ADDRESS, [SEEDS.setupPayer, bytes(market)]);
}

export function feeAuthorityPda(market: Address): Promise<Address> {
    return find(PIPELINE_PROGRAM_ADDRESS, [SEEDS.feeAuthority, bytes(market)]);
}

/** The market's fee account for one mint: the fee authority's associated token account. */
export async function feeAccountPda(market: Address, mint: Address, tokenProgram: Address): Promise<Address> {
    return associatedTokenAddress(await feeAuthorityPda(market), mint, tokenProgram);
}

/**
 * The PDA a strategy program signs with to register itself without an upgrade authority.
 * Derived from the strategy program, not the router.
 */
export function registerPda(strategyProgram: Address): Promise<Address> {
    return find(strategyProgram, [SEEDS.register]);
}

/** Anchor's `#[event_cpi]` authority for the router. */
export function eventAuthorityPda(): Promise<Address> {
    return find(PIPELINE_PROGRAM_ADDRESS, [SEEDS.eventAuthority]);
}

/** A loader-v3 program's ProgramData account. */
export function programDataPda(program: Address): Promise<Address> {
    return find(BPF_LOADER_UPGRADEABLE_PROGRAM_ADDRESS, [bytes(program)]);
}

export function associatedTokenAddress(owner: Address, mint: Address, tokenProgram: Address): Promise<Address> {
    return find(ASSOCIATED_TOKEN_PROGRAM_ADDRESS, [bytes(owner), bytes(tokenProgram), bytes(mint)]);
}
