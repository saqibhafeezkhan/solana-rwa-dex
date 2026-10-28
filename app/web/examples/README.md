# Example playbooks

End-to-end walks through the RWADEX interface, one real-world asset class per file.
Every playbook uses only controls that exist in the app today — the page name, the
panel title and the exact button label are quoted, so you can follow along with the
UI open next to the document.

These are operational recipes, not API docs. If a step is missing from the UI, the
playbook says so instead of inventing a workaround.

## The playbooks

| # | File | Asset | Primary venues |
|---|------|-------|----------------|
| 1 | [01-tokenized-fund-nav.md](01-tokenized-fund-nav.md) | Tokenized money-market fund share with a published NAV | RWA, KYC, DLMM, Orders, Settlement, Audit |
| 2 | [02-carbon-credit.md](02-carbon-credit.md) | Verified carbon removal credit (1 token = 1 tCO2e) | RWA, KYC, DLMM, Orders, Settlement, Audit |
| 3 | [03-treasury-bill-twamm.md](03-treasury-bill-twamm.md) | Short-dated treasury bill placed on a schedule | RWA, TWAMM, CLMM, Settlement, Audit |
| 4 | [04-private-credit-orderbook.md](04-private-credit-orderbook.md) | Trade-finance receivable traded at a discount | RWA, Orders, Settlement, Enterprise |
| 5 | [05-tokenized-gold-maverick.md](05-tokenized-gold-maverick.md) | Allocated gold warehouse receipt with a keeper | RWA, KYC, Maverick, Orders, Settlement |
| 6 | [06-fund-share-classes-lifecycle.md](06-fund-share-classes-lifecycle.md) | Two share classes plus the full fund lifecycle runbook | RWA, KYC, DLMM, Settlement, Enterprise, Audit |
| 7 | [07-per-mint-transfer-agents.md](07-per-mint-transfer-agents.md) | Two instruments cleared by two different Transfer Agents on one clearinghouse | RWA, KYC, DLMM, Settlement, Audit |
| 8 | [08-oracle-breaker-runbook.md](08-oracle-breaker-runbook.md) | Risk desk: arming the deviation breaker, tripping it and restoring the pool | DLMM, CLMM, Maverick, KYC |

Files 1–6 are asset-class walkthroughs; 7 and 8 are **operational runbooks** for
surfaces that apply across assets — per-mint settlement keys, and the oracle
circuit breaker. They assume the mint and whitelist steps from [01](01-tokenized-fund-nav.md)
rather than repeating them.

Read [01](01-tokenized-fund-nav.md) first. It is the reference walkthrough: the
whitelist → mint → pool → order → settlement chain is identical in all the others,
and the later files only call out what changes.

## Before you start

### Runtime

This frontend uses Bun.

```bash
bun install
bun run dev
```

### Local network with everything pre-seeded

One command builds and deploys all eleven programs, seeds two SPL mints, the KYC
gate with an issued attestation, the whitelist root, the settlement config and
escrow, and all four AMM pools:

```bash
bun run bootstrap:localnet -- --user <YOUR_WALLET_PUBKEY>
```

That also funds and KYC-approves the browser wallet you pass. It prints the two
seeded mints; their addresses are also in `../../.anchor/seed-localnet.manifest.json`.
They are legacy SPL Token mints, so use them on the `Orders` page or as the quote in
[04](04-private-credit-orderbook.md) — not as the quote of a Token-2022 pool (see
*Accounts you need*).

### Top bar checklist

Do these before the first step of any playbook:

1. **Cluster** — pick the cluster in the top-right select. It defaults to
   `localnet`, matching `Anchor.toml`. The choice persists in `localStorage`.
2. **Connect wallet** — the button on the top right. Without it every panel's
   action button stays disabled. The top bar then shows the shortened address and
   the `KYC: none` / `Accredited: Verified` badge.
3. **Sample Data** — turn this **off**. While it is on, the pool pages show demo
   mints and the Orders page disables sending with
   `Sample data — sending is off`.
4. **Direct / Multisig** — the toggle next to Sample Data. `Direct` signs and
   broadcasts from the connected wallet. `Multisig` captures each action as a
   proposal instead (see [06](06-fund-share-classes-lifecycle.md)).
5. **Role** — `Trader`, `Treasury Manager`, `Compliance Officer` or `Admin`. The
   role is a local UI gate for the enterprise surfaces and the proposal queue; it
   does **not** change on-chain authority. Set `Admin` for a first walkthrough.

One-time initializers are deployer-only: **Initialize hook**, **Initialize KYC
mint**, **Initialize config** (settlement) and creating an order book all check the
program's upgrade authority. If you are not the deployer, ask whoever deployed the
programs to run them once; every later action uses the authorities those calls
recorded.

### Accounts you need

A playbook that moves value needs at least three wallets:

| Wallet | Purpose |
|--------|---------|
| Issuer / fund admin | Creates the mint, publishes the whitelist root, holds mint + fee + hook authority |
| Investor (counterparty) | Receives the token; must be whitelisted for RWA transfers |
| Transfer agent / registry | Signs settlement releases (secp256k1 key, not a Solana wallet) and owns the retirement or custody account |

You also need a **quote asset**. Every AMM instruction carries a single
`token_program` account for both mints, and RWA mints are Token-2022, so the quote
leg of a pool must be Token-2022 as well. The two `mUSDC` / `mTSLA` mints the
bootstrap seeds are legacy SPL Token, so they cannot be pooled with an RWA mint
(they are fine on the `Orders` page, which resolves a token program per mint).

Create the quote mint the same way as any other RWA mint — `RWA` → **Create RWA
mint**, 6 decimals, e.g. `Test USD` / `TUSD` — press **Register hook metas for this
mint** for it, and include its holders in the same whitelist. The playbooks call
this mint `TUSD`; [04](04-private-credit-orderbook.md) deliberately uses the seeded
legacy `mUSDC` instead, because its book only needs one program per leg.

The issuer needs SOL for rent and fees. On localnet,
`bun run bootstrap:localnet -- --user <pubkey>` airdrops to the wallet it is given.

## Shared concepts

### The RWA mint stack

`RWA` → **Create RWA mint** creates a Token-2022 mint in one instruction with four
extensions plus embedded metadata:

```
TransferHook  -> whitelist enforcement program (baked in at creation)
MetadataPointer -> points at the mint itself
TransferFeeConfig -> 0 bps at birth; the issuing wallet can raise it later
PermanentDelegate -> the connected wallet, for regulatory seizure
TokenMetadata -> name / symbol / URI
```

Two details that matter for every playbook:

- The **URI** field is where the off-chain document lives: ISIN, NAV attestation,
  registry serial, prospectus. It is written at creation and the UI has no
  "update metadata" control, so point it at a **stable URL that serves the current
  document**, not at a dated file.
- After `Create mint` the new address is only shown as the placeholder of
  **Mint (defaults to the freshly created one)**. The field itself is empty, and
  browsers do not let you select placeholder text, so read the address with the
  recipe below before leaving the page. It is component state: navigating away or
  reloading loses the reference even though the mint is on chain forever.

### Reading the new mint's address

The mint's keypair is generated in the browser and never exported, so the address
must be recovered from the creation transaction. Take the signature from the
success toast (or from `Audit` → **On-chain audit trail**) and ask the RPC for the
transaction's signers:

```bash
curl -s http://127.0.0.1:8899 -X POST -H 'content-type: application/json' -d '{
  "jsonrpc": "2.0", "id": 1, "method": "getTransaction",
  "params": ["<SIGNATURE>", { "encoding": "json", "maxSupportedTransactionVersion": 0 }]
}' | jq -r '.result.transaction.message.accountKeys[] | select(.signer) | .pubkey'
```

Two keys come back: your wallet (the fee payer) and the new mint. Swap the RPC URL
for whichever cluster the top bar shows.

Two shortcuts avoid the step entirely:

- Finish everything that only uses the address on the `RWA` page first (hook metas,
  fee, mint, transfer). The page keeps using the fresh mint until you leave it.
- `bun run bootstrap:localnet` already creates, whitelists and mints a Token-2022
  RWA mint ("Tokenized Treasury Note", `mTBILL`) and prints its address; it is also
  in `../../.anchor/seed-localnet.manifest.json`. Use that address if you only want
  to exercise the pool, book and settlement steps, and read the "create the mint"
  sections as the procedure for your own asset.

### The whitelist is a Merkle tree, and entries are the on-chain copy

`KYC` → **Transfer-hook whitelist** manages compliance:

- Leaves are `keccak(0x00 ‖ wallet)`.
- Nodes are `keccak(0x01 ‖ min ‖ max)` over sorted pairs (RFC-6962 style).
- Publishing a root does **not** move existing entries. Every entry whose path no
  longer folds to the new root stops verifying, so **re-register every institution
  after a root update**.
- A transfer needs *both* wallets' entry accounts, because Token-2022 derives them
  from the two token accounts. A missing entry fails before the hook is called.

The practical order is: import the wallet list on `Enterprise` → paste the same
list into the whitelist field on `KYC` → `Update root` → build proofs → register
entries → then transfer.

### Who needs a whitelist entry (including program PDAs)

The hook checks the owner of **both** token accounts on every transfer. Owners are
not only people: the vaults of a pool are owned by the pool PDA, and an order's
escrow is owned by the order PDA. Those accounts have to be whitelisted too, or the
venue fails on the first transfer — Token-2022 cannot resolve their entry account
at all.

| Owner | Where to read it | Needed when |
|-------|------------------|-------------|
| Institution wallet | your wallet list | always |
| **Pool PDA** | **Derived pool PDA** on `CLMM` / `DLMM` / `Maverick` / `TWAMM` (type the pair, before creating the pool) | the pool holds or moves a hooked mint |
| **Order PDA** | `bun examples/pdas.ts --pair … --maker … --order-id …` | the order escrows a hooked mint (every ask, and any bid escrowed in a Token-2022 quote) |
| **Hook authority PDA** | `KYC` → **Hooks escrow router**, or `bun examples/pdas.ts` | settlement-routed output parks in its custody ATA on the way to escrow |
| **Settlement escrow authority PDA** | `bun examples/pdas.ts` | a hooked mint is released from, or cancelled out of, escrow |

Registering a PDA is the same as registering a wallet: add it to the whitelist list
**before** `Update root`, then use **Merkle proof builder** with the PDA as the
recipient, **Use for recipient entry**, and **Register entry**. The hook config's
authority wallet signs the entry; the PDA itself never does.

Two practical consequences:

- **Order ids need to be known in advance.** The Orders form seeds **Order id**
  from the clock and re-seeds it after each placement, but you can overwrite it
  with a fixed number. Pick one id per order, derive its PDA, whitelist it, *then*
  place the order with that id.
- **One tree covers every hooked mint.** All mints created on `RWA` point at the
  same hook program, so a wallet's entry works for the fund share and the `TUSD`
  quote alike. Adding a custody account means re-publishing the root and
  re-registering **every** existing entry.

```bash
# fixed accounts
bun examples/pdas.ts

# plus the book and the order you are about to place
bun examples/pdas.ts --pair <BASE> <QUOTE> --maker <YOUR_WALLET> \
  --order-id 1700000000000 --escrow-mint <BASE>
```

Run it from the `web/` directory. It is pure PDA derivation — no RPC, no keys.

### KYC attestation vs. whitelist

They are separate gates and pools can require either:

| Gate | Where | What it proves |
|------|-------|----------------|
| KYC attestation | `KYC` → **KYC gate** | The wallet holds a soulbound attestation from the KYC mint |
| Transfer-hook whitelist | `KYC` → **Transfer-hook whitelist** | Both sides of an RWA transfer are in the published Merkle tree |

AMM pools and order books created with `Require KYC = On` / `KYC rail =
Require attestation` check the first. RWA token transfers always check the second.

### Amounts are raw base units

Every RWA field labelled **Amount** is raw base units, not whole tokens. Convert
before typing:

| Decimals | Whole tokens | Type this |
|----------|--------------|-----------|
| 6 | 1 | `1000000` |
| 6 | 1,000 | `1000000000` |
| 6 | 1,000,000 | `1000000000000` |
| 0 | 1 | `1` |
| 2 | 1 | `100` |

The AMM pages use **raw** amounts too (`Amount in (raw)` on CLMM, `Amount in` on
DLMM / Maverick / TWAMM), while the Orders page **Price** field is a human
quote-per-base number that the page scales by
`10^(quote_decimals − base_decimals) × 1e9`. Mixing the two conventions is the
most common source of "amount too small" and "max pay too low" errors.

### T+N settlement

`Settlement` is a slot-based clearinghouse. Pools, books and TWAMM orders can
route proceeds into an escrow instead of paying out directly:

- Each escrow hangs off a cluster-wide `trade_id` — ids are shared with AMM swaps.
- Release requires **both** the settlement slot to pass **and** a valid transfer
  agent secp256k1 signature over `(trade_id ‖ taker ‖ mint ‖ amount)`, hashed with
  keccak256. The signature is `r ‖ s` (128 hex) plus a recovery id.
- On localnet a slot is roughly 400 ms, so `T+N slots = 100` is about 40 seconds.
  On mainnet 100 slots is well under a minute; size the slot count for the
  settlement window you actually want.

## How to read a step

Each step looks like this:

> **Step 3 — Publish the whitelist root**
> `KYC` → **Transfer-hook whitelist** → *Whitelist (comma/space-separated
> pubkeys)* → paste the wallets → **Update root**
> Expect: the toast `Hook update root`, and the next transfer's computed root
> equals the on-chain root.

"Expect" is what you should see before moving on. The last section of each file is
a verification checklist plus the failures that are specific to that asset.

## Cross-cutting troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| Action buttons stay greyed out | No wallet connected, or Sample Data is on | Connect the wallet, turn Sample Data off |
| `The imported whitelist does not reproduce the on-chain Merkle root` | The list on `RWA` (imported on `Enterprise`) is not the exact set that was published | Re-import the published set, or publish this root |
| `… whitelist entry is missing on chain` | Proofs match, entries were never registered or were invalidated by a root update | `Register both whitelist entries`, or register each entry on `KYC` |
| Transfer fails with an "unknown program" / account error | `Register hook metas for this mint` has not run for this mint | Press it once on `RWA` for that mint |
| `Transaction simulation failed: … custom program error` on a pool swap | KYC rail is on and the wallet has no attestation | `KYC` → **Issue KYC** from the authority, then `Verify my attestation on-chain` |
| Escrow creation fails on an AMM swap | The hook authority PDA is out of lamports | `KYC` → **Hooks escrow router** → **Fund hook authority** |
| Swap / deposit / fill fails on a hooked mint | The custody owner (pool PDA, order PDA, hook authority or escrow authority) has no whitelist entry | Add it to the list, `Update root`, re-register every entry |
| Pool creation fails with a token-program error | The two mints are under different token programs | Recreate the quote as a Token-2022 RWA mint, or use the order book |
| `No resting orders on this book` right after creating one | The Orders page needs **both** mints filled to derive the book PDA | Fill *Base mint* and *Quote mint*, or refresh |
| A newly created mint "disappeared" | The RWA page keeps the mint in component state | Recover the address from the creation signature (see above); paste it into *Mint (defaults to the freshly created one)* |
| Pool page shows the demo pair after reload | Sample Data is still on | Toggle it off |
| A pool stays halted after **Unpause pool** | Oracle breaker latched separately from the admin pause | Admin **Unpause pool** clears the pool flag; the oracle owner must also press **Unpause pool (oracle owner)** in the risk panel |

## What these playbooks do not cover

- **Burning / retiring tokens on chain.** The UI exposes mint, transfer and fee
  updates only. Retirement is modelled as a transfer to a registry-owned wallet in
  [02](02-carbon-credit.md). The permanent delegate does allow an authorised
  clawback with the delegate key outside this UI.
- **Updating metadata after creation.** The URI is immutable from the UI.
- **Multisig execution on chain.** `Multisig` mode queues proposals locally and
  broadcasts them from the approver's wallet; it is not a Squads vault. See
  [06](06-fund-share-classes-lifecycle.md) for the approval flow.
- **Roles as on-chain permissions.** RBAC here is a UI gate. On-chain authority is
  whatever key the program recorded (pool authority, hook authority, KYC
  authority, fee authority).
