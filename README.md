# Pipeline

Pipeline is a permissionless router for experimental Solana markets. Developers write any
AMM or market mechanism they like as a **strategy program**, register it once, and every
market created from it is immediately discoverable, quotable and tradable through one
router program, with one swap instruction and one swap event. Desks, terminals, bots and
chart sites integrate Pipeline once and see every new market without per-market code.

- **Strategies hold their own liquidity.** Pipeline never custodies pool funds; it moves
  the trader's input into the strategy, calls it, and checks what the trader received.
- **One fee:** 0.25% of each swap's input through Pipeline, measured on-chain. Every hop of
  a multi-hop route pays it. Nothing else is charged.
- **Open listing:** registering a strategy program costs a one-time SOL fee (0.01 SOL on
  devnet). There is no whitelist and no approval step. Markets are free to create.
- **The trader is protected by the router**, not by trusting strategies: the trader never
  signs for the strategy, never pays more than their input plus the stated fees, and gets
  at least their minimum output or the swap fails.

> **Status: live on devnet.** Not on mainnet yet. The router's maths is formally verified
> ([`docs/VERIFICATION.md`](docs/VERIFICATION.md)) and it has been security-audited
> ([`docs/SECURITY-AUDIT.md`](docs/SECURITY-AUDIT.md)); there is no external audit.
> Interfaces may still change, and package names are placeholders.

## Live on devnet

| | Address |
|---|---|
| Router | [`PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34`](https://orbmarkets.io/address/PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34/history?cluster=devnet) |
| Example strategy (constant product) | [`yRptCdde1du1JYR2DcNFtLC1fkp5Asd6rE3XSvZoUFx`](https://orbmarkets.io/address/yRptCdde1du1JYR2DcNFtLC1fkp5Asd6rE3XSvZoUFx/history?cluster=devnet) |

[`docs/DEVNET.md`](docs/DEVNET.md) gets you from nothing to a swap through the router on
devnet in a few minutes.

## Where to start

| You want to | Read |
|---|---|
| Try it on devnet | [`docs/DEVNET.md`](docs/DEVNET.md) |
| Build your own AMM or market | [`docs/QUICKSTART.md`](docs/QUICKSTART.md), then [`docs/STRATEGY-GUIDE.md`](docs/STRATEGY-GUIDE.md) |
| See a strategy with per-trader state and a custom payout action | [`docs/GACHA-CURVE.md`](docs/GACHA-CURVE.md) |
| Build a desk, terminal, bot or chart on top | [`docs/INTEGRATOR-GUIDE.md`](docs/INTEGRATOR-GUIDE.md) |
| Understand the router | [`docs/ROUTER.md`](docs/ROUTER.md) |
| See how it's verified and audited | [`docs/VERIFICATION.md`](docs/VERIFICATION.md), [`docs/SECURITY-AUDIT.md`](docs/SECURITY-AUDIT.md) |

## Repository

| Path | What | Licence |
|---|---|---|
| `programs/pipeline` | The router program (Anchor) | BSL 1.1, becomes GPL-2.0-or-later on 2028-11-08 |
| `programs/test-strategy-cp` | Example strategy: a constant-product pool. The starting point for your own | Apache-2.0 |
| `programs/gacha-curve` | Example strategy: a bonding curve with an opt-in prize pot ([`docs/GACHA-CURVE.md`](docs/GACHA-CURVE.md)) | Apache-2.0 |
| `programs/test-strategy-evil`, `test-hook`, `test-hook-refill` | Hostile strategy and transfer hooks used by the router's security tests | Apache-2.0 |
| `crates/pipeline-strategy-interface` | The strategy interface: types, discriminators, account layouts, fee maths | Apache-2.0 |
| `crates/pipeline-client` | Rust addresses, instruction builders and account decoders | Apache-2.0 |
| `crates/pipeline-events` | Event decoding for indexers | Apache-2.0 |
| `crates/pipeline-quoter` | Quotes any strategy by running its real code in an embedded Solana VM | Apache-2.0 |
| `crates/pipeline-tests` | LiteSVM integration and security tests | Apache-2.0 |
| `sdk/ts` | TypeScript SDK (`@solana/kit`) | Apache-2.0 |
| `scripts/devnet` | End-to-end script: create tokens and a market, quote, swap, decode the event | Apache-2.0 |

## Build and test

You need Rust (the version in `rust-toolchain.toml` is selected automatically),
`cargo-build-sbf` 4.4 or later for sBPF v3 output, and Node 20.19 or later for the
TypeScript SDK.

```sh
cargo install cargo-build-sbf --version 4.6.0 --locked
(cd programs && cargo build-sbf)          # programs -> programs/target/deploy
cargo test --workspace                    # LiteSVM integration and unit tests
(cd sdk/ts && npm install && npm test)    # TypeScript SDK
```

CI runs the same on every push. Keypairs are never committed: `scripts/check-no-keypairs.sh`
and the pre-commit hook (`git config core.hooksPath .githooks`) enforce it.

## Licence

The router (`programs/pipeline`) is under the Business Source License 1.1: read, modify and
use it freely outside production; on 2028-11-08 it converts to GPL-2.0-or-later. Everything
else is Apache-2.0. See [`LICENSING.md`](LICENSING.md).
