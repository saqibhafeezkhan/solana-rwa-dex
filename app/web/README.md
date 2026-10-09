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

One command builds and deploys all ten programs to a local validator and seeds
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
mints it printed into the CLMM / DLMM / Maverick / TWAMM pages. The manifest with
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

## Pages

- **Overview** — protocol map, live infrastructure status (settlement config, KYC mint, hook authority), your T+N escrows
- **CLMM** — create pools, add/remove concentrated liquidity over tick ranges, collect fees, swap with an explicit tick walk, trip the circuit breaker
- **DLMM** — create pools, initialize bin arrays, per-bin liquidity, swap across bin arrays, pause/rotate authority
- **Maverick** — bin kinds (static/right/left/both), permissionless `move_bins` keeper, swap, remove along the move chain
- **TWAMM** — LP shares, long-term orders (create / crank / cancel / withdraw), virtual-order execution
- **Settlement** — clearinghouse config, transfer-agent key rotation, escrow initiate / settle (secp256k1) / cancel
- **KYC & Whitelist** — soulbound attestations (issue/revoke/rotate), transfer-hook Merkle root governance, hook-authority funding
- **RWA Assets** — create Token-2022 RWA mints (hook + metadata + transfer fee), mint, transfer with dual Merkle proofs, fee updates

## Client architecture

The app does not use generated IDL clients; it encodes Anchor instructions directly:

- `src/core/borsh.ts` — minimal borsh writer + Anchor discriminators (sha256 of `global:<name>`, cached)
- `src/core/pdas.ts` — every PDA derivation, mirroring the on-chain seeds exactly
- `src/core/instructions-*.ts` — one builder per program; account order matches each `#[derive(Accounts)]` struct; optional rails are left off with the program-ID sentinel
- `src/core/decoders.ts` — field-exact decoders for pool, position, oracle, settlement and token accounts
- `src/ui/` — wallet/cluster context, toast transaction feedback, one page per protocol

Cluster selection (localnet by default, matching `Anchor.toml`) persists in `localStorage`.
