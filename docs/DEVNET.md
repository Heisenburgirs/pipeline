# Pipeline on devnet

Pipeline's router is live on Solana devnet, together with an example strategy (a
constant-product pool) that anyone can create markets on.

| | Address |
|---|---|
| Router | [`PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34`](https://orbmarkets.io/address/PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34/history?cluster=devnet) |
| Example strategy | [`yRptCdde1du1JYR2DcNFtLC1fkp5Asd6rE3XSvZoUFx`](https://orbmarkets.io/address/yRptCdde1du1JYR2DcNFtLC1fkp5Asd6rE3XSvZoUFx/history?cluster=devnet) |
| Listing fee | 0.01 devnet SOL, once per strategy program |
| Swap fee | 0.25% of the input, on every swap through the router |

Devnet programs can be upgraded or reset without notice while Pipeline is pre-release.

## 1. Make your first swap (5 minutes)

You need the Solana CLI, Node 20.19 or later, and a devnet wallet with a little SOL (the
walkthrough spends about 0.02).

```sh
# A wallet and some devnet SOL (or use https://faucet.solana.com)
solana-keygen new                      # skip if ~/.config/solana/id.json exists
solana airdrop 1 --url devnet

# Build the SDK and run the end-to-end script
(cd sdk/ts && npm install && npm run build)
cd scripts/devnet && npm install
RPC_URL=https://api.devnet.solana.com node smoke.ts
```

The script uses `~/.config/solana/id.json` (set `KEYPAIR` to use another) and, through
the real router on devnet:

1. creates two test tokens and mints them to you;
2. creates a market for them on the example strategy and adds liquidity;
3. asks the strategy which accounts a trade needs and quotes a swap by simulation;
4. swaps through the router;
5. decodes the router's `SwapEvent` and checks that the output equals the quote and the
   fee is exactly 0.25%, rounded up.

It prints the swap's signature, so you can open it in an explorer. The script is short and
uses only the TypeScript SDK; it is the quickest way to see every call in context.

## 2. Build your own strategy

A strategy is any Solana program that implements Pipeline's five strategy instructions.
[`QUICKSTART.md`](QUICKSTART.md) walks through copying the example strategy, testing it
against the real router in LiteSVM, and putting it on devnet:

1. **Deploy** your program to devnet as usual (`solana program deploy`).
2. **Register** it with the router's `register_strategy`, signed by the program's upgrade
   authority. This pays the 0.01 SOL listing fee. TypeScript: `registerStrategy`.
3. **Publish metadata** (name, image, links, custom actions; see
   [`STRATEGY-GUIDE.md`](STRATEGY-GUIDE.md) section 7) and point your registration at it
   with `update_metadata`.
4. **Create a market** with the router's `create_market`, plus `init_fee_account` for each
   of its mints, then add liquidity with your strategy's own instruction.

From then on, anything built on Pipeline can find, quote and trade your market.

## 3. Build on top

[`INTEGRATOR-GUIDE.md`](INTEGRATOR-GUIDE.md) covers discovering every strategy and market
(`getProgramAccounts` on the router), quoting (embedded VM, or RPC simulation), building
swaps and multi-hop routes, and indexing trades from `SwapEvent`.

## Running Pipeline on a local validator

To test without devnet, load the router and example strategy into a local validator:

```sh
# Toolchain: cargo-build-sbf 4.6 (sBPF v3) and the Agave 4.4 CLI
cargo install cargo-build-sbf --version 4.6.0 --locked
sh -c "$(curl -sSfL https://release.anza.xyz/v4.4.1/install)"

(cd programs && cargo build-sbf)
KEY=~/.config/solana/id.json
ME=$(solana-keygen pubkey $KEY)
solana-test-validator --reset \
  --deactivate-feature 6TkHkRmP7JZy1fdM6fg5uXn76wChQBWGokHBJzrLB3mj \
  --upgradeable-program PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34 programs/target/deploy/pipeline.so $ME \
  --upgradeable-program yRptCdde1du1JYR2DcNFtLC1fkp5Asd6rE3XSvZoUFx programs/target/deploy/test_strategy_cp.so $ME
```

In another terminal:

```sh
solana -u localhost airdrop 100
cd scripts/devnet && KEYPAIR=~/.config/solana/id.json node smoke.ts
```

Because your wallet is the programs' upgrade authority here, the script first initialises
the router's config and registers the example strategy, then runs the same steps as on
devnet. The `--deactivate-feature` flag switches off the deeper CPI limit (SIMD-0268),
which test validators enable by default but devnet and mainnet don't have yet.

To try your own strategy, add `--upgradeable-program <id> <your .so> $ME` and adapt the
market-creation and deposit steps in `smoke.ts`.
