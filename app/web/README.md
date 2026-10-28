# RWA DEX Frontend

This folder contains the Vite + React + TypeScript trading interface for the Solana-based RWA DEX.

## Runtime

This project uses Bun instead of npm for package management and local tooling.

## Commands

```bash
bun install
bun run dev
bun run build
bun run preview
```

## Trying it against real state (localnet)

One command builds and deploys all eleven programs to a local validator and seeds
it with everything the pages read — two SPL mints, the KYC gate with an issued
attestation, the whitelist root, the settlement config and escrow, and all four
AMM pools with liquidity — then trades a swap through the app's own instruction
builders as a self-check:

```bash
bun run bootstrap:localnet                       # build + deploy + seed
bun run bootstrap:localnet -- --user <pubkey>    # fund/KYC a browser wallet too
bun run bootstrap:localnet -- --rails oracle,kyc # wire the KYC rail into the pools
bun run bootstrap:localnet -- --skip-deploy      # re-seed only
```

The deploy half is `../../scripts/deploy-localnet.sh`; the seed half is
`scripts/seed-localnet.ts`, which uses `src/core/*` (the same builders, PDAs and
decoders the browser uses) and is safe to re-run — existing accounts are skipped
and the mint keypairs are remembered in `../../.anchor/seed-localnet.json`.

Then `bun run dev`, connect the wallet it printed (localnet), and paste the two
mints it printed into the Orders / CLMM / DLMM / Maverick / TWAMM pages. The manifest with
every address it created is written to `../../.anchor/seed-localnet.manifest.json`.

Note: `anchor build` is invoked with `--arch v0`, because the SBPF v3 images it
produces by default are rejected by the Agave 3.1.x toolchain this repo pins
(`Failed to parse ELF file: invalid file header`). Override with `SBF_ARCH=v1`
only when deploying to a cluster whose loader needs it.

## Stack

- Vite
- React
- TypeScript
- Bun
- Solana wallet integration (Phantom / Backpack via injection)
- Institutional trading console for every program in the workspace

## Example playbooks

`examples/` contains step-by-step UI playbooks: six asset-class walkthroughs (a
NAV-bearing fund share, a carbon credit, a treasury bill, a private-credit
receivable, tokenized gold, and a two-class fund with a lifecycle runbook) plus
two operational runbooks — one desk clearing several assets under different
per-mint Transfer Agents, and the oracle circuit breaker from arming to incident
response. Each quotes the exact page, panel and button labels, with units,
verification checklists and failure modes.
Start at [`examples/README.md`](examples/README.md).

## Pages

- **Overview** — protocol map, live infrastructure status (settlement config, KYC mint, hook authority), your T+N escrows
- **Orders** — resting limit orders (ask/bid) with isolated escrow and expiry, partial or full fills with a slippage bound and caller-named settlement ids, and cancellation for a refund
- **CLMM** — create pools, add/remove concentrated liquidity over tick ranges, collect fees, swap with an explicit tick walk, trip the circuit breaker
- **DLMM** — create pools, initialize bin arrays, per-bin liquidity, swap across bin arrays, pause/rotate authority
- **Maverick** — bin kinds (static/right/left/both), permissionless `move_bins` keeper, swap, remove along the move chain
- **TWAMM** — LP shares, long-term orders (create / crank / cancel / withdraw), virtual-order execution
- **Settlement** — clearinghouse config, global and per-mint transfer-agent keys, escrow initiate / settle (secp256k1) / cancel, residual close
- **KYC & Whitelist** — soulbound attestations (issue/revoke/rotate), transfer-hook Merkle root governance, hook-authority funding
- **RWA Assets** — create Token-2022 RWA mints (hook + metadata + transfer fee), mint, transfer with dual Merkle proofs, fee updates

### Orders page (limit order book)

The Orders page is the client for `programs/order_book`:

- **Market**: the book is keyed by the base/quote pair, so the pair and its order
  matter (`orderBook(base, quote)`). Only the program's upgrade authority can
  create a book; the initialize form appears when the pair has none.
- **Place**: choose ask/bid, a human quote-per-base price (converted with
  `toRawPrice`, i.e. `human × 10^(quote_decimals − base_decimals) × 1e9`), the
  amount **in the escrow token**, an expiry in minutes and an order id. The id
  is part of the order PDA `(book, wallet, order_id)`, so the field is seeded
  from the clock and advanced after each placement; reusing one collides with
  the existing order.
- **Fill**: select a resting order. The amount is the escrow leg (base for an
  ask, quote for a bid) and "Max pay" is the counter-leg slippage bound. The
  page resolves both mints' token programs, creates any missing taker/proceeds
  ATAs in the same transaction, and appends each mint's transfer-hook accounts.
  On a settlement-routed book it names a fresh random `trade_id`
  (`randomTradeId()`), because ids live in a cluster-wide settlement namespace
  shared with the AMM swaps.
- **Cancel**: shown for your own orders only. It refunds the escrow's whole
  balance (the unfilled amount plus anything donated to the escrow) and returns
  the order + escrow rent. It stays available while the book is paused and after
  expiry.

## Client architecture

The app does not use generated IDL clients; it encodes Anchor instructions directly:

- `src/core/borsh.ts` — minimal borsh writer + Anchor discriminators (sha256 of `global:<name>`, cached)
- `src/core/pdas.ts` — every PDA derivation, mirroring the on-chain seeds exactly
- `src/core/instructions-*.ts` — one builder per program (`clmm`, `dlmm`, `maverick`, `twamm`, `order_book`, `rails`); account order matches each `#[derive(Accounts)]` struct; optional rails are left off with the program-ID sentinel
- `src/core/decoders.ts` — field-exact decoders for pool, position, oracle, settlement, order, bin-array and token accounts
- `src/core/walks.ts` — derives the TWAMM order-list walks (insertion, expired crank, cancel prefix) and the Maverick move-chain hop arrays from on-chain links, so neither list is typed by hand
- `src/core/merkle.ts` — RFC-6962 Keccak Merkle tree shared by the whitelist proof builder and the RWA transfer form, which checks its derived root against the transfer-hook config. The proofs are what the form registers on chain (`set_whitelist_entry`), since the transfer itself carries only the two resulting entry PDAs
- `src/ui/` — wallet/cluster context, toast transaction feedback, one page per protocol

Cluster selection (localnet by default, matching `Anchor.toml`) persists in `localStorage`.
