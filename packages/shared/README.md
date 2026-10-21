# `@rwa-dex/shared` (planned)

> **Status: planned.** No `package.json` or source here yet. This describes what
> should move in, and the rules that keep it from becoming a second copy of the
> app.

## Why

The same client logic exists twice today:

| Concern | `app/web/src/core` | `tests/ts` |
| :--- | :--- | :--- |
| Program ids, PDA seeds | `pdas.ts` | re-derived per test |
| Account layouts | `decoders.ts` | hand-read offsets in `rwa-dex.ts` |
| Instruction data | `instructions-orderbook.ts` | raw 8-byte discriminators |
| Whitelist tree | `merkle.ts` | **imported from the app** |
| Instruction builders | `instructions-{clmm,dlmm,maverick,twamm,orderbook,rails}.ts` | raw discriminators per test |
| Amount/price display | inline in `ui/` components | duplicated in assertions |

The tests already import `app/web/src/core/merkle.ts` directly, which is the
clearest sign the split is wrong: the verification layer depends on the
application layer. A shared, framework-free package fixes the direction — both
consume it, neither owns it.

## Contents

```
packages/shared/
├── src/
│   ├── programs.ts      # ids + cluster config (devnet/localnet)
│   ├── pdas.ts          # every seed derivation, one function per PDA
│   ├── coders.ts        # discriminators, Borsh/zero-copy layouts
│   ├── instructions/    # moved from app/web/src/core/instructions-*.ts
│   ├── amounts.ts       # base-unit math, price/amount rounding, decimals (new)
│   ├── merkle.ts        # whitelist tree + proofs (moved from app/web)
│   └── types.ts         # decoded account + instruction argument types (new)
└── package.json         # ESM, types, no runtime deps beyond @solana/kit
```

Design rules:

* **No React, no DOM, no Node-only APIs.** It runs in the browser, in `bun test`,
  and in the mocha suite.
* **Coders are hand-written and tested against fixtures**, not generated at
  import time. A generated IDL client is a fine *additional* consumer, but the
  decoders that read historical accounts must not change shape when a program is
  redeployed.
* **One PDA function per address.** `bookPda(base, quote)` and friends return
  `{ address, bump }`; callers never inline seeds. Seeds that appear in more than
  one place are the classic source of "works in the UI, fails on chain".
* **Amounts are `bigint`, never `number`.** Formatting and parsing are separate
  from arithmetic; the UI owns display decisions.
* **No network calls.** The package builds instructions; submitting them is the
  caller's job. That keeps it usable from the seeder, the tests and the app.

## Consumers and migration

1. Create the package and move `merkle.ts` first (lowest risk, already shared).
2. Move `pdas.ts` and `coders.ts`; make `app/web/src/core` re-export from the
   package so existing imports keep working.
3. Move the instruction builders (`clmm`, `dlmm`, `maverick`, `twamm`,
   `order_book`, `rails`), starting with `order_book` — newest, and least
   entangled with UI state.
4. Point `tests/ts` at the package and delete its hand-rolled offsets. This is
   the step that actually proves the extraction: if the tests still need local
   decoding, the package is incomplete.
5. Delete the re-export shims once nothing imports the old paths.

Each step is independently shippable; nothing here requires a program change.

## Testing

* Unit tests in the package itself (`bun test`) for every coder and PDA against
  fixed vectors, including a golden account blob captured from a localnet run.
* The existing web tests stay where they are until step 2; they should end up
  thin wrappers over package tests.
* Cross-check against `target/idl`: a CI script that asserts every instruction
  name in the package exists in the IDL, and every account layout's size matches
  the IDL's declared size.

## Non-goals

Not a UI kit, not a state manager, not an RPC client wrapper, and not a place for
protocol policy. If a function needs `fetch`, a wallet, or a React hook, it
belongs in the app.
