# Quick start: your first Pipeline strategy

This takes you from an empty folder to a strategy that is registered, has a market, and trades through Pipeline. It covers your machine first, then devnet once Pipeline is deployed there (see `docs/DEVNET.md` for status).

You need:
- Rust;
- `cargo-build-sbf` 4.4 or later, which builds sBPF v3 programs (`cargo install cargo-build-sbf --version 4.6.0 --locked`), and the Solana CLI 4.4;
- Node 20.19 or later, for the TypeScript SDK and scripts.

## 1. Build and test everything

```sh
git clone <this repository> && cd <it>
(cd programs && cargo build-sbf)
cargo test --workspace
```

You should see every test pass, including:
- **router tests** — swaps, fees, pause, sweeps, two-hop routes;
- **Token-2022 tests** — transfer fees and transfer hooks;
- **adversarial tests** — a hostile strategy can't harm traders or fees;
- **quoter tests** — quotes match execution exactly.

## 2. Copy the reference strategy

`programs/test-strategy-cp` is a complete constant-product strategy in Anchor. To make your own:

1. Copy it into `programs/my-strategy`, rename the package, and add it to `programs/Cargo.toml`'s members.
2. Generate a program key and put its address in `declare_id!`:

   ```sh
   solana-keygen new --outfile ~/my-strategy-keypair.json
   ```

3. Change the pricing in `Pool::quote`. Keep the rest of the structure:
   - `pipeline_swap` credits input with `credited_input`, calls the same `quote`, then settles. That keeps quotes and swaps identical.
   - Read `docs/STRATEGY-GUIDE.md` sections 2 and 4 before changing anything else.
4. Build:

   ```sh
   (cd programs && cargo build-sbf)
   ```

## 3. Test it against the real router

Copy `crates/pipeline-tests/tests/router.rs` to a new file and point its helpers at your program. `tests/common/mod.rs` shows how to:
- load the router and your program into LiteSVM, and set your upgrade authority;
- register your strategy, create a market and fee accounts, and add liquidity;
- swap through the router and decode the `SwapEvent`.

Also copy the checks in `tests/quoter.rs`, which prove your quotes match execution. Run them with:

```sh
cargo test -p pipeline-tests --test <your file>
```

## 4. Rehearse on a local validator

`docs/DEVNET.md` ("Running Pipeline on a local validator") starts `solana-test-validator` with Pipeline loaded and runs `scripts/devnet/smoke.ts`. To rehearse your own strategy, add `--upgradeable-program <your id> <your .so> <your key>` to the validator, and adapt the market-creation and deposit steps in the script.

## 5. On devnet

Pipeline is live on devnet ([`DEVNET.md`](DEVNET.md)):

1. **Deploy your program:**

   ```sh
   solana program deploy programs/target/deploy/my_strategy.so --program-id ~/my-strategy-keypair.json --url devnet
   ```

2. **Register it** with the router's `register_strategy`, signed by your upgrade authority. This pays the 0.01 devnet-SOL listing fee. In TypeScript: `registerStrategy` from the SDK.
3. **Publish a metadata JSON** (`docs/STRATEGY-GUIDE.md` section 7) and point your registration at it with `update_metadata`.
4. **Create a market** with `create_market` plus `init_fee_account` for each mint, then add liquidity with your own custom action.
5. **Trade it:** `resolveAccounts`, `quoteStrategy`, then `swap`, all in the SDK. Anything built on Pipeline can now see and trade your market.

## Where to read next

- `docs/STRATEGY-GUIDE.md`: the interface, the rules and Solana's limits for strategies.
- `docs/INTEGRATOR-GUIDE.md`: if you're building a desk, terminal, bot or chart on top.
- `docs/ROUTER.md`: how the router works.
