# PropAMM Forge

Build and run your own proprietary AMM on Solana — a private vault holding your
own capital, priced by your own off-chain engine, quotable by aggregators.

Proprietary market makers take the majority of Solana DEX aggregator volume, but
none of them publish an IDL or an SDK. Every new desk has rebuilt the program,
the pricing engine and the aggregator integration from scratch. This is that
work, packaged: a deployable vault program, a quoting SDK, a CLI that gets you
on chain in five commands, and — as the remaining milestones land — a pricing
engine, a router adapter and a monitoring console.

**Status: milestone M1 — the on-chain core.** A vault deploys, takes capital
from its owner only, quotes parametrically and executes swaps against that
quote. Everything past that is listed under [Not here yet](#not-here-yet).

---

## Quick start

Requires the Solana CLI (Agave 3.1.10) and a keypair with devnet SOL.

```sh
forge init . --pair <BASE_MINT>/<QUOTE_MINT> --cluster devnet
forge deploy                            # create the vault on chain
forge fund --side base  --amount 100    # one call per side of the pair
forge fund --side quote --amount 15000
forge quote --mid 150 --spread-bps 10 --size 10
forge status                            # addresses, balances, quote age
```

`forge init` writes a project README of its own describing exactly what was
created and what the next command does.

Swapping is not a `forge` command by design: the counterparty is a third party,
not the desk that owns the capital. Build the swap instruction with the TypeScript
SDK in `packages/sdk`, or let an aggregator build it.

## How it is put together

**One program, many vaults.** The bytecode on chain is shared. "Your own AMM" is
your own *vault* — a PDA derived from your owner address and the asset pair — not
your own copy of the program. You do not build, deploy or pay rent for bytecode;
that is what makes a five-command path possible at all.

**Capital belongs to the owner alone.** No outside deposits, no LP shares, no
claim on profit by anyone else. Deposits from other accounts are rejected by the
program, not by convention.

**No curve, and no oracle read on chain.** Price is not discovered by `x*y=k`.
The vault stores a mid, a half-spread, an inventory skew and a maximum order
size; the swap computes the output from those. The program never reads a price
feed — an off-chain signer posts the quote. That is the whole reason a swap fits
in a router's compute budget.

**Quotes expire in slots.** Every quote carries the slot it was posted in, and
the program refuses any swap against a quote older than the vault's freshness
limit. Posting by hand is for testing; keeping a quote alive is the engine's job.

**One pair per vault.** A second pair is a second vault with its own capital and
its own risk limits. The pair direction is part of the vault address, so the
same two mints the other way round is a different vault.

**Assets whose transfers change the amount are refused at deploy time.** A mint
carrying a transfer hook, a transfer fee, a permanent delegate, a pausable or
non-transferable extension, or a default-frozen state is rejected when the vault
is created — with a message naming the extension — rather than silently making
quoted and executed amounts disagree.

## What is measured

Numbers below come from the test suite and the benchmark, not from estimates.
Each is a success criterion in `docs/SPEC.md`.

| | Budget | Measured |
|---|---|---|
| Empty directory to first swap | ≤ 15 min, ≤ 5 commands | 5 commands, 2.43 s |
| Swap instruction cost | ≤ 60,000 CU | 22,950 CU worst pair, 18,647 CU plain SPL |
| Swaps against a stale quote | 0 of 1,000 (≥ 200 deliberately stale) | 0 of 1,000 |
| Inventory past its hard bound | 0 of 10,000 | 0 over 695 swaps so far |

`update_quote` costs 6,028 CU and deliberately has no declared ceiling: no budget
for it is claimed anywhere, and inventing one would be a number off a shelf.

## Layout

| Path | What lives there |
|---|---|
| `programs/propamm-vault` | the Anchor program: vault state, quoting, swap, guards |
| `crates/propamm-quote` | the quote math, shared by the program and every consumer |
| `crates/propamm-cli` | `forge` — init, deploy, fund, quote, status |
| `packages/sdk` | TypeScript SDK: vendored IDL, instruction builders, event decoding |
| `apps/web` | screen prototype of the console and the deployment wizard |
| `tests/program` | instruction-level tests on Mollusk, plus the CU budget gate |
| `tests/e2e` | end-to-end run against a local validator |

`apps/web` runs on **mock data**. Every figure on those screens is drawn, not
observed: there is no collector, no database and no chain behind them yet.

## Building

The program must be built with `cargo-build-sbf`, not `anchor build`. On this
toolchain `anchor build` emits SBPFv3, which Agave 3.1.10 will not load — it is
kept only to regenerate the IDL. `scripts/wsl-build.sh` wraps both, along with
the test, benchmark and end-to-end targets.

```sh
scripts/wsl-build.sh build-sbf   # the artifact that goes on chain
scripts/wsl-build.sh test        # 227 tests
scripts/wsl-build.sh bench-cu    # compute-unit report with deltas
pnpm gate                        # IDL check, lint, typecheck, SDK tests
```

Program ID: `77Y9n3vWE2noN1u9PTshuWxdDRsrw9UMtejBypUD9wjq`.

## Not here yet

Named plainly, because a demo without this list creates a false impression of
what has been proven:

- **No pricing engine.** Quotes are posted by hand through `forge quote`. Feed
  reading, the hybrid refresh rule and inventory-aware spreads are milestone M2.
- **No aggregator adapter and no router.** Integration and the local router twin
  are M3. Nothing here has been listed by a production router.
- **No monitoring console on real data.** Event collection, P&L accounting and
  the CU history are M4; the screens in `apps/web` are mocks until then.
- **No risk loop.** Emergency halt, daily-loss limits and automatic withdrawal
  of quotes are M5.
- **No mainnet and no real capital.** Work runs on a local validator and devnet
  with test tokens. Nothing here says how the vault behaves under real order
  flow or real MEV.
- **No trading strategy.** A template engine with a spread and an inventory skew
  is supplied. Alpha research, calibration and any claim of profitability are
  the desk's own work.
